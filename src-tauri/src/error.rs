//! Unified application error type.
//!
//! Every fallible command returns [`AppResult`]. On the wire an error is
//! serialized as `{ "code": "family/kind", "params": { ... } }`. The frontend
//! looks the `code` up in its i18n dictionary and renders human-readable text;
//! raw third-party error details never reach the UI (they go to the log).

use std::collections::HashMap;

use serde::ser::{Serialize, SerializeStruct, Serializer};

/// Application-wide error. Variants are grouped into four families by the
/// prefix of their [`AppError::code`]: `auth/*`, `network/*`, `storage/*`,
/// `local/*`, plus a catch-all `internal`.
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

    #[error("connection not found: {id}")]
    ConnectionNotFound { id: String },

    #[error("local store io error: {message}")]
    StoreIo { message: String },
    #[error("failed to decrypt local store")]
    DecryptFailed,

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
            AppError::ConnectionNotFound { .. } => "storage/connection-not-found",
            AppError::StoreIo { .. } => "local/store-io",
            AppError::DecryptFailed => "local/decrypt-failed",
            AppError::Internal { .. } => "internal",
        }
    }

    /// Interpolation params for the i18n message (never contains secrets).
    pub fn params(&self) -> HashMap<String, String> {
        let mut p = HashMap::new();
        match self {
            AppError::BucketNotFound { bucket } | AppError::BucketExists { bucket } => {
                p.insert("bucket".to_string(), bucket.clone());
            }
            AppError::ConnectionNotFound { id } => {
                p.insert("id".to_string(), id.clone());
            }
            AppError::StoreIo { message } | AppError::Internal { message } => {
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
            AppError::BucketNotFound {
                bucket: "x".into()
            }
            .code(),
            "storage/bucket-not-found"
        );
        assert_eq!(AppError::DecryptFailed.code(), "local/decrypt-failed");
        assert_eq!(
            AppError::Internal {
                message: "boom".into()
            }
            .code(),
            "internal"
        );
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
}
