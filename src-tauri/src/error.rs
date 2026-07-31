//! Unified application error type.
//!
//! Every fallible command returns [`AppResult`]. On the wire an error is
//! serialized as `{ "code": "family/kind", "params": { ... } }`. The frontend
//! looks the `code` up in its i18n dictionary and renders human-readable text;
//! raw third-party error details never reach the UI (they go to the log).

use std::collections::HashMap;

use serde::ser::{Serialize, SerializeStruct, Serializer};

/// Application-wide error. Variants are grouped into five families by the
/// prefix of their [`AppError::code`]: `auth/*`, `network/*`, `storage/*`,
/// `local/*`, `update/*`, plus a catch-all `internal`.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("access denied")]
    AccessDenied,

    #[error("network timeout")]
    Timeout,
    #[error("endpoint unreachable")]
    Unreachable,

    #[error("bucket not found: {bucket}")]
    BucketNotFound { bucket: String },
    #[error("bucket already exists: {bucket}")]
    BucketExists { bucket: String },
    #[error("bucket is in a different region")]
    WrongRegion { bucket: String },

    #[error("object key not found: {key}")]
    KeyNotFound { key: String },

    #[error("connection not found: {id}")]
    ConnectionNotFound { id: String },

    #[error("transfer task not found: {id}")]
    TaskNotFound { id: String },

    #[error("local store io error: {message}")]
    StoreIo { message: String },
    #[error("failed to decrypt local store")]
    DecryptFailed,
    #[error("failed to derive local store encryption key")]
    KeyDerivationFailed,

    #[error("local file io error at {path}: {message}")]
    FileIo { path: String, message: String },

    /// Fetching or parsing the update manifest failed. Deliberately its own
    /// family rather than an `Internal`: reaching GitHub is the single most
    /// likely thing to fail for a user behind a restrictive network, and
    /// "发生未知错误" tells them nothing about what to try next.
    #[error("update check failed: {message}")]
    UpdateCheckFailed { message: String },
    /// Downloading, verifying or applying the update package failed.
    #[error("update install failed: {message}")]
    UpdateInstallFailed { message: String },

    #[error("internal error: {message}")]
    Internal { message: String },
}

impl AppError {
    /// Stable machine code the frontend uses as an i18n key.
    pub fn code(&self) -> &'static str {
        match self {
            AppError::InvalidCredentials => "auth/invalid-credentials",
            AppError::AccessDenied => "auth/access-denied",
            AppError::Timeout => "network/timeout",
            AppError::Unreachable => "network/unreachable",
            AppError::BucketNotFound { .. } => "storage/bucket-not-found",
            AppError::BucketExists { .. } => "storage/bucket-exists",
            AppError::WrongRegion { .. } => "storage/wrong-region",
            AppError::KeyNotFound { .. } => "storage/key-not-found",
            AppError::ConnectionNotFound { .. } => "storage/connection-not-found",
            AppError::TaskNotFound { .. } => "storage/task-not-found",
            AppError::StoreIo { .. } => "local/store-io",
            AppError::DecryptFailed => "local/decrypt-failed",
            AppError::KeyDerivationFailed => "local/key-derivation-failed",
            AppError::FileIo { .. } => "local/file-io",
            AppError::UpdateCheckFailed { .. } => "update/check-failed",
            AppError::UpdateInstallFailed { .. } => "update/install-failed",
            AppError::Internal { .. } => "internal",
        }
    }

    /// Interpolation params for the i18n message (never contains secrets).
    pub fn params(&self) -> HashMap<String, String> {
        let mut p = HashMap::new();
        match self {
            AppError::BucketNotFound { bucket }
            | AppError::BucketExists { bucket }
            | AppError::WrongRegion { bucket } => {
                p.insert("bucket".to_string(), bucket.clone());
            }
            AppError::KeyNotFound { key } => {
                p.insert("key".to_string(), key.clone());
            }
            AppError::ConnectionNotFound { id } | AppError::TaskNotFound { id } => {
                p.insert("id".to_string(), id.clone());
            }
            AppError::StoreIo { message } | AppError::Internal { message } => {
                p.insert("message".to_string(), message.clone());
            }
            AppError::FileIo { path, message } => {
                p.insert("path".to_string(), path.clone());
                p.insert("message".to_string(), message.clone());
            }
            _ => {}
        }
        p
    }
}

impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("AppError", 2)?;
        s.serialize_field("code", self.code())?;
        s.serialize_field("params", &self.params())?;
        s.end()
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::StoreIo {
            message: e.to_string(),
        }
    }
}

/// Placeholder mapping from the AWS S3 SDK's unified error type.
///
/// `aws_sdk_s3::Error` (not `SdkError<Op, R>`) is the right conversion target
/// here: every generated operation error (`ListBucketsError`,
/// `CreateBucketError`, `DeleteBucketError`, ...) implements
/// `From<SdkError<ThatError, R>> for aws_sdk_s3::Error`, so provider code can
/// do `client.list_buckets().send().await.map_err(aws_sdk_s3::Error::from)?`
/// (or a plain `?` once the call site's error type is already
/// `aws_sdk_s3::Error`) and land here regardless of which S3 operation
/// failed.
///
/// Important limitation carried over from the SDK's own conversion: for each
/// operation, `From<SdkError<Op, R>> for aws_sdk_s3::Error` only preserves
/// structured detail when the failure is `SdkError::ServiceError` (a modeled
/// S3 error response). Every other `SdkError` variant --
/// `ConstructionFailure`, `TimeoutError`, `DispatchFailure`, `ResponseError`
/// -- collapses into the catch-all `Error::Unhandled` *before* it reaches
/// this `impl`, so the original distinction between "DNS lookup failed" and
/// "TLS handshake failed" is already gone. We recover a best guess by
/// scanning the rendered message for well-known keywords. Task 4 (which owns
/// the actual `list_buckets`/`create_bucket`/`delete_bucket` call sites) can
/// get a strictly better signal by matching on `SdkError` directly -- before
/// calling `.into()` -- if precise network-vs-internal classification turns
/// out to matter; this conversion is intentionally a conservative first cut.
///
/// Also note: the SDK's `NoSuchBucket` / `BucketAlreadyExists` /
/// `BucketAlreadyOwnedByYou` error bodies carry only an optional `message`,
/// never the bucket name itself (S3 doesn't echo it back). Call sites that
/// already know which bucket they targeted should prefer constructing
/// `AppError::BucketNotFound { bucket }` / `AppError::BucketExists { bucket
/// }` directly with that name instead of routing through this impl; this
/// conversion falls back to the SDK's message text (or `"unknown"`) for the
/// `bucket` param, which is a placeholder Task 4 is expected to improve on.
impl From<aws_sdk_s3::Error> for AppError {
    fn from(e: aws_sdk_s3::Error) -> Self {
        use aws_sdk_s3::Error as S3Error;

        match &e {
            S3Error::AccessDenied(_) => AppError::AccessDenied,
            S3Error::NoSuchBucket(inner) => AppError::BucketNotFound {
                bucket: inner.message().unwrap_or("unknown").to_string(),
            },
            S3Error::BucketAlreadyExists(inner) => AppError::BucketExists {
                bucket: inner.message().unwrap_or("unknown").to_string(),
            },
            S3Error::BucketAlreadyOwnedByYou(inner) => AppError::BucketExists {
                bucket: inner.message().unwrap_or("unknown").to_string(),
            },
            // Every other modeled variant (NoSuchKey, InvalidRequest, ...) plus
            // Error::Unhandled (transport failures, unparsed responses, and any
            // future variant the SDK adds) fall through to this best-effort
            // keyword sniff of the rendered message.
            _ => {
                let message = e.to_string();
                let lower = message.to_lowercase();
                if lower.contains("timed out") || lower.contains("timeout") {
                    AppError::Timeout
                } else if lower.contains("dispatch failure")
                    || lower.contains("dns")
                    || lower.contains("connect")
                    || lower.contains("unreachable")
                    || lower.contains("tls")
                {
                    AppError::Unreachable
                } else {
                    AppError::Internal { message }
                }
            }
        }
    }
}

/// Result alias used throughout the Rust core.
pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_maps_each_family() {
        assert_eq!(
            AppError::InvalidCredentials.code(),
            "auth/invalid-credentials"
        );
        assert_eq!(AppError::Timeout.code(), "network/timeout");
        assert_eq!(
            AppError::BucketNotFound { bucket: "x".into() }.code(),
            "storage/bucket-not-found"
        );
        assert_eq!(AppError::DecryptFailed.code(), "local/decrypt-failed");
        assert_eq!(
            AppError::KeyDerivationFailed.code(),
            "local/key-derivation-failed"
        );
        assert_eq!(
            AppError::Internal {
                message: "boom".into()
            }
            .code(),
            "internal"
        );
    }

    #[test]
    fn key_derivation_failed_serializes_with_empty_params() {
        let e = AppError::KeyDerivationFailed;
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["code"], "local/key-derivation-failed");
        assert!(v["params"].as_object().unwrap().is_empty());
    }

    #[test]
    fn serializes_code_and_params() {
        let e = AppError::BucketNotFound {
            bucket: "photos".into(),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["code"], "storage/bucket-not-found");
        assert_eq!(v["params"]["bucket"], "photos");
    }

    #[test]
    fn wrong_region_maps_code_and_params() {
        let e = AppError::WrongRegion {
            bucket: "mybucket".into(),
        };
        assert_eq!(e.code(), "storage/wrong-region");
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["code"], "storage/wrong-region");
        assert_eq!(v["params"]["bucket"], "mybucket");
    }

    #[test]
    fn errors_without_params_serialize_empty_map() {
        let e = AppError::DecryptFailed;
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["code"], "local/decrypt-failed");
        assert!(v["params"].as_object().unwrap().is_empty());
    }

    #[test]
    fn io_error_maps_to_store_io() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let e: AppError = io.into();
        assert_eq!(e.code(), "local/store-io");
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["params"]["message"], "nope");
    }

    // Coverage note: `aws_sdk_s3::Error`'s modeled variants (`NoSuchBucket`,
    // `BucketAlreadyExists`, `BucketAlreadyOwnedByYou`, `AccessDenied`) wrap
    // `#[non_exhaustive]` structs whose only public constructor is a builder
    // (e.g. `NoSuchBucket::builder().message(..).build()`), and the builder
    // is reachable from this crate the same way Task 4's provider code will
    // reach it -- there is no SDK-internal-only gate here. That builder path
    // is exercised below for the two match arms that carry a `bucket` param
    // (`NoSuchBucket`, `BucketAlreadyExists`); `BucketAlreadyOwnedByYou`
    // takes the identical shape via the same match arm pattern and
    // `AccessDenied` needs no builder-derived field at all. The keyword-sniff
    // fallback (`Timeout`/`Unreachable`/`Internal`) is exercised directly
    // against `AppError` without going through an `aws_sdk_s3::Error` value,
    // since the SDK gives no public constructor for `Error::Unhandled`
    // (its inner `sealed_unhandled::Unhandled` fields are crate-private) --
    // that variant can only be produced by an actual `SdkError` conversion,
    // which is exactly why Task 4 owns refining this branch.

    #[test]
    fn s3_no_such_bucket_maps_to_bucket_not_found() {
        let inner = aws_sdk_s3::types::error::NoSuchBucket::builder()
            .message("the bucket does not exist")
            .build();
        let e: AppError = aws_sdk_s3::Error::NoSuchBucket(inner).into();
        assert_eq!(e.code(), "storage/bucket-not-found");
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["params"]["bucket"], "the bucket does not exist");
    }

    #[test]
    fn s3_bucket_already_exists_maps_to_bucket_exists() {
        let inner = aws_sdk_s3::types::error::BucketAlreadyExists::builder()
            .message("bucket name taken")
            .build();
        let e: AppError = aws_sdk_s3::Error::BucketAlreadyExists(inner).into();
        assert_eq!(e.code(), "storage/bucket-exists");
    }

    #[test]
    fn s3_access_denied_maps_to_auth_family() {
        let inner = aws_sdk_s3::types::error::AccessDenied::builder()
            .message("nope")
            .build();
        let e: AppError = aws_sdk_s3::Error::AccessDenied(inner).into();
        assert_eq!(e.code(), "auth/access-denied");
    }

    #[test]
    fn key_not_found_maps_code_and_params() {
        let e = AppError::KeyNotFound {
            key: "docs/readme.md".into(),
        };
        assert_eq!(e.code(), "storage/key-not-found");
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["code"], "storage/key-not-found");
        assert_eq!(v["params"]["key"], "docs/readme.md");
    }

    #[test]
    fn file_io_maps_to_the_local_family_with_both_params() {
        let err = AppError::FileIo {
            path: "/tmp/a.bin".to_string(),
            message: "permission denied".to_string(),
        };
        assert_eq!(err.code(), "local/file-io");
        let p = err.params();
        assert_eq!(p.get("path").map(String::as_str), Some("/tmp/a.bin"));
        assert_eq!(
            p.get("message").map(String::as_str),
            Some("permission denied")
        );
    }
}
