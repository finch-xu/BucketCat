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
    /// Cloudflare API token value, for R2 connections only. **A second
    /// secret**, held to the same handling rules as `secret_access_key`.
    ///
    /// It cannot be recovered from `secret_access_key`, which is precisely
    /// why it needs its own field: R2 derives the S3 secret as
    /// `sha256(token value)` (see
    /// [`crate::provider::r2::r2_secret_from_token`]), and that hash is
    /// one-way, while `api.cloudflare.com` only accepts the token value
    /// itself as a Bearer credential. Without this field, a connection could
    /// browse objects but could never read bucket usage or public-access
    /// settings.
    ///
    /// `#[serde(default)]` is load-bearing: it is what lets connections saved
    /// by a build predating this field decrypt into `None` instead of failing
    /// the whole store's deserialization.
    #[serde(default)]
    pub api_token: Option<String>,
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
            // Redacted as a *presence* flag, not a value: printing
            // `Some("<redacted>")` vs `None` still tells a reader whether a
            // token is configured, which is the only thing a log ever needs.
            .field("api_token", &self.api_token.as_ref().map(|_| "<redacted>"))
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
    /// Cloudflare API token value for an R2 connection, or `None`/blank.
    ///
    /// Follows the same "leave blank to keep" contract as
    /// `secret_access_key` on edit. When it is supplied and
    /// `secret_access_key` is blank, the backend derives the S3 secret from
    /// it (see `commands::connection::new_connection` /
    /// [`super::ConnectionInput`]'s use in `merge_update`) -- the frontend
    /// never computes that hash, so the derived secret never exists inside
    /// the webview.
    ///
    /// `#[serde(default)]` keeps the field optional on the wire, so every
    /// non-R2 form can keep sending the payload it always sent.
    #[serde(default)]
    pub api_token: Option<String>,
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
            .field("api_token", &self.api_token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Backend -> frontend view of a [`Connection`].
///
/// Deliberately has **no** `secret_access_key` and **no** `api_token` field:
/// this is a compile-time guarantee that neither secret can ever be
/// serialized to the UI through this type, backed up by a runtime test that
/// asserts the serialized JSON contains no `"secret"` substring.
///
/// `has_api_token` is the one thing the UI does need to know about the
/// token -- whether one is stored -- so the edit form can offer the same
/// "leave blank to keep" affordance it already offers for the secret key.
/// A boolean carries exactly that and nothing more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectionDto {
    pub id: String,
    pub provider: String,
    pub name: String,
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub default_bucket: Option<String>,
    pub has_api_token: bool,
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
            has_api_token: c.api_token.is_some(),
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
            api_token: None,
        };

        let debugged = format!("{:?}", conn);

        assert!(!debugged.contains("super-secret-value"));
        assert!(debugged.contains("<redacted>"));
        // Sanity: other fields still show up normally.
        assert!(debugged.contains("AKIAEXAMPLE"));
    }

    /// The R2 API token is the second secret on this struct and gets exactly
    /// the same treatment. Without this test the field would be redacted only
    /// by convention -- and a `#[derive(Debug)]` slipping back in would print
    /// it in the clear at the first `dbg!`, panic payload or error log.
    #[test]
    fn connection_debug_redacts_api_token() {
        let conn = Connection {
            id: "c1".to_string(),
            provider: "r2".to_string(),
            name: "test r2".to_string(),
            endpoint: "https://acct.r2.cloudflarestorage.com".to_string(),
            region: "auto".to_string(),
            access_key_id: "tokenid".to_string(),
            secret_access_key: "super-secret-value".to_string(),
            default_bucket: None,
            api_token: Some("cfut_super-secret-token".to_string()),
        };

        let debugged = format!("{:?}", conn);

        assert!(
            !debugged.contains("cfut_super-secret-token"),
            "the API token must never appear in a Debug rendering: {debugged}"
        );
        assert!(debugged.contains("api_token: Some(\"<redacted>\")"));
    }

    /// A connection with no token renders as `None` rather than as a redacted
    /// placeholder, so a log still distinguishes "no token configured" from
    /// "token withheld" -- the only thing about the token a reader needs.
    #[test]
    fn connection_debug_shows_a_missing_api_token_as_none() {
        let conn = Connection {
            id: "c1".to_string(),
            provider: "r2".to_string(),
            name: "test r2".to_string(),
            endpoint: "https://acct.r2.cloudflarestorage.com".to_string(),
            region: "auto".to_string(),
            access_key_id: "tokenid".to_string(),
            secret_access_key: "s".to_string(),
            default_bucket: None,
            api_token: None,
        };

        assert!(format!("{:?}", conn).contains("api_token: None"));
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
            api_token: None,
        };

        let debugged = format!("{:?}", input);

        assert!(!debugged.contains("super-secret-value"));
        assert!(debugged.contains("<redacted>"));
        // Sanity: other fields still show up normally.
        assert!(debugged.contains("AKIAEXAMPLE"));
    }

    fn r2_connection_with_token(token: Option<&str>) -> Connection {
        Connection {
            id: "c1".to_string(),
            provider: "r2".to_string(),
            name: "test r2".to_string(),
            endpoint: "https://acct.r2.cloudflarestorage.com".to_string(),
            region: "auto".to_string(),
            access_key_id: "tokenid".to_string(),
            secret_access_key: "super-secret-value".to_string(),
            default_bucket: None,
            api_token: token.map(str::to_string),
        }
    }

    /// The DTO is the *only* connection shape that crosses into the webview,
    /// so this is the assertion that actually protects the token: neither
    /// secret may appear anywhere in its serialized form, by value or by
    /// field name.
    #[test]
    fn connection_dto_serializes_neither_secret() {
        let conn = r2_connection_with_token(Some("cfut_super-secret-token"));
        let json = serde_json::to_string(&ConnectionDto::from(&conn)).unwrap();

        assert!(
            !json.contains("cfut_super-secret-token"),
            "leaked token: {json}"
        );
        assert!(
            !json.contains("super-secret-value"),
            "leaked secret: {json}"
        );
        assert!(
            !json.contains("secret"),
            "leaked a secret field name: {json}"
        );
        // The *key* `"api_token"`, not the substring: `has_api_token` is a
        // legitimate field whose name contains it, and that boolean is exactly
        // what the UI is allowed to see.
        assert!(
            !json.contains(r#""api_token""#),
            "leaked the token field: {json}"
        );
    }

    /// `has_api_token` is the whole reason the DTO knows about the token at
    /// all -- it drives the edit form's "leave blank to keep" affordance, so
    /// it has to track presence in both directions.
    #[test]
    fn connection_dto_reports_whether_a_token_is_stored() {
        assert!(ConnectionDto::from(&r2_connection_with_token(Some("cfut_x"))).has_api_token);
        assert!(!ConnectionDto::from(&r2_connection_with_token(None)).has_api_token);
    }

    /// The backward-compatibility guarantee for the encrypted store: every
    /// connection saved before `api_token` existed has no such key in its
    /// JSON, and must still load. Without `#[serde(default)]` this would fail
    /// the *entire* store's deserialization, losing every saved connection at
    /// once -- which is why it gets a test rather than a comment.
    #[test]
    fn connection_without_an_api_token_key_still_deserializes() {
        let legacy = r#"{
            "id": "c1",
            "provider": "s3",
            "name": "legacy",
            "endpoint": "https://s3.amazonaws.com",
            "region": "us-east-1",
            "access_key_id": "AKIAEXAMPLE",
            "secret_access_key": "s",
            "default_bucket": null
        }"#;

        let conn: Connection = serde_json::from_str(legacy)
            .expect("a connection saved before `api_token` existed must still load");
        assert_eq!(conn.api_token, None);
        assert_eq!(conn.id, "c1");
    }

    /// The same guarantee for the IPC payload: every non-R2 form keeps
    /// sending the object it always sent, with no `api_token` key at all.
    #[test]
    fn connection_input_without_an_api_token_key_still_deserializes() {
        let legacy = r#"{
            "provider": "s3",
            "name": "legacy",
            "endpoint": "https://s3.amazonaws.com",
            "region": "us-east-1",
            "access_key_id": "AKIAEXAMPLE",
            "secret_access_key": "s",
            "default_bucket": null
        }"#;

        let input: ConnectionInput = serde_json::from_str(legacy)
            .expect("a payload from a form that doesn't know about `api_token` must still parse");
        assert_eq!(input.api_token, None);
    }
}
