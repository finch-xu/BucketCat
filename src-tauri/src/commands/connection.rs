//! Connection CRUD, connectivity test, and bucket listing commands.
//!
//! ## App state / IO model
//!
//! [`AppState`] holds an `Arc<ProviderHub>` -- see `provider::hub`'s module
//! docs for the hub's own internals. Every mutating command still does a
//! full load-modify-save round trip through the hub's `store` lock (no
//! in-memory cache of the *connection list* exists, or is planned: it's
//! expected to hold at most a few dozen small records -- a saved profile is
//! a few hundred bytes of JSON before encryption -- so re-reading and
//! re-writing the whole file on every call is a sub-millisecond cost that's
//! imperceptible to a human clicking a button, and it sidesteps an entire
//! class of cache-invalidation bugs a longer-lived cache would have to get
//! right for no real benefit at this data size). What the hub *does* cache
//! is the built `aws_sdk_s3` client per connection id: a command that hits
//! that cache does zero store I/O and zero client rebuilding, only handing
//! back a clone of an `Arc`. Any successful mutation invalidates the whole
//! client cache (see `provider::hub` for why that's deliberately blunt, and
//! for the epoch guard that keeps a race from repopulating it with a client
//! built from stale credentials).
//!
//! ## Blocking IO inside async commands
//!
//! [`SecureStore::load`]/[`SecureStore::save`] are synchronous `std::fs`
//! calls. `ProviderHub::connections`/`mutate` call them directly (holding
//! only the hub's `store` lock, never across a network `.await`), and this
//! module's `async fn` command bodies call *those* directly in turn --
//! neither layer routes the file IO through `tokio::task::spawn_blocking`.
//! That's a deliberate choice, not an oversight: the store file is tiny (see above),
//! so the *actual* blocking time per call is on the order of tens of
//! microseconds -- reading/writing a few hundred bytes to a local disk (or
//! OS page cache) is not in the same universe as the multi-millisecond
//! blocking calls `spawn_blocking` exists to keep off the executor. The
//! Tokio runtime here is configured `rt-multi-thread`, so even a slower disk
//! stalls only the one worker thread handling this particular command
//! invocation, not the whole runtime, and BucketCat is a single-user desktop
//! app issuing at most a handful of concurrent commands. Given that, the
//! extra `spawn_blocking` indirection (a second thread hop, a `JoinError` to
//! map into `AppError` on top of the store's own errors) would add
//! complexity without a measurable benefit; this can be revisited if the
//! store ever grows large enough for disk latency to become observable.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauri::State;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::provider::r2::r2_secret_from_token;
use crate::provider::{from_connection, Bucket, Provider, ProviderHub};
use crate::store::{Connection, ConnectionDto, ConnectionInput, SecureStore};

/// Shared, Tauri-managed application state: the [`ProviderHub`], which owns
/// both the encrypted connection store and the cache of built provider
/// clients. Held behind an `Arc` so the transfer engine -- which has no
/// access to Tauri's `State` -- can hold the same hub.
pub struct AppState {
    hub: Arc<ProviderHub>,
}

impl AppState {
    /// Ensures `config_dir` exists (creating it, and any missing parents,
    /// with owner-only `0700` permissions on unix -- see [`ensure_config_dir`])
    /// and returns an `AppState` backed by `<config_dir>/connections.enc`.
    ///
    /// This must run before the first `SecureStore::save`: `save` itself only
    /// ever creates the leaf *file* (via its own atomic tmp-then-rename
    /// write), never the containing directory, so on a fresh install with no
    /// config directory yet, saving without this step would fail with an
    /// `ENOENT`-flavored `AppError::StoreIo`.
    pub fn new(config_dir: PathBuf) -> AppResult<Self> {
        ensure_config_dir(&config_dir)?;
        let path = config_dir.join("connections.enc");
        Ok(Self {
            hub: Arc::new(ProviderHub::new(SecureStore { path })),
        })
    }

    /// The hub, for callers that live outside a `State<'_, AppState>` --
    /// notably the transfer engine, wired up in `lib.rs`'s `setup`.
    pub fn hub(&self) -> Arc<ProviderHub> {
        Arc::clone(&self.hub)
    }
}

/// Creates `dir` (and any missing parents) if it doesn't already exist, then
/// -- on unix only -- restricts `dir` itself to owner-only `0700`
/// permissions, since it will hold the encrypted connection store.
fn ensure_config_dir(dir: &Path) -> AppResult<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Normalizes an incoming `api_token`: trimmed, with blank treated as absent.
///
/// A pasted token routinely carries a trailing newline, and an empty string
/// arriving from a form field must mean "no token" rather than "a token that
/// happens to be empty" -- the latter would make [`ConnectionDto`]'s
/// `has_api_token` claim a token exists and send an empty Bearer credential
/// to Cloudflare.
fn normalized_api_token(token: Option<String>) -> Option<String> {
    token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Builds the full [`Connection`] a fresh `add_connection` call should
/// persist: an `input`'s fields plus a freshly generated id.
///
/// `access_key_id` and `secret_access_key` are both stored trimmed, so
/// accidental leading/trailing whitespace (e.g. from a copy-paste) doesn't
/// silently become part of the stored credential -- the same rationale, and
/// the same trimming, that [`merge_update`] applies on every edit, so a
/// connection's credentials get identical treatment whether they arrive via
/// `add_connection` or `update_connection`.
///
/// **The one derivation:** when an `api_token` is supplied and
/// `secret_access_key` is blank, the secret is derived from the token as
/// `sha256(token)` (see [`r2_secret_from_token`]). That is Cloudflare R2's
/// documented relationship between the two, verified live against two real
/// tokens of different privilege tiers on 2026-07-30.
///
/// It is deliberately **not** gated on `provider == "r2"`. `api_token` is only
/// ever populated by the R2 connection form, so gating would add a failure
/// mode without preventing anything: switching an R2 connection's provider in
/// the form would silently blank its secret instead of keeping the credential
/// that still works. An explicitly typed `secret_access_key` always wins, so
/// the S3-key mode of that form is unaffected.
fn new_connection(input: ConnectionInput) -> Connection {
    let api_token = normalized_api_token(input.api_token);
    let typed_secret = input.secret_access_key.trim();
    let secret_access_key = if !typed_secret.is_empty() {
        typed_secret.to_string()
    } else {
        api_token
            .as_deref()
            .map(r2_secret_from_token)
            .unwrap_or_default()
    };

    Connection {
        id: Uuid::new_v4().to_string(),
        provider: input.provider,
        name: input.name,
        endpoint: input.endpoint,
        region: input.region,
        access_key_id: input.access_key_id.trim().to_string(),
        secret_access_key,
        default_bucket: input.default_bucket,
        api_token,
    }
}

/// Applies an `update_connection` edit: every field of `existing` is
/// replaced by `input`'s, **except** that a blank (empty or all-whitespace)
/// `input.secret_access_key` -- 留空或全空白则保留原值 -- means "leave the
/// secret unchanged" (the UI's "leave blank to keep" convention for editing
/// a connection without re-entering its secret key) rather than "set the
/// secret to that blank value". A non-blank secret is stored trimmed, and
/// `access_key_id` is always stored trimmed too (it has no "leave blank to
/// keep" affordance, so there's no blank case to special-case), so
/// accidental leading/trailing whitespace (e.g. from a copy-paste) doesn't
/// silently become part of either stored credential -- see [`new_connection`]
/// for the matching treatment on creation. `id` is always preserved from
/// `existing`, since `ConnectionInput` carries no id of its own.
pub fn merge_update(existing: &Connection, input: ConnectionInput) -> Connection {
    // The R2 API token follows the identical "leave blank to keep" contract:
    // the edit form never echoes it back, so a blank field means "unchanged",
    // not "clear it".
    let new_token = normalized_api_token(input.api_token);
    let api_token = new_token.clone().or_else(|| existing.api_token.clone());

    let typed_secret = input.secret_access_key.trim();
    let secret_access_key = if !typed_secret.is_empty() {
        // An explicitly typed secret always wins, so the form's S3-key mode
        // can still override a derived one.
        typed_secret.to_string()
    } else if let Some(token) = new_token.as_deref() {
        // A *newly pasted* token re-derives the secret. Without this, rotating
        // an R2 token through the edit form would store the new token while
        // leaving the old token's hash as the S3 secret -- the connection
        // would keep browsing (both are valid until the old token is revoked)
        // and then break later, far from the edit that caused it.
        r2_secret_from_token(token)
    } else {
        existing.secret_access_key.clone()
    };

    Connection {
        id: existing.id.clone(),
        provider: input.provider,
        name: input.name,
        endpoint: input.endpoint,
        region: input.region,
        access_key_id: input.access_key_id.trim().to_string(),
        secret_access_key,
        default_bucket: input.default_bucket,
        api_token,
    }
}

/// Lists every saved connection, as [`ConnectionDto`]s (never the secret
/// access key).
#[tauri::command]
pub async fn list_connections(state: State<'_, AppState>) -> AppResult<Vec<ConnectionDto>> {
    let connections = state.hub.connections().await?;
    Ok(connections.iter().map(ConnectionDto::from).collect())
}

/// Creates a new connection (backend-generated id) and persists it.
#[tauri::command]
pub async fn add_connection(
    state: State<'_, AppState>,
    input: ConnectionInput,
) -> AppResult<ConnectionDto> {
    state
        .hub
        .mutate(|connections| {
            let connection = new_connection(input);
            let dto = ConnectionDto::from(&connection);
            connections.push(connection);
            Ok(dto)
        })
        .await
}

/// Updates the connection with the given `id` from `input` (empty
/// `secret_access_key` keeps the existing secret; see [`merge_update`]) and
/// persists the result.
#[tauri::command]
pub async fn update_connection(
    state: State<'_, AppState>,
    id: String,
    input: ConnectionInput,
) -> AppResult<ConnectionDto> {
    state
        .hub
        .mutate(move |connections| {
            let idx = connections
                .iter()
                .position(|c| c.id == id)
                .ok_or(AppError::ConnectionNotFound { id })?;
            let updated = merge_update(&connections[idx], input);
            let dto = ConnectionDto::from(&updated);
            connections[idx] = updated;
            Ok(dto)
        })
        .await
}

/// Removes any connection with the given `id` from `connections`, in place.
/// A no-op (not an error) when no connection has that id -- factored out of
/// [`delete_connection`] so its idempotency is unit-testable without a live
/// Tauri app (like [`merge_update`], `State<'_, AppState>` has no public
/// constructor outside real command dispatch).
fn remove_by_id(connections: &mut Vec<Connection>, id: &str) {
    connections.retain(|c| c.id != id);
}

/// Deletes the connection with the given `id` and persists the result.
/// Idempotent: deleting an `id` that doesn't exist (already deleted, never
/// existed, ...) is still `Ok(())` rather than an error -- the caller's
/// postcondition ("this id is not in the store") already holds either way.
#[tauri::command]
pub async fn delete_connection(state: State<'_, AppState>, id: String) -> AppResult<()> {
    state
        .hub
        .mutate(move |connections| {
            remove_by_id(connections, &id);
            Ok(())
        })
        .await
}

/// Tests connectivity for an as-yet-unsaved connection profile: builds a
/// throwaway [`Connection`] from `input` (never persisted) and runs the
/// provider's cheapest connectivity check against it.
#[tauri::command]
pub async fn test_connection(input: ConnectionInput) -> AppResult<()> {
    let connection = new_connection(input);
    let provider = from_connection(&connection)?;
    provider.test_connection().await
}

/// Lists every bucket visible to the saved connection `connection_id`'s
/// credentials. The hub hands back a cached client, so repeat calls reuse the
/// same connection pool instead of rebuilding one per invocation.
#[tauri::command]
pub async fn list_buckets(
    state: State<'_, AppState>,
    connection_id: String,
) -> AppResult<Vec<Bucket>> {
    let provider = state.hub.provider(&connection_id).await?;
    provider.list_buckets().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_connection(id: &str, secret: &str) -> Connection {
        Connection {
            id: id.to_string(),
            provider: "aws".to_string(),
            name: "test bucket".to_string(),
            endpoint: "https://s3.amazonaws.com".to_string(),
            region: "us-east-1".to_string(),
            access_key_id: "AKIAEXAMPLE".to_string(),
            secret_access_key: secret.to_string(),
            default_bucket: Some("my-bucket".to_string()),
            api_token: None,
        }
    }

    fn sample_input(secret: &str) -> ConnectionInput {
        ConnectionInput {
            provider: "aws".to_string(),
            name: "renamed bucket".to_string(),
            endpoint: "https://s3.us-west-2.amazonaws.com".to_string(),
            region: "us-west-2".to_string(),
            access_key_id: "AKIANEW".to_string(),
            secret_access_key: secret.to_string(),
            default_bucket: None,
            api_token: None,
        }
    }

    // --- id generation -------------------------------------------------

    #[test]
    fn new_connection_generates_unique_ids() {
        let a = new_connection(sample_input("s1"));
        let b = new_connection(sample_input("s2"));

        assert_ne!(a.id, b.id);
        assert!(!a.id.is_empty());
        assert!(!b.id.is_empty());
    }

    #[test]
    fn new_connection_carries_over_input_fields() {
        let conn = new_connection(sample_input("s1"));

        assert_eq!(conn.provider, "aws");
        assert_eq!(conn.name, "renamed bucket");
        assert_eq!(conn.secret_access_key, "s1");
        assert_eq!(conn.default_bucket, None);
    }

    #[test]
    fn new_connection_trims_a_padded_secret() {
        let conn = new_connection(sample_input("  new-secret  "));

        assert_eq!(conn.secret_access_key, "new-secret");
    }

    #[test]
    fn new_connection_trims_a_padded_access_key_id() {
        let mut input = sample_input("s1");
        input.access_key_id = "  AKIANEW  ".to_string();

        let conn = new_connection(input);

        assert_eq!(conn.access_key_id, "AKIANEW");
    }

    // --- merge_update: the "leave blank to keep secret" contract --------

    #[test]
    fn merge_update_keeps_existing_secret_when_input_secret_is_empty() {
        let existing = sample_connection("c1", "original-secret");
        let input = sample_input("");

        let updated = merge_update(&existing, input);

        assert_eq!(updated.secret_access_key, "original-secret");
    }

    #[test]
    fn merge_update_replaces_secret_when_input_secret_is_non_empty() {
        let existing = sample_connection("c1", "original-secret");
        let input = sample_input("new-secret");

        let updated = merge_update(&existing, input);

        assert_eq!(updated.secret_access_key, "new-secret");
    }

    #[test]
    fn merge_update_keeps_existing_secret_when_input_secret_is_whitespace_only() {
        let existing = sample_connection("c1", "original-secret");
        let input = sample_input("   ");

        let updated = merge_update(&existing, input);

        assert_eq!(updated.secret_access_key, "original-secret");
    }

    #[test]
    fn merge_update_trims_a_padded_non_blank_secret() {
        let existing = sample_connection("c1", "original-secret");
        let input = sample_input("  new-secret  ");

        let updated = merge_update(&existing, input);

        assert_eq!(updated.secret_access_key, "new-secret");
    }

    #[test]
    fn merge_update_trims_a_padded_access_key_id() {
        let existing = sample_connection("c1", "original-secret");
        let mut input = sample_input("new-secret");
        input.access_key_id = "  AKIANEW  ".to_string();

        let updated = merge_update(&existing, input);

        assert_eq!(updated.access_key_id, "AKIANEW");
    }

    #[test]
    fn merge_update_preserves_existing_id() {
        let existing = sample_connection("c1", "original-secret");
        let input = sample_input("new-secret");

        let updated = merge_update(&existing, input);

        assert_eq!(updated.id, "c1");
    }

    #[test]
    fn merge_update_applies_other_input_fields() {
        let existing = sample_connection("c1", "original-secret");
        let input = sample_input("new-secret");

        let updated = merge_update(&existing, input);

        assert_eq!(updated.name, "renamed bucket");
        assert_eq!(updated.endpoint, "https://s3.us-west-2.amazonaws.com");
        assert_eq!(updated.region, "us-west-2");
        assert_eq!(updated.access_key_id, "AKIANEW");
        assert_eq!(updated.default_bucket, None);
    }

    // --- the R2 API token: storage, normalization, secret derivation -----
    //
    // These pin the relationship Cloudflare defines between an R2 API token
    // and the S3 credentials it projects into:
    //   Access Key ID     = the token's id (only the API can report it)
    //   Secret Access Key = sha256(token value)   <- derived here
    //   Bearer credential = the token value       <- stored as `api_token`
    // Verified live against two real tokens of different privilege tiers on
    // 2026-07-30.

    fn sample_input_with_token(secret: &str, token: Option<&str>) -> ConnectionInput {
        ConnectionInput {
            api_token: token.map(str::to_string),
            ..sample_input(secret)
        }
    }

    /// The headline behavior: pasting a token is enough, because the secret
    /// is derived from it rather than typed.
    #[test]
    fn new_connection_derives_the_secret_from_an_api_token() {
        let conn = new_connection(sample_input_with_token("", Some("cfut_token")));

        assert_eq!(conn.api_token.as_deref(), Some("cfut_token"));
        assert_eq!(conn.secret_access_key, r2_secret_from_token("cfut_token"));
        assert_eq!(
            conn.secret_access_key.len(),
            64,
            "a derived secret is SHA-256 hex"
        );
    }

    /// The S3-key mode of the same form: an explicitly typed secret is never
    /// overwritten by a derivation, even when a token is present too.
    #[test]
    fn new_connection_prefers_an_explicitly_typed_secret_over_the_derivation() {
        let conn = new_connection(sample_input_with_token("typed-secret", Some("cfut_token")));

        assert_eq!(conn.secret_access_key, "typed-secret");
        assert_eq!(conn.api_token.as_deref(), Some("cfut_token"));
    }

    /// A pasted token routinely carries a trailing newline. Storing it would
    /// send a malformed Bearer credential; hashing it would derive a secret
    /// that silently fails to sign.
    #[test]
    fn new_connection_trims_a_padded_api_token() {
        let conn = new_connection(sample_input_with_token("", Some("  cfut_token\n")));

        assert_eq!(conn.api_token.as_deref(), Some("cfut_token"));
        assert_eq!(conn.secret_access_key, r2_secret_from_token("cfut_token"));
    }

    /// A blank field from a form means "no token", not "an empty token" --
    /// otherwise `ConnectionDto::has_api_token` would claim one exists and the
    /// bucket-info panel would send an empty Bearer credential to Cloudflare.
    #[test]
    fn new_connection_treats_a_blank_api_token_as_absent() {
        for blank in ["", "   ", "\n"] {
            let conn = new_connection(sample_input_with_token("s", Some(blank)));
            assert_eq!(
                conn.api_token, None,
                "blank token {blank:?} must be dropped"
            );
        }
    }

    /// Every non-R2 connection is completely unaffected: no token in, no
    /// token stored, secret handled exactly as before.
    #[test]
    fn new_connection_leaves_a_tokenless_input_untouched() {
        let conn = new_connection(sample_input("plain-secret"));

        assert_eq!(conn.api_token, None);
        assert_eq!(conn.secret_access_key, "plain-secret");
    }

    /// The token gets the same "leave blank to keep" affordance as the secret
    /// key -- the edit form never echoes it back, so a blank field means
    /// unchanged, not cleared.
    #[test]
    fn merge_update_keeps_an_existing_token_when_the_input_token_is_blank() {
        let existing = Connection {
            api_token: Some("cfut_existing".to_string()),
            ..sample_connection("c1", "original-secret")
        };

        let updated = merge_update(&existing, sample_input_with_token("", None));

        assert_eq!(updated.api_token.as_deref(), Some("cfut_existing"));
        assert_eq!(updated.secret_access_key, "original-secret");
    }

    /// Token rotation. Without re-deriving here, the connection would store
    /// the *new* token alongside the *old* token's hash as its S3 secret --
    /// and keep working until the old token was revoked, breaking long after
    /// the edit that caused it.
    #[test]
    fn merge_update_rederives_the_secret_when_a_new_token_is_pasted() {
        let existing = Connection {
            api_token: Some("cfut_old".to_string()),
            ..sample_connection("c1", r2_secret_from_token("cfut_old").as_str())
        };

        let updated = merge_update(&existing, sample_input_with_token("", Some("cfut_new")));

        assert_eq!(updated.api_token.as_deref(), Some("cfut_new"));
        assert_eq!(updated.secret_access_key, r2_secret_from_token("cfut_new"));
    }

    #[test]
    fn merge_update_prefers_an_explicitly_typed_secret_over_a_new_token() {
        let existing = sample_connection("c1", "original-secret");

        let updated = merge_update(
            &existing,
            sample_input_with_token("typed-secret", Some("cfut_new")),
        );

        assert_eq!(updated.secret_access_key, "typed-secret");
        assert_eq!(updated.api_token.as_deref(), Some("cfut_new"));
    }

    // --- remove_by_id: delete_connection's idempotency contract ---------

    #[test]
    fn remove_by_id_removes_the_matching_connection() {
        let mut connections = vec![sample_connection("c1", "s1"), sample_connection("c2", "s2")];

        remove_by_id(&mut connections, "c1");

        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].id, "c2");
    }

    #[test]
    fn remove_by_id_is_a_no_op_for_an_absent_id() {
        let mut connections = vec![sample_connection("c1", "s1"), sample_connection("c2", "s2")];
        let before = connections.clone();

        // Mirrors delete_connection's contract: deleting an id that isn't
        // in the store doesn't error or change anything -- the command
        // still returns Ok(()) around this call.
        remove_by_id(&mut connections, "does-not-exist");

        assert_eq!(connections, before);
    }

    // --- ensure_config_dir -----------------------------------------------

    #[test]
    fn ensure_config_dir_creates_missing_nested_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("nested").join("config");

        ensure_config_dir(&target).expect("should create nested dir");

        assert!(target.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_config_dir_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("config");

        ensure_config_dir(&target).expect("should create dir");

        let mode = fs::metadata(&target)
            .expect("dir must exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "config dir must be owner-only (0700)");
    }
}
