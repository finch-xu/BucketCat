//! Backblaze B2 commands: the connection form's key probe.
//!
//! Kept out of [`super::connection`] for the same reason [`super::r2`] is: a
//! separate concern with a separate backend ([`crate::provider::b2_admin`],
//! which talks to `api.backblazeb2.com` rather than to any S3 endpoint), and
//! `connection.rs` is already the largest command module.
//!
//! **Security**: [`b2_probe_key`] takes a live credential pair as command
//! arguments, because the user has just typed them into the form and nothing
//! is saved yet. They are used for exactly one round trip and never logged,
//! echoed back, or persisted by this command -- persisting them is
//! `add_connection`'s job, through the encrypted store.

use crate::error::AppResult;
use crate::provider::b2_admin::{self, B2KeyProbe};

/// Probes a B2 `(keyID, applicationKey)` pair the user has just pasted into
/// the connection form, **before** any connection exists to save it against.
///
/// Returns the account's authoritative S3 region and endpoint, straight from
/// Backblaze's `s3ApiUrl`. The form has already guessed both offline from the
/// key id's cluster prefix (see [`crate::provider::b2::b2_region_from_key_id`],
/// a convention Backblaze does not document); this is what confirms or
/// corrects that guess, and what lets a region Backblaze launches after this
/// build shipped work without an app update.
///
/// `allowed_buckets` coming back **empty is a success**, not a failure: it is
/// what an unrestricted key reports (`allowed.buckets: null`, verified live on
/// 2026-07-30). Only a non-empty list means the key is scoped to particular
/// buckets, which the form surfaces so a short bucket list later isn't a
/// mystery.
#[tauri::command]
pub async fn b2_probe_key(key_id: String, application_key: String) -> AppResult<B2KeyProbe> {
    b2_admin::authorize_account(&key_id, &application_key).await
}
