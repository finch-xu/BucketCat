//! AWS S3-compatible provider (admin plane): bucket list/create/delete plus
//! a lightweight connection check, backed by `aws-sdk-s3`.
//!
//! This module is the only place in the crate that touches `aws_sdk_s3`
//! types outside of [`crate::error`]'s `From<aws_sdk_s3::Error>` -- the
//! [`Provider`] trait and [`Bucket`] DTO in [`crate::provider`] keep every
//! other caller (in particular `crate::commands`) free of an SDK dependency.

use async_trait::async_trait;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::error::SdkError;
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

/// Normalizes any of the three bucket-operation `SdkError`s into an
/// [`AppError`]: [`classify_sdk_error`] first, falling back to
/// `aws_sdk_s3::Error`'s own `From` impl (see `crate::error`) -- which
/// still owns modeled-service-error mapping and the last-resort keyword
/// sniff for anything neither layer recognizes -- for everything else.
fn normalize_s3_error<E, R>(err: SdkError<E, R>) -> AppError
where
    aws_sdk_s3::Error: From<SdkError<E, R>>,
{
    classify_sdk_error(&err).unwrap_or_else(|| AppError::from(aws_sdk_s3::Error::from(err)))
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
}
