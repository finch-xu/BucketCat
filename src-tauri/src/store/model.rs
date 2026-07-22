//! Connection domain model.
//!
//! Three shapes exist on purpose:
//! - [`Connection`] is the full record (including the secret access key)
//!   that lives inside the encrypted store and is used by Rust-side
//!   provider/command logic.
//! - [`ConnectionInput`] is what the frontend sends when creating a
//!   connection: the same fields minus `id`, which the backend assigns.
//! - [`ConnectionDto`] is what the backend ever sends back to the
//!   frontend: `Connection` minus `secret_access_key`. The field is
//!   omitted from the struct definition itself, not just skipped during
//!   serialization, so leaking the secret to the UI is a compile error
//!   rather than a runtime bug.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A saved connection profile, including its secret access key.
///
/// This type must never be serialized to the frontend; use
/// [`ConnectionDto`] (via `From<&Connection>`) for anything that crosses
/// the Tauri command boundary.
///
/// `Debug` is hand-written (not derived) so that logging a `Connection` --
/// e.g. in an error message, a `dbg!()`, or a panic payload -- can never
/// print `secret_access_key` in the clear; see the manual `impl` below.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub provider: String,
    pub name: String,
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub default_bucket: Option<String>,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("default_bucket", &self.default_bucket)
            .finish()
    }
}

/// Frontend -> backend payload for creating a connection. Has no `id`;
/// the backend generates one when persisting the new [`Connection`].
///
/// `Debug` is hand-written (not derived), mirroring [`Connection`]'s manual
/// `impl`, so that logging a `ConnectionInput` can never print
/// `secret_access_key` in the clear.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct ConnectionInput {
    pub provider: String,
    pub name: String,
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub default_bucket: Option<String>,
}

impl fmt::Debug for ConnectionInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionInput")
            .field("provider", &self.provider)
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("default_bucket", &self.default_bucket)
            .finish()
    }
}

/// Backend -> frontend view of a [`Connection`].
///
/// Deliberately has **no** `secret_access_key` field: this is a
/// compile-time guarantee that the secret can never be serialized to the
/// UI through this type, backed up by a runtime test that asserts the
/// serialized JSON contains no `"secret"` substring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectionDto {
    pub id: String,
    pub provider: String,
    pub name: String,
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub default_bucket: Option<String>,
}

impl From<&Connection> for ConnectionDto {
    fn from(c: &Connection) -> Self {
        ConnectionDto {
            id: c.id.clone(),
            provider: c.provider.clone(),
            name: c.name.clone(),
            endpoint: c.endpoint.clone(),
            region: c.region.clone(),
            access_key_id: c.access_key_id.clone(),
            default_bucket: c.default_bucket.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_debug_redacts_secret_access_key() {
        let conn = Connection {
            id: "c1".to_string(),
            provider: "aws".to_string(),
            name: "test bucket".to_string(),
            endpoint: "https://s3.amazonaws.com".to_string(),
            region: "us-east-1".to_string(),
            access_key_id: "AKIAEXAMPLE".to_string(),
            secret_access_key: "super-secret-value".to_string(),
            default_bucket: Some("my-bucket".to_string()),
        };

        let debugged = format!("{:?}", conn);

        assert!(!debugged.contains("super-secret-value"));
        assert!(debugged.contains("<redacted>"));
        // Sanity: other fields still show up normally.
        assert!(debugged.contains("AKIAEXAMPLE"));
    }

    #[test]
    fn connection_input_debug_redacts_secret_access_key() {
        let input = ConnectionInput {
            provider: "aws".to_string(),
            name: "test bucket".to_string(),
            endpoint: "https://s3.amazonaws.com".to_string(),
            region: "us-east-1".to_string(),
            access_key_id: "AKIAEXAMPLE".to_string(),
            secret_access_key: "super-secret-value".to_string(),
            default_bucket: Some("my-bucket".to_string()),
        };

        let debugged = format!("{:?}", input);

        assert!(!debugged.contains("super-secret-value"));
        assert!(debugged.contains("<redacted>"));
        // Sanity: other fields still show up normally.
        assert!(debugged.contains("AKIAEXAMPLE"));
    }
}
