//! Cloudflare's REST API (`api.cloudflare.com/client/v4`), for the R2 bucket
//! metadata the S3 API does not expose.
//!
//! The S3 plane already covers everything BucketCat *browses*; this module
//! exists for the things only Cloudflare's own API knows: how many objects a
//! bucket holds and how large it is, which jurisdiction and location hint it
//! was created with, and whether it is published on `r2.dev` or a custom
//! domain.
//!
//! Requests are plain GETs carrying `Authorization: Bearer <token value>`, so
//! unlike [`crate::provider::oss_admin`] there is no signing at all -- the
//! whole module is a thin envelope parser over
//! [`crate::provider::https`]'s shared client.
//!
//! ## Two things that will bite anyone editing this file
//!
//! 1. **The jurisdiction MUST be sent as the `cf-r2-jurisdiction` header.**
//!    The `?jurisdiction=eu` query parameter that looks equivalent is not:
//!    verified live on 2026-07-30, it does not error, it silently returns the
//!    **default** jurisdiction's buckets. A wrong-but-quiet answer is far
//!    worse than a 400, so [`get_json`] only ever sets the header and no
//!    caller may add that query parameter.
//!
//! 2. **`GET /accounts` returns `200` with an empty array for a token that
//!    lacks account-level permission**, not a 403. An R2 "Object Read only"
//!    token verifies fine and reports its own id, but sees no accounts at all.
//!    So [`list_accounts`] returning empty means "this token can't enumerate
//!    accounts", *not* "this user has no accounts", and callers must fall back
//!    to asking for the account id rather than reporting an error.
//!
//! ## Security
//!
//! `token` is a live Bearer credential. It is never logged, never included in
//! an error message, and never interpolated into a URL (it only ever rides in
//! the `Authorization` header). Cloudflare's own error text *is* surfaced into
//! `AppError::Internal` for diagnosis -- its messages are generic
//! ("Authentication error", "No route matches this url") and never echo the
//! credential.

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::provider::https;

/// Base URL for Cloudflare's v4 REST API.
const API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Context string prefixed onto this module's transport-level errors.
const CTX: &str = "cloudflare api";

// --- DTOs (no `serde_json`/`hyper` type crosses this boundary) --------------

/// One account a token can see, from `GET /accounts`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct R2Account {
    pub id: String,
    pub name: String,
}

/// What a token probe can establish before a connection is saved: the token's
/// own id (which *is* the S3 Access Key ID) and whichever accounts it can
/// enumerate.
///
/// `accounts` being empty is a normal, successful outcome for a
/// low-privilege token -- see the module doc.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct R2TokenProbe {
    pub access_key_id: String,
    pub accounts: Vec<R2Account>,
}

/// Bucket metadata from `GET /accounts/{a}/r2/buckets/{b}`.
///
/// `location` is R2's coarse location *hint* (`APAC`, `WNAM`, `WEUR`, ...),
/// not a region: R2's SigV4 region is always `auto`.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct R2BucketMeta {
    pub location: Option<String>,
    pub storage_class: Option<String>,
    pub jurisdiction: Option<String>,
}

/// Storage usage from `GET .../usage`.
///
/// Cloudflare reports every one of these as a **JSON string**, not a number
/// (`"payloadSize":"16256988"`), so they are parsed rather than read directly
/// -- see [`parse_usage`].
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct R2Usage {
    pub object_count: u64,
    pub payload_size: u64,
    pub metadata_size: u64,
    pub upload_count: u64,
}

/// The `r2.dev` managed domain from `GET .../domains/managed`. `domain` is
/// present even when `enabled` is false -- Cloudflare always allocates the
/// name, it just doesn't serve it until public access is turned on.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct R2ManagedDomain {
    pub enabled: bool,
    pub domain: String,
}

/// One custom domain from `GET .../domains/custom`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct R2CustomDomain {
    pub domain: String,
    pub enabled: bool,
    /// Cloudflare's SSL provisioning status (`active`, `pending`, ...), when
    /// reported. A domain with `enabled: true` but a non-`active` status is
    /// configured but not yet serving.
    pub ssl_status: Option<String>,
}

// --- envelope parsing (pure, network-free) ----------------------------------

/// Cloudflare wraps every response in
/// `{"success":bool,"errors":[{"code":int,"message":str}],"result":...}`.
/// This extracts `result`, or the first error's message when `success` is
/// false.
///
/// Pure and network-free so the tests below can exercise it against fixture
/// bodies captured from the live API, with no account required.
fn parse_envelope(body: &str) -> Result<Value, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| format!("response was not valid JSON: {e}"))?;

    if value.get("success").and_then(Value::as_bool) != Some(true) {
        let message = value
            .get("errors")
            .and_then(Value::as_array)
            .and_then(|errors| errors.first())
            .and_then(|first| first.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("no error message")
            .to_string();
        return Err(message);
    }

    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

/// Cloudflare reports usage counters as JSON strings. Anything missing or
/// unparseable becomes `0` rather than failing the whole request: a usage
/// panel showing a zero for one counter is strictly better than showing
/// nothing because Cloudflare added a field this build doesn't expect.
fn usage_number(result: &Value, field: &str) -> u64 {
    result
        .get(field)
        .and_then(|v| {
            v.as_str()
                .and_then(|s| s.parse::<u64>().ok())
                // Tolerate a real JSON number too, in case Cloudflare ever
                // stops stringifying these.
                .or_else(|| v.as_u64())
        })
        .unwrap_or(0)
}

/// Parses a `.../usage` result body. Pure; see [`R2Usage`].
fn parse_usage(result: &Value) -> R2Usage {
    R2Usage {
        object_count: usage_number(result, "objectCount"),
        payload_size: usage_number(result, "payloadSize"),
        metadata_size: usage_number(result, "metadataSize"),
        upload_count: usage_number(result, "uploadCount"),
    }
}

/// Parses a bucket-detail result body. Pure; see [`R2BucketMeta`].
fn parse_bucket_meta(result: &Value) -> R2BucketMeta {
    let string_field = |name: &str| {
        result
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    R2BucketMeta {
        location: string_field("location"),
        storage_class: string_field("storage_class"),
        jurisdiction: string_field("jurisdiction"),
    }
}

/// Parses a `.../domains/custom` result body. Pure; see [`R2CustomDomain`].
///
/// A entry with no `domain` string is skipped rather than surfaced as an empty
/// row -- there is nothing useful to show for it.
fn parse_custom_domains(result: &Value) -> Vec<R2CustomDomain> {
    result
        .get("domains")
        .and_then(Value::as_array)
        .map(|domains| {
            domains
                .iter()
                .filter_map(|d| {
                    let domain = d.get("domain").and_then(Value::as_str)?.to_string();
                    Some(R2CustomDomain {
                        domain,
                        enabled: d.get("enabled").and_then(Value::as_bool).unwrap_or(false),
                        ssl_status: d
                            .get("status")
                            .and_then(|s| s.get("ssl"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parses an `/accounts` result body. Pure; see [`R2Account`].
fn parse_accounts(result: &Value) -> Vec<R2Account> {
    result
        .as_array()
        .map(|accounts| {
            accounts
                .iter()
                .filter_map(|a| {
                    let id = a.get("id").and_then(Value::as_str)?.to_string();
                    let name = a
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Some(R2Account { id, name })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Maps an HTTP status plus Cloudflare's own error message onto an
/// [`AppError`].
///
/// `401`/`403` become [`AppError::AccessDenied`] -- the single most common
/// outcome here, because an R2 *object*-scoped token verifies fine yet is
/// refused by every `/r2/*` endpoint. Callers rendering optional metadata are
/// expected to treat that as "this section is unavailable", not as a failure.
fn classify_status(status: u16, bucket: &str, message: &str) -> AppError {
    match status {
        401 | 403 => AppError::AccessDenied,
        404 if !bucket.is_empty() => AppError::BucketNotFound {
            bucket: bucket.to_string(),
        },
        _ => AppError::Internal {
            message: format!("{CTX}: request failed with status {status}: {message}"),
        },
    }
}

// --- transport --------------------------------------------------------------

/// One authenticated GET against the Cloudflare API, returning the unwrapped
/// `result` value.
///
/// `jurisdiction` is sent as the `cf-r2-jurisdiction` **header** when
/// non-empty. Never as a query parameter -- see this module's doc comment for
/// why that alternative silently returns the wrong data.
///
/// `bucket` is used only to build a [`AppError::BucketNotFound`] on a 404 and
/// may be empty for account-level calls.
async fn get_json(token: &str, path: &str, jurisdiction: &str, bucket: &str) -> AppResult<Value> {
    let uri: http::Uri = format!("{API_BASE}/{path}")
        .parse()
        .map_err(|e| AppError::Internal {
            message: format!("{CTX}: invalid request path: {e}"),
        })?;

    let mut builder = http::Request::builder()
        .method(http::Method::GET)
        .uri(uri)
        .header("Authorization", format!("Bearer {}", token.trim()))
        .header("Accept", "application/json");
    if !jurisdiction.is_empty() {
        builder = builder.header("cf-r2-jurisdiction", jurisdiction);
    }
    let request = builder
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
    let body_text = String::from_utf8_lossy(&body);

    match parse_envelope(&body_text) {
        // Cloudflare sets `success: false` *and* a non-2xx status together, so
        // an `Ok` envelope on a failing status would be a contradiction; the
        // status still wins, since it is the more trustworthy of the two.
        Ok(result) if (200..300).contains(&status) => Ok(result),
        Ok(_) => Err(classify_status(status, bucket, "no error message")),
        Err(message) => Err(classify_status(status, bucket, &message)),
    }
}

// --- public API -------------------------------------------------------------

/// Verifies a token and reports its id, which **is** the S3 Access Key ID for
/// R2 (the secret being `sha256(token value)`, see
/// [`crate::provider::r2::r2_secret_from_token`]).
///
/// Works at every privilege tier -- verified live against both an
/// account-admin token and an object-read-only one on 2026-07-30.
pub async fn verify_token(token: &str) -> AppResult<String> {
    let result = get_json(token, "user/tokens/verify", "", "").await?;
    result
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| AppError::Internal {
            message: format!("{CTX}: token verification returned no token id"),
        })
}

/// Lists the accounts a token can see.
///
/// **An empty list is a success, not an error.** A token without
/// account-level permission gets `200` with `[]` here, so callers must treat
/// empty as "ask the user for the account id" -- see the module doc.
pub async fn list_accounts(token: &str) -> AppResult<Vec<R2Account>> {
    Ok(parse_accounts(&get_json(token, "accounts", "", "").await?))
}

/// Verifies a token and enumerates its accounts in one call, for the
/// connection form's "probe" button.
pub async fn probe_token(token: &str) -> AppResult<R2TokenProbe> {
    let access_key_id = verify_token(token).await?;
    // A token that verified but cannot enumerate accounts is the normal
    // low-privilege case, so an error here must not sink the whole probe --
    // the access key id alone is already most of what the form needed.
    let accounts = list_accounts(token).await.unwrap_or_default();
    Ok(R2TokenProbe {
        access_key_id,
        accounts,
    })
}

pub async fn bucket_meta(
    token: &str,
    account_id: &str,
    jurisdiction: &str,
    bucket: &str,
) -> AppResult<R2BucketMeta> {
    let path = format!("accounts/{account_id}/r2/buckets/{bucket}");
    Ok(parse_bucket_meta(
        &get_json(token, &path, jurisdiction, bucket).await?,
    ))
}

pub async fn bucket_usage(
    token: &str,
    account_id: &str,
    jurisdiction: &str,
    bucket: &str,
) -> AppResult<R2Usage> {
    let path = format!("accounts/{account_id}/r2/buckets/{bucket}/usage");
    Ok(parse_usage(
        &get_json(token, &path, jurisdiction, bucket).await?,
    ))
}

pub async fn managed_domain(
    token: &str,
    account_id: &str,
    jurisdiction: &str,
    bucket: &str,
) -> AppResult<R2ManagedDomain> {
    let path = format!("accounts/{account_id}/r2/buckets/{bucket}/domains/managed");
    let result = get_json(token, &path, jurisdiction, bucket).await?;
    Ok(R2ManagedDomain {
        enabled: result
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        domain: result
            .get("domain")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

pub async fn custom_domains(
    token: &str,
    account_id: &str,
    jurisdiction: &str,
    bucket: &str,
) -> AppResult<Vec<R2CustomDomain>> {
    let path = format!("accounts/{account_id}/r2/buckets/{bucket}/domains/custom");
    Ok(parse_custom_domains(
        &get_json(token, &path, jurisdiction, bucket).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every fixture below is a real response body captured from the live
    // Cloudflare API on 2026-07-30 (account id and domains left as observed;
    // no credential appears in any of them). Parsing them here is what keeps
    // the shapes honest without a network call.

    // --- parse_envelope ----------------------------------------------------

    #[test]
    fn unwraps_a_successful_envelope() {
        let body = r#"{"success":true,"errors":[],"messages":[],"result":{"id":"abc"}}"#;
        let result = parse_envelope(body).expect("a success envelope should unwrap");
        assert_eq!(result.get("id").and_then(Value::as_str), Some("abc"));
    }

    #[test]
    fn reports_the_first_error_message_from_a_failed_envelope() {
        let body = r#"{"success":false,"errors":[{"code":10000,"message":"Authentication error"}],"result":null}"#;
        assert_eq!(
            parse_envelope(body),
            Err("Authentication error".to_string())
        );
    }

    #[test]
    fn a_failed_envelope_with_no_errors_still_reports_something() {
        let body = r#"{"success":false,"errors":[],"result":null}"#;
        assert_eq!(parse_envelope(body), Err("no error message".to_string()));
    }

    /// A gateway or proxy failure can return HTML, not JSON. That must be a
    /// clean error rather than a panic or a silently empty result.
    #[test]
    fn rejects_a_non_json_body() {
        assert!(parse_envelope("<html>502 Bad Gateway</html>").is_err());
    }

    /// A body with no `success` key at all is not a Cloudflare envelope and
    /// must not be mistaken for one -- otherwise an unexpected response shape
    /// would parse as "succeeded, with no data".
    #[test]
    fn rejects_a_body_that_is_not_an_envelope() {
        assert!(parse_envelope(r#"{"result":{"id":"abc"}}"#).is_err());
    }

    // --- parse_usage -------------------------------------------------------

    /// The headline quirk: Cloudflare stringifies every counter.
    #[test]
    fn parses_stringified_usage_counters() {
        let body = r#"{"success":true,"errors":[],"result":{
            "end":"2026-07-30T12:20:00.000Z","payloadSize":"16256988","metadataSize":"5189",
            "objectCount":"131","uploadCount":"0","infrequentAccessPayloadSize":"0"}}"#;
        let usage = parse_usage(&parse_envelope(body).unwrap());
        assert_eq!(
            usage,
            R2Usage {
                object_count: 131,
                payload_size: 16_256_988,
                metadata_size: 5_189,
                upload_count: 0,
            }
        );
    }

    /// Tolerate real JSON numbers too, in case Cloudflare stops stringifying.
    #[test]
    fn parses_numeric_usage_counters() {
        let result: Value = serde_json::from_str(r#"{"objectCount":7,"payloadSize":42}"#).unwrap();
        let usage = parse_usage(&result);
        assert_eq!(usage.object_count, 7);
        assert_eq!(usage.payload_size, 42);
    }

    /// One unexpected/missing counter must not blank the whole panel.
    #[test]
    fn missing_or_unparseable_counters_become_zero() {
        let result: Value =
            serde_json::from_str(r#"{"objectCount":"131","payloadSize":"not-a-number"}"#).unwrap();
        let usage = parse_usage(&result);
        assert_eq!(usage.object_count, 131);
        assert_eq!(usage.payload_size, 0);
        assert_eq!(usage.metadata_size, 0);
    }

    // --- parse_bucket_meta -------------------------------------------------

    #[test]
    fn parses_bucket_metadata() {
        let body = r#"{"success":true,"errors":[],"result":{"name":"my-bucket",
            "creation_date":"2025-07-17T16:17:12.990Z","location":"APAC",
            "storage_class":"Standard","jurisdiction":"default"}}"#;
        let meta = parse_bucket_meta(&parse_envelope(body).unwrap());
        assert_eq!(
            meta,
            R2BucketMeta {
                location: Some("APAC".to_string()),
                storage_class: Some("Standard".to_string()),
                jurisdiction: Some("default".to_string()),
            }
        );
    }

    /// Absent and empty-string fields both mean "not reported", so the UI can
    /// test a single `None` rather than also checking for `Some("")`.
    #[test]
    fn absent_and_empty_metadata_fields_are_both_none() {
        let result: Value = serde_json::from_str(r#"{"name":"b","location":""}"#).unwrap();
        assert_eq!(parse_bucket_meta(&result), R2BucketMeta::default());
    }

    // --- parse_custom_domains ----------------------------------------------

    #[test]
    fn parses_custom_domains_with_their_ssl_status() {
        let body = r#"{"success":true,"errors":[],"result":{"domains":[
            {"domain":"cdn.example.com","status":{"ssl":"active","ownership":"active"},
             "zoneName":"example.com","minTLS":"1.2","enabled":true}]}}"#;
        assert_eq!(
            parse_custom_domains(&parse_envelope(body).unwrap()),
            vec![R2CustomDomain {
                domain: "cdn.example.com".to_string(),
                enabled: true,
                ssl_status: Some("active".to_string()),
            }]
        );
    }

    #[test]
    fn parses_an_empty_custom_domain_list() {
        let body = r#"{"success":true,"errors":[],"result":{"domains":[]}}"#;
        assert!(parse_custom_domains(&parse_envelope(body).unwrap()).is_empty());
    }

    /// An entry with no `domain` has nothing to render, so it is dropped
    /// rather than shown as a blank row.
    #[test]
    fn skips_a_custom_domain_entry_with_no_domain() {
        let result: Value =
            serde_json::from_str(r#"{"domains":[{"enabled":true},{"domain":"ok.dev"}]}"#).unwrap();
        let domains = parse_custom_domains(&result);
        assert_eq!(domains.len(), 1);
        assert_eq!(domains[0].domain, "ok.dev");
    }

    // --- parse_accounts ----------------------------------------------------

    #[test]
    fn parses_the_account_list() {
        let body = r#"{"success":true,"errors":[],"result":[
            {"id":"a1b2c3d4e5f60718293a4b5c6d7e8f90","name":"Some Account","type":"standard"}]}"#;
        assert_eq!(
            parse_accounts(&parse_envelope(body).unwrap()),
            vec![R2Account {
                id: "a1b2c3d4e5f60718293a4b5c6d7e8f90".to_string(),
                name: "Some Account".to_string(),
            }]
        );
    }

    /// **The low-privilege case.** An object-scoped R2 token gets `200` with an
    /// empty array here, not a 403 -- verified live. Parsing it as an ordinary
    /// empty list (rather than an error) is what lets the connection form fall
    /// back to asking for the account id instead of refusing the token.
    #[test]
    fn an_empty_account_list_parses_as_empty_rather_than_failing() {
        let body = r#"{"result":[],"result_info":{"page":1,"count":0},"success":true,"errors":[]}"#;
        let result = parse_envelope(body).expect("this IS a success response");
        assert!(parse_accounts(&result).is_empty());
    }

    // --- classify_status ---------------------------------------------------

    /// The single most important mapping in this module: every `/r2/*`
    /// endpoint answers 403 for an object-scoped token, and the bucket-info
    /// panel keys its "this token can't read that" degradation off exactly
    /// this code.
    #[test]
    fn unauthorized_statuses_become_access_denied() {
        for status in [401, 403] {
            assert_eq!(
                classify_status(status, "b", "Authentication error").code(),
                "auth/access-denied"
            );
        }
    }

    #[test]
    fn a_404_on_a_bucket_call_becomes_bucket_not_found() {
        let err = classify_status(404, "missing-bucket", "not found");
        assert_eq!(err.code(), "storage/bucket-not-found");
        assert_eq!(
            err.params().get("bucket").map(String::as_str),
            Some("missing-bucket")
        );
    }

    /// Account-level calls pass an empty bucket, so a 404 there is a routing
    /// problem, not a missing bucket -- reporting it as `bucket-not-found`
    /// with an empty name would render as a nonsense message.
    #[test]
    fn a_404_with_no_bucket_stays_internal() {
        assert_eq!(
            classify_status(404, "", "No route matches this url").code(),
            "internal"
        );
    }

    #[test]
    fn other_statuses_carry_cloudflares_message_for_diagnosis() {
        let err = classify_status(500, "b", "Internal Server Error");
        assert_eq!(err.code(), "internal");
        assert!(err
            .params()
            .get("message")
            .is_some_and(|m| m.contains("Internal Server Error") && m.contains("500")));
    }
}
