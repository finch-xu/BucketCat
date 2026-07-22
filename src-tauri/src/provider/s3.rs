//! AWS S3-compatible provider (admin plane): bucket list/create/delete plus
//! a lightweight connection check, backed by `aws-sdk-s3`.
//!
//! This module is the only place in the crate that touches `aws_sdk_s3`
//! types outside of [`crate::error`]'s `From<aws_sdk_s3::Error>` -- the
//! [`Provider`] trait and [`Bucket`] DTO in [`crate::provider`] keep every
//! other caller (in particular `crate::commands`) free of an SDK dependency.

use async_trait::async_trait;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::DateTimeFormat;

use crate::error::{AppError, AppResult};
use crate::provider::{Bucket, Provider};
use crate::store::Connection;

/// Returns true only for real AWS endpoints (`*.amazonaws.com`, matched on
/// whole hostname labels, case-insensitively). Every S3-compatible backend
/// this app targets -- MinIO, Cloudflare R2, self-hosted -- returns false
/// here and gets `force_path_style(true)`; virtual-hosted-style addressing
/// otherwise breaks for endpoints that don't support wildcard DNS for
/// bucket subdomains.
pub fn is_aws_endpoint(endpoint: &str) -> bool {
    let without_scheme = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    let authority = without_scheme.split('/').next().unwrap_or("");
    // Strip a trailing `:port` when everything after the last `:` is
    // digits. Deliberately simple (no IPv6-literal handling): a non-AWS
    // host that confuses this still correctly returns `false`, which is
    // the safe (path-style) default.
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    };
    let host = host.to_ascii_lowercase();
    host == "amazonaws.com" || host.ends_with(".amazonaws.com")
}

/// A constructed S3 (or S3-compatible) admin-plane client for one saved
/// [`Connection`].
#[derive(Debug)]
pub struct S3Provider {
    client: aws_sdk_s3::Client,
}

/// Builds an [`S3Provider`] from a saved connection profile.
///
/// - An empty `endpoint` is rejected before any SDK type is touched. This
///   maps to [`AppError::Internal`] rather than a `storage/*` code: a
///   `Connection` with no endpoint should never have made it past input
///   validation at the store/command boundary, so reaching here with one
///   signals a bug upstream, not a normal "storage" condition (like a
///   missing bucket) the user caused.
/// - An empty `region` defaults to `"us-east-1"` -- the same fallback
///   `aws-cli`/`boto3` apply when no region is configured -- but **only**
///   for non-AWS endpoints ([`is_aws_endpoint`] is false): MinIO/R2/
///   self-hosted backends routinely don't care about region at all. A
///   real AWS endpoint with an empty region is rejected the same way as
///   an empty endpoint (`AppError::Internal`): the UI is expected to
///   prefill a region for AWS connections, so an empty one reaching here
///   signals the same kind of upstream-validation gap, and silently
///   guessing `"us-east-1"` for a real AWS account risks pointing
///   requests at the wrong region instead of failing fast.
/// - `force_path_style` is the negation of [`is_aws_endpoint`]: MinIO, R2
///   and other self-hosted backends need path-style addressing.
pub fn from_connection(conn: &Connection) -> AppResult<S3Provider> {
    if conn.endpoint.trim().is_empty() {
        return Err(AppError::Internal {
            message: "connection endpoint must not be empty".to_string(),
        });
    }

    let is_aws = is_aws_endpoint(&conn.endpoint);

    let region = if conn.region.trim().is_empty() {
        if is_aws {
            return Err(AppError::Internal {
                message: "connection region must not be empty for an AWS endpoint".to_string(),
            });
        }
        "us-east-1".to_string()
    } else {
        conn.region.clone()
    };

    let credentials = Credentials::new(
        conn.access_key_id.clone(),
        conn.secret_access_key.clone(),
        None,
        None,
        "bucketcat",
    );

    let config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .endpoint_url(conn.endpoint.clone())
        .region(Region::new(region))
        .credentials_provider(credentials)
        .force_path_style(!is_aws)
        .build();

    Ok(S3Provider {
        client: aws_sdk_s3::Client::from_conf(config),
    })
}

/// Classifies an [`SdkError`] into an [`AppError`] *before* it collapses
/// into `aws_sdk_s3::Error`, recovering the network-cause fidelity that
/// conversion throws away (see the doc comment on
/// `impl From<aws_sdk_s3::Error> for AppError` in `crate::error`).
///
/// Returns `None` for `SdkError::ServiceError` (and any future
/// `#[non_exhaustive]` variant this crate doesn't yet know about) so the
/// caller falls through to the existing `aws_sdk_s3::Error` conversion,
/// which is where modeled S3 errors like `NoSuchBucket` /
/// `BucketAlreadyExists` get their structured detail. Pure and
/// network-free: every branch is exercised in `tests` below by
/// constructing `SdkError` values directly via its public constructors,
/// with no real request involved.
fn classify_sdk_error<E, R>(err: &SdkError<E, R>) -> Option<AppError> {
    match err {
        SdkError::TimeoutError(_) => Some(AppError::Timeout),
        // `DispatchFailure` wraps a `ConnectorError`; only its `is_timeout`
        // kind maps to `Timeout`, everything else (io/user/other) never
        // reached the network successfully, so it's `Unreachable`.
        SdkError::DispatchFailure(failure) => Some(if failure.is_timeout() {
            AppError::Timeout
        } else {
            AppError::Unreachable
        }),
        // The request never made it onto the wire (e.g. the endpoint
        // resolver rejected the configured `endpoint_url`); there is no
        // response to inspect, so this is a connectivity problem from the
        // caller's point of view.
        SdkError::ConstructionFailure(_) => Some(AppError::Unreachable),
        // A response came back but couldn't be parsed as this protocol --
        // typically because the endpoint isn't actually an S3-compatible
        // server. Also a connectivity/target problem, not a modeled S3
        // error.
        SdkError::ResponseError(_) => Some(AppError::Unreachable),
        SdkError::ServiceError(_) => None,
        _ => None,
    }
}

/// Maps a raw S3 error *code* (as read off [`ProvideErrorMetadata::code`])
/// to an [`AppError`], recovering detail for codes the SDK's generated
/// per-operation error enum doesn't model as a distinct variant for some
/// operations. `ListBucketsError` and `DeleteBucketError` model *nothing*
/// but `Unhandled` (unlike e.g. `CreateBucketError`, which models
/// `BucketAlreadyExists`/`BucketAlreadyOwnedByYou`) -- so a wrong secret
/// key (`SignatureDoesNotMatch`) on `test_connection`/`list_buckets`, or a
/// delete against a bucket that's already gone (`NoSuchBucket`) on
/// `delete_bucket`, both erase to `aws_sdk_s3::Error::Unhandled` and, with
/// nothing to recover them, to `AppError::Internal` -- even though the
/// server's raw response said exactly what went wrong. Confirmed against
/// live MinIO by `tests/minio_e2e.rs`'s `wrong_secret_...` test, which
/// previously had to pin `internal` as a documented gap.
///
/// Only consulted from [`normalize_s3_error`] as the last fallback tier,
/// *after* the existing modeled-variant/`aws_sdk_s3::Error` mapping has had
/// its turn -- i.e. only for cases that would otherwise become
/// `AppError::Internal`. A real modeled variant (already a non-`Internal`
/// `AppError`) is never second-guessed by this.
///
/// Pure string -> variant mapping, deliberately factored out (rather than
/// threaded through `SdkError`'s generics) so it's directly testable with
/// plain string literals, no SDK error construction required.
fn classify_error_code(code: &str, message: Option<&str>) -> Option<AppError> {
    match code {
        "SignatureDoesNotMatch" | "InvalidAccessKeyId" => Some(AppError::InvalidCredentials),
        "AccessDenied" | "AccessDeniedException" => Some(AppError::AccessDenied),
        "NoSuchBucket" => Some(AppError::BucketNotFound {
            bucket: message.unwrap_or("unknown").to_string(),
        }),
        _ => None,
    }
}

/// Normalizes any of the three bucket-operation `SdkError`s into an
/// [`AppError`] in three tiers:
///
/// 1. [`classify_sdk_error`] -- network-cause fidelity for anything that
///    isn't a real, parsed service response.
/// 2. `aws_sdk_s3::Error`'s own `From` impl (see `crate::error`) -- modeled
///    per-operation variants (e.g. `BucketAlreadyExists`) first preference,
///    its own keyword sniff second.
/// 3. If (2) still lands on `AppError::Internal`, [`classify_error_code`]
///    gets one more attempt using the *raw* error code/message read off
///    the pre-erasure operation error (`SdkError::ServiceError`'s `E`, via
///    [`ProvideErrorMetadata`], captured before `err` is consumed by the
///    tier-2 conversion) -- this is what recovers e.g.
///    `SignatureDoesNotMatch` on `ListBuckets`/`DeleteBucket`, whose
///    generated error enums don't model anything but `Unhandled` and so
///    never reach tier 2's modeled-variant arms.
fn normalize_s3_error<E, R>(err: SdkError<E, R>) -> AppError
where
    E: ProvideErrorMetadata,
    aws_sdk_s3::Error: From<SdkError<E, R>>,
{
    if let Some(app_err) = classify_sdk_error(&err) {
        return app_err;
    }

    // At this point `err` is `SdkError::ServiceError` (the only case
    // `classify_sdk_error` returns `None` for, aside from a hypothetical
    // future `#[non_exhaustive]` variant it also can't classify -- for
    // which `code`/`message` below are simply `None`, a no-op for tier 3).
    let (code, message) = match &err {
        SdkError::ServiceError(ctx) => (
            ctx.err().code().map(str::to_string),
            ctx.err().message().map(str::to_string),
        ),
        _ => (None, None),
    };

    let mapped = AppError::from(aws_sdk_s3::Error::from(err));
    if matches!(mapped, AppError::Internal { .. }) {
        if let Some(app_err) = code
            .as_deref()
            .and_then(|c| classify_error_code(c, message.as_deref()))
        {
            return app_err;
        }
    }
    mapped
}

#[async_trait]
impl Provider for S3Provider {
    async fn test_connection(&self) -> AppResult<()> {
        self.list_buckets().await.map(|_| ())
    }

    async fn list_buckets(&self) -> AppResult<Vec<Bucket>> {
        let output = self
            .client
            .list_buckets()
            .send()
            .await
            .map_err(normalize_s3_error)?;

        Ok(output
            .buckets()
            .iter()
            .map(|b| Bucket {
                name: b.name().unwrap_or_default().to_string(),
                creation_date: b
                    .creation_date()
                    .and_then(|d| d.fmt(DateTimeFormat::DateTime).ok()),
            })
            .collect())
    }

    async fn create_bucket(&self, name: &str) -> AppResult<()> {
        self.client
            .create_bucket()
            .bucket(name)
            .send()
            .await
            .map_err(normalize_s3_error)?;
        Ok(())
    }

    async fn delete_bucket(&self, name: &str) -> AppResult<()> {
        self.client
            .delete_bucket()
            .bucket(name)
            .send()
            .await
            .map_err(normalize_s3_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::error::ErrorMetadata;

    fn aws_connection() -> Connection {
        Connection {
            id: "c1".to_string(),
            provider: "aws".to_string(),
            name: "prod".to_string(),
            endpoint: "https://s3.amazonaws.com".to_string(),
            region: "us-east-1".to_string(),
            access_key_id: "AKIAEXAMPLE".to_string(),
            secret_access_key: "secret".to_string(),
            default_bucket: None,
        }
    }

    fn minio_connection() -> Connection {
        Connection {
            id: "c2".to_string(),
            provider: "minio".to_string(),
            name: "local".to_string(),
            endpoint: "http://localhost:9000".to_string(),
            region: "".to_string(),
            access_key_id: "minioadmin".to_string(),
            secret_access_key: "minioadmin".to_string(),
            default_bucket: None,
        }
    }

    // --- is_aws_endpoint -------------------------------------------------

    #[test]
    fn recognizes_bare_aws_host() {
        assert!(is_aws_endpoint("s3.amazonaws.com"));
    }

    #[test]
    fn recognizes_scheme_prefixed_aws_host() {
        assert!(is_aws_endpoint("https://s3.amazonaws.com"));
    }

    #[test]
    fn recognizes_regional_aws_host() {
        assert!(is_aws_endpoint("https://s3.us-west-2.amazonaws.com"));
    }

    #[test]
    fn is_case_insensitive() {
        assert!(is_aws_endpoint("https://S3.AMAZONAWS.COM"));
    }

    #[test]
    fn rejects_localhost() {
        assert!(!is_aws_endpoint("http://localhost:9000"));
    }

    #[test]
    fn rejects_r2() {
        assert!(!is_aws_endpoint("https://abc123.r2.cloudflarestorage.com"));
    }

    #[test]
    fn rejects_lookalike_host_without_label_boundary() {
        // "evil-amazonaws.com" ends with the substring "amazonaws.com" but
        // is not a subdomain of it -- must not match.
        assert!(!is_aws_endpoint("https://evil-amazonaws.com"));
    }

    #[test]
    fn rejects_suffix_spoofed_host() {
        // "s3.amazonaws.com.evil.com" *starts* with a real AWS hostname but
        // the actual (right-most) hostname label is "evil.com" -- must not
        // match, since only a hostname *ending in* ".amazonaws.com" is AWS.
        assert!(!is_aws_endpoint("s3.amazonaws.com.evil.com"));
    }

    #[test]
    fn rejects_empty_string() {
        assert!(!is_aws_endpoint(""));
    }

    // --- from_connection ---------------------------------------------------

    #[test]
    fn empty_endpoint_is_rejected() {
        let mut conn = aws_connection();
        conn.endpoint = "".to_string();

        let err = from_connection(&conn).unwrap_err();

        assert_eq!(err.code(), "internal");
    }

    #[test]
    fn whitespace_only_endpoint_is_rejected() {
        let mut conn = aws_connection();
        conn.endpoint = "   ".to_string();

        assert!(from_connection(&conn).is_err());
    }

    #[test]
    fn valid_aws_connection_builds_successfully() {
        assert!(from_connection(&aws_connection()).is_ok());
    }

    #[test]
    fn valid_minio_connection_builds_successfully() {
        assert!(from_connection(&minio_connection()).is_ok());
    }

    #[test]
    fn non_aws_empty_region_defaults_to_us_east_1() {
        let conn = minio_connection();
        assert_eq!(conn.region, "");

        let provider = from_connection(&conn).unwrap();

        assert_eq!(
            provider.client.config().region().map(|r| r.as_ref()),
            Some("us-east-1")
        );
    }

    #[test]
    fn aws_endpoint_with_empty_region_is_rejected() {
        let mut conn = aws_connection();
        conn.region = "".to_string();

        let err = from_connection(&conn).unwrap_err();

        assert_eq!(err.code(), "internal");
    }

    #[test]
    fn aws_endpoint_with_whitespace_only_region_is_rejected() {
        let mut conn = aws_connection();
        conn.region = "   ".to_string();

        assert!(from_connection(&conn).is_err());
    }

    // --- SdkError classification (pure, no network) -------------------------

    #[test]
    fn classify_timeout_error_as_timeout() {
        let err: SdkError<(), ()> = SdkError::timeout_error("boom");
        assert!(matches!(classify_sdk_error(&err), Some(AppError::Timeout)));
    }

    #[test]
    fn classify_dispatch_timeout_as_timeout() {
        let connector_err = aws_sdk_s3::error::ConnectorError::timeout("boom".into());
        let err: SdkError<(), ()> = SdkError::dispatch_failure(connector_err);
        assert!(matches!(classify_sdk_error(&err), Some(AppError::Timeout)));
    }

    #[test]
    fn classify_dispatch_io_as_unreachable() {
        let connector_err = aws_sdk_s3::error::ConnectorError::io("boom".into());
        let err: SdkError<(), ()> = SdkError::dispatch_failure(connector_err);
        assert!(matches!(
            classify_sdk_error(&err),
            Some(AppError::Unreachable)
        ));
    }

    #[test]
    fn classify_construction_failure_as_unreachable() {
        let err: SdkError<(), ()> = SdkError::construction_failure("boom");
        assert!(matches!(
            classify_sdk_error(&err),
            Some(AppError::Unreachable)
        ));
    }

    #[test]
    fn classify_response_error_as_unreachable() {
        let err: SdkError<(), ()> = SdkError::response_error("boom", ());
        assert!(matches!(
            classify_sdk_error(&err),
            Some(AppError::Unreachable)
        ));
    }

    #[test]
    fn classify_service_error_defers_to_from_impl() {
        let err: SdkError<(), ()> = SdkError::service_error((), ());
        assert!(classify_sdk_error(&err).is_none());
    }

    #[test]
    fn normalize_service_error_uses_existing_bucket_already_exists_mapping() {
        let inner = aws_sdk_s3::types::error::BucketAlreadyExists::builder()
            .message("taken")
            .build();
        let create_err =
            aws_sdk_s3::operation::create_bucket::CreateBucketError::BucketAlreadyExists(inner);
        let sdk_err: SdkError<_, ()> = SdkError::service_error(create_err, ());

        let app_err = normalize_s3_error(sdk_err);

        assert_eq!(app_err.code(), "storage/bucket-exists");
    }

    #[test]
    fn normalize_timeout_does_not_fall_through_to_keyword_sniff() {
        let err: SdkError<aws_sdk_s3::operation::list_buckets::ListBucketsError, ()> =
            SdkError::timeout_error("connection timed out");

        let app_err = normalize_s3_error(err);

        assert_eq!(app_err.code(), "network/timeout");
    }

    // --- classify_error_code (pure) -----------------------------------------

    #[test]
    fn classify_error_code_maps_signature_does_not_match_to_invalid_credentials() {
        assert!(matches!(
            classify_error_code("SignatureDoesNotMatch", None),
            Some(AppError::InvalidCredentials)
        ));
    }

    #[test]
    fn classify_error_code_maps_invalid_access_key_id_to_invalid_credentials() {
        assert!(matches!(
            classify_error_code("InvalidAccessKeyId", None),
            Some(AppError::InvalidCredentials)
        ));
    }

    #[test]
    fn classify_error_code_maps_access_denied_variants_to_access_denied() {
        assert!(matches!(
            classify_error_code("AccessDenied", None),
            Some(AppError::AccessDenied)
        ));
        assert!(matches!(
            classify_error_code("AccessDeniedException", None),
            Some(AppError::AccessDenied)
        ));
    }

    #[test]
    fn classify_error_code_maps_no_such_bucket_using_message_as_bucket_name() {
        let app_err = classify_error_code("NoSuchBucket", Some("my-bucket"));
        assert!(matches!(
            app_err,
            Some(AppError::BucketNotFound { ref bucket }) if bucket == "my-bucket"
        ));
    }

    #[test]
    fn classify_error_code_no_such_bucket_falls_back_to_unknown_without_message() {
        let app_err = classify_error_code("NoSuchBucket", None);
        assert!(matches!(
            app_err,
            Some(AppError::BucketNotFound { ref bucket }) if bucket == "unknown"
        ));
    }

    #[test]
    fn classify_error_code_returns_none_for_unrecognized_code() {
        assert!(classify_error_code("SomeOtherCode", None).is_none());
    }

    // --- normalize_s3_error metadata-code fallback (pure, no network) -------

    #[test]
    fn normalize_recovers_invalid_credentials_from_unmodeled_list_buckets_error() {
        // Reproduces the real MinIO wire response from a wrong secret key
        // (see tests/minio_e2e.rs): a 403 with S3 error code
        // `SignatureDoesNotMatch`, which `ListBucketsError` doesn't model
        // as its own variant, so the SDK's own conversion alone would
        // collapse this to `AppError::Internal`.
        let meta = ErrorMetadata::builder()
            .code("SignatureDoesNotMatch")
            .message("The request signature we calculated does not match")
            .build();
        let unhandled = aws_sdk_s3::operation::list_buckets::ListBucketsError::generic(meta);
        let sdk_err: SdkError<_, ()> = SdkError::service_error(unhandled, ());

        let app_err = normalize_s3_error(sdk_err);

        assert_eq!(app_err.code(), "auth/invalid-credentials");
    }

    #[test]
    fn normalize_recovers_bucket_not_found_from_unmodeled_delete_bucket_error() {
        let meta = ErrorMetadata::builder()
            .code("NoSuchBucket")
            .message("no-such-bucket")
            .build();
        let unhandled = aws_sdk_s3::operation::delete_bucket::DeleteBucketError::generic(meta);
        let sdk_err: SdkError<_, ()> = SdkError::service_error(unhandled, ());

        let app_err = normalize_s3_error(sdk_err);

        assert_eq!(app_err.code(), "storage/bucket-not-found");
    }

    #[test]
    fn normalize_leaves_modeled_variant_mapping_untouched() {
        // A modeled variant (tier 2) must win outright -- the metadata-code
        // fallback (tier 3) must never be consulted, let alone override it.
        let inner = aws_sdk_s3::types::error::BucketAlreadyExists::builder()
            .message("taken")
            .build();
        let create_err =
            aws_sdk_s3::operation::create_bucket::CreateBucketError::BucketAlreadyExists(inner);
        let sdk_err: SdkError<_, ()> = SdkError::service_error(create_err, ());

        let app_err = normalize_s3_error(sdk_err);

        assert_eq!(app_err.code(), "storage/bucket-exists");
    }

    #[test]
    fn normalize_unrecognized_service_error_still_falls_through_to_internal() {
        let meta = ErrorMetadata::builder().code("SomeUnknownCode").build();
        let unhandled = aws_sdk_s3::operation::list_buckets::ListBucketsError::generic(meta);
        let sdk_err: SdkError<_, ()> = SdkError::service_error(unhandled, ());

        let app_err = normalize_s3_error(sdk_err);

        assert_eq!(app_err.code(), "internal");
    }
}
