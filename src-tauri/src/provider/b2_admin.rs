//! Backblaze's native API (`api.backblazeb2.com`), for the one thing the S3
//! API cannot tell us: which region an account lives in.
//!
//! The S3 plane already covers everything BucketCat browses. This module
//! exists solely because a B2 account's region is fixed at account creation
//! and is not discoverable from any S3 call -- you have to already know the
//! endpoint to make one. `b2_authorize_account` answers it from the credential
//! alone, and is the authoritative source: [`crate::provider::b2::
//! b2_region_from_key_id`] guesses the same thing offline from an
//! **undocumented** key-id convention, and this call is what corrects it.
//!
//! One request, one shape: a `GET` carrying HTTP Basic auth, so unlike
//! [`crate::provider::oss_admin`] there is no signing at all -- the whole
//! module is a thin JSON parser over [`crate::provider::https`]'s shared
//! client.
//!
//! ## Two things that will bite anyone editing this file
//!
//! 1. **Use `s3ApiUrl`, never `apiUrl` or `downloadUrl`.** All three come back
//!    together and all three are region-specific, but only `s3ApiUrl` is the
//!    S3-compatible endpoint. Verified live on 2026-07-30, the same account
//!    reports `apiUrl: https://api004.backblazeb2.com`,
//!    `downloadUrl: https://f004.backblazeb2.com` and
//!    `s3ApiUrl: https://s3.us-west-004.backblazeb2.com` -- pointing the SDK at
//!    either of the first two produces failures that look nothing like "wrong
//!    endpoint".
//!
//! 2. **A master application key cannot authenticate here for our purposes.**
//!    Backblaze states outright that it does not work with the S3-Compatible
//!    API, so even a master key that authorizes fine on this endpoint would
//!    hand back an endpoint the user's S3 credentials cannot use. The shape
//!    check ([`crate::provider::b2::looks_like_master_key_id`]) therefore runs
//!    *before* the request, turning a wasted round trip into an actionable
//!    error.
//!
//! ## Security
//!
//! `application_key` is a live credential. It is never logged, never included
//! in an error message, and never interpolated into a URL -- it only ever
//! rides inside the `Authorization` header, base64-encoded as Basic auth
//! requires (which is encoding, not protection; the transport is
//! HTTPS-only, enforced by `https::build_https_client`). Backblaze's own error
//! text is *not* surfaced either: its 401 body echoes nothing useful, and this
//! module maps status codes to [`AppError`] variants instead.

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use serde::Serialize;
use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::provider::b2::{looks_like_master_key_id, parse_s3_api_url};
use crate::provider::https;

/// Prefix for this module's `AppError::Internal` messages, so a log line says
/// which subsystem failed.
const CTX: &str = "b2 authorize";

/// Backblaze's native authorize endpoint.
///
/// v4 rather than v3: v4 reports `allowed.buckets` as an **array**, which is
/// what a multi-bucket application key actually has. v3's single
/// `bucketId`/`bucketName` pair cannot represent one, so it would silently
/// under-report which buckets a key can reach.
const AUTHORIZE_URL: &str = "https://api.backblazeb2.com/b2api/v4/b2_authorize_account";

/// What the connection form learns from one authorize call.
///
/// `region` and `endpoint` both come from `s3ApiUrl` via
/// [`parse_s3_api_url`], so they are consistent by construction and may name a
/// region this build's table has never heard of -- deliberately, see that
/// function.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct B2KeyProbe {
    /// Region id, e.g. `us-west-004`.
    pub region: String,
    /// `https://s3.{region}.backblazeb2.com`, ready to store as
    /// `Connection::endpoint`.
    pub endpoint: String,
    /// Buckets this key is restricted to, empty when it can reach all of them.
    ///
    /// Empty is the *unrestricted* case, not "no buckets" -- Backblaze reports
    /// `allowed.buckets: null` for a key with account-wide access (verified
    /// live on 2026-07-30). The form uses a non-empty list only to tell the
    /// user their key is scoped, which explains why the bucket list will look
    /// short later.
    pub allowed_buckets: Vec<String>,
    /// Whether the key may call `ListBuckets`. A key without it still works
    /// for object operations, so this drives a hint rather than an error.
    pub can_list_buckets: bool,
}

/// Pulls `apiInfo.storageApi` out of an authorize response.
fn storage_api(body: &Value) -> Option<&Value> {
    body.get("apiInfo")?.get("storageApi")
}

/// Reads `allowed.buckets[].name`, tolerating every shape Backblaze uses for
/// "unrestricted": the key absent, `null`, or an empty array.
fn parse_allowed_buckets(storage: &Value) -> Vec<String> {
    storage
        .get("allowed")
        .and_then(|a| a.get("buckets"))
        .and_then(Value::as_array)
        .map(|buckets| {
            buckets
                .iter()
                .filter_map(|b| b.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_capabilities(storage: &Value) -> Vec<String> {
    storage
        .get("allowed")
        .and_then(|a| a.get("capabilities"))
        .and_then(Value::as_array)
        .map(|caps| {
            caps.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Maps an authorize HTTP status onto an [`AppError`].
///
/// `401` is [`AppError::InvalidCredentials`] rather than
/// [`AppError::AccessDenied`]: this endpoint's only job is to check the
/// credential pair, so a refusal here means the pair is wrong, not that a
/// valid identity lacks a permission. Backblaze's own message is deliberately
/// dropped -- its 401 body adds nothing a user can act on, and not
/// interpolating a response into an error keeps this path incapable of echoing
/// the credential.
fn classify_status(status: u16) -> AppError {
    match status {
        401 => AppError::InvalidCredentials,
        403 => AppError::AccessDenied,
        _ => AppError::Internal {
            message: format!("{CTX}: request failed with status {status}"),
        },
    }
}

/// Authorizes a `(keyID, applicationKey)` pair and reports the account's S3
/// region and endpoint.
///
/// The credentials are checked for shape before any network call: a master
/// application key id is rejected up front with
/// [`AppError::InvalidCredentials`], because Backblaze does not accept master
/// keys on the S3-Compatible API at all, so an endpoint obtained with one
/// would be unusable anyway.
pub async fn authorize_account(key_id: &str, application_key: &str) -> AppResult<B2KeyProbe> {
    let key_id = key_id.trim();
    let application_key = application_key.trim();
    if key_id.is_empty() || application_key.is_empty() {
        return Err(AppError::InvalidCredentials);
    }
    // Cheaper and clearer than letting Backblaze answer: a master key may well
    // authorize successfully here, and we would then hand back an endpoint the
    // user's S3 requests cannot use, with nothing explaining why.
    if looks_like_master_key_id(key_id) {
        return Err(AppError::InvalidCredentials);
    }

    let credentials =
        base64::engine::general_purpose::STANDARD.encode(format!("{key_id}:{application_key}"));

    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(AUTHORIZE_URL)
        .header("Authorization", format!("Basic {credentials}"))
        .header("Accept", "application/json")
        .body(Empty::<Bytes>::new())
        .map_err(|e| AppError::Internal {
            message: format!("{CTX}: failed to build request: {e}"),
        })?;

    let client = https::build_https_client(CTX)?;
    let response = client
        .request(request)
        .await
        .map_err(|e| https::classify_transport_error(&e))?;

    let status = response.status().as_u16();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| AppError::Internal {
            message: format!("{CTX}: failed to read response body: {e}"),
        })?
        .to_bytes();

    if !(200..300).contains(&status) {
        return Err(classify_status(status));
    }

    let parsed: Value = serde_json::from_slice(&body).map_err(|e| AppError::Internal {
        message: format!("{CTX}: response was not valid JSON: {e}"),
    })?;

    let storage = storage_api(&parsed).ok_or_else(|| AppError::Internal {
        message: format!("{CTX}: response carried no apiInfo.storageApi"),
    })?;

    let s3_api_url = storage
        .get("s3ApiUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Internal {
            message: format!("{CTX}: response carried no s3ApiUrl"),
        })?;

    let (region, endpoint) = parse_s3_api_url(s3_api_url).ok_or_else(|| AppError::Internal {
        // The URL itself is safe to include: it is a public hostname derived
        // from the account's region and contains no part of the credential.
        message: format!("{CTX}: unrecognized s3ApiUrl `{s3_api_url}`"),
    })?;

    let capabilities = parse_capabilities(storage);
    Ok(B2KeyProbe {
        region,
        endpoint,
        allowed_buckets: parse_allowed_buckets(storage),
        can_list_buckets: capabilities.iter().any(|c| c == "listBuckets"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real 2026-07-30 response, with `authorizationToken` redacted and
    /// the capability list trimmed. Everything else is verbatim -- the point
    /// is that the parsers are pinned to Backblaze's actual shape, not to one
    /// invented here.
    fn real_response() -> Value {
        serde_json::json!({
            "accountId": "024147e7d076",
            "apiInfo": {
                "storageApi": {
                    "absoluteMinimumPartSize": 5000000,
                    "allowed": {
                        "buckets": null,
                        "capabilities": ["listFiles", "writeFiles", "listBuckets", "readBuckets"],
                        "namePrefix": null
                    },
                    "apiUrl": "https://api004.backblazeb2.com",
                    "downloadUrl": "https://f004.backblazeb2.com",
                    "recommendedPartSize": 100000000,
                    "s3ApiUrl": "https://s3.us-west-004.backblazeb2.com"
                }
            },
            "applicationKeyExpirationTimestamp": null,
            "authorizationToken": "<redacted>"
        })
    }

    #[test]
    fn finds_the_storage_api_block() {
        assert!(storage_api(&real_response()).is_some());
        assert!(storage_api(&serde_json::json!({})).is_none());
    }

    /// `s3ApiUrl` is the only one of the three URLs that is an S3 endpoint --
    /// this pins that we read the right field, since all three parse as valid
    /// URLs and only one works.
    #[test]
    fn reads_the_s3_url_and_not_the_native_or_download_url() {
        let body = real_response();
        let storage = storage_api(&body).unwrap();
        let s3 = storage.get("s3ApiUrl").and_then(Value::as_str).unwrap();
        assert_eq!(s3, "https://s3.us-west-004.backblazeb2.com");
        assert_eq!(
            parse_s3_api_url(s3).unwrap().0,
            "us-west-004",
            "the S3 url is the only one that yields a region"
        );
        for other in ["apiUrl", "downloadUrl"] {
            let url = storage.get(other).and_then(Value::as_str).unwrap();
            assert_eq!(
                parse_s3_api_url(url),
                None,
                "`{other}` must not be mistaken for an S3 endpoint"
            );
        }
    }

    /// `null` is what an unrestricted key actually reports. Reading it as
    /// "restricted to zero buckets" would make the form claim the key can see
    /// nothing.
    #[test]
    fn a_null_bucket_list_means_unrestricted() {
        let body = real_response();
        assert!(parse_allowed_buckets(storage_api(&body).unwrap()).is_empty());
    }

    #[test]
    fn an_absent_allowed_block_means_unrestricted() {
        let storage = serde_json::json!({ "s3ApiUrl": "https://s3.us-west-004.backblazeb2.com" });
        assert!(parse_allowed_buckets(&storage).is_empty());
        assert!(parse_capabilities(&storage).is_empty());
    }

    #[test]
    fn reads_the_names_of_a_bucket_restricted_key() {
        let storage = serde_json::json!({
            "allowed": {
                "buckets": [
                    { "id": "abc", "name": "pidan-tmps" },
                    { "id": "def", "name": "other-bucket" }
                ],
                "capabilities": ["listFiles"]
            }
        });
        assert_eq!(
            parse_allowed_buckets(&storage),
            vec!["pidan-tmps".to_string(), "other-bucket".to_string()]
        );
    }

    #[test]
    fn detects_the_list_buckets_capability() {
        let body = real_response();
        let caps = parse_capabilities(storage_api(&body).unwrap());
        assert!(caps.iter().any(|c| c == "listBuckets"));
        assert!(!caps.iter().any(|c| c == "deleteBuckets"));
    }

    /// A wrong credential pair is `InvalidCredentials`, not `AccessDenied`:
    /// this endpoint checks nothing *but* the pair, so the distinction is what
    /// tells the user to fix the key rather than its permissions.
    #[test]
    fn maps_401_to_invalid_credentials_and_403_to_access_denied() {
        assert_eq!(classify_status(401).code(), "auth/invalid-credentials");
        assert_eq!(classify_status(403).code(), "auth/access-denied");
        assert_eq!(classify_status(500).code(), "internal");
    }

    /// Rejected before any network call -- see `authorize_account`.
    #[tokio::test]
    async fn a_master_key_id_is_rejected_without_a_request() {
        let err = authorize_account("024147e7d076", "irrelevant")
            .await
            .expect_err("a master key id must not be accepted");
        assert_eq!(err.code(), "auth/invalid-credentials");
    }

    #[tokio::test]
    async fn blank_credentials_are_rejected_without_a_request() {
        for (key_id, key) in [("", "k"), ("004024147e7d0760000000001", "  "), ("", "")] {
            let err = authorize_account(key_id, key)
                .await
                .expect_err("blank credentials must not be accepted");
            assert_eq!(err.code(), "auth/invalid-credentials");
        }
    }
}
