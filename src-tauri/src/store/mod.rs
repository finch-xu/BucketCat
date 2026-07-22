//! Local encrypted connection store.
//!
//! Persists saved connection profiles (endpoint, credentials, region, ...)
//! to disk, encrypted at rest. Encryption key material is derived via HKDF
//! from a machine-bound secret (see the `hkdf`, `sha2`, `aes-gcm` and
//! `machine-uid` dependencies) so the store is unreadable outside the
//! machine that created it.

pub mod model;
pub mod secure;

pub use model::{Connection, ConnectionDto, ConnectionInput};
pub use secure::{derive_key, SecureStore};
