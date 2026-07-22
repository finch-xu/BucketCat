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

use serde::{Deserialize, Serialize};

/// A saved connection profile, including its secret access key.
///
/// This type must never be serialized to the frontend; use
/// [`ConnectionDto`] (via `From<&Connection>`) for anything that crosses
/// the Tauri command boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Frontend -> backend payload for creating a connection. Has no `id`;
/// the backend generates one when persisting the new [`Connection`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ConnectionInput {
    pub provider: String,
    pub name: String,
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub default_bucket: Option<String>,
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
