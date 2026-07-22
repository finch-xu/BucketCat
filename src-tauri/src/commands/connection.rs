//! Connection CRUD, connectivity test, and bucket listing commands.
//!
//! ## App state / IO model
//!
//! [`AppState`] wraps the single [`SecureStore`] behind a `tokio::sync::Mutex`
//! and holds no in-memory cache of the connection list: every mutating
//! command does a full load-modify-save round trip, and every read-only
//! command does a full load. This is a deliberate simplicity-over-caching
//! choice -- the connection list is expected to hold at most a few dozen
//! small records (a saved profile is a few hundred bytes of JSON before
//! encryption), so re-reading and re-writing the whole file on every call is
//! a sub-millisecond cost that's imperceptible to a human clicking a button,
//! and it sidesteps an entire class of cache-invalidation bugs (stale state
//! across two windows, a crash between "mutate in memory" and "persist",
//! ...) that a longer-lived cache would have to get right for no real
//! benefit at this data size.
//!
//! ## Blocking IO inside async commands
//!
//! [`SecureStore::load`]/[`SecureStore::save`] are synchronous `std::fs`
//! calls; this module calls them directly from `async fn` command bodies
//! instead of routing them through `tokio::task::spawn_blocking`. That's a
//! deliberate choice, not an oversight: the store file is tiny (see above),
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

use tauri::State;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::provider::{from_connection, Bucket, Provider};
use crate::store::{Connection, ConnectionDto, ConnectionInput, SecureStore};

/// Shared, Tauri-managed application state: just the encrypted connection
/// store, guarded by an async mutex so concurrent command invocations
/// serialize their load-modify-save cycles instead of racing each other.
pub struct AppState {
    store: Mutex<SecureStore>,
}

impl AppState {
    /// Ensures `config_dir` exists (creating it, and any missing parents,
    /// with owner-only `0700` permissions on unix -- see [`ensure_config_dir`])
    /// and returns an `AppState` backed by `<config_dir>/connections.enc`.
    ///
    /// This must run before the first [`SecureStore::save`]: `save` itself
    /// only ever creates the leaf *file* (via its own atomic tmp-then-rename
    /// write), never the containing directory, so on a fresh install with no
    /// config directory yet, saving without this step would fail with an
    /// `ENOENT`-flavored `AppError::StoreIo`.
    pub fn new(config_dir: PathBuf) -> AppResult<Self> {
        ensure_config_dir(&config_dir)?;
        let path = config_dir.join("connections.enc");
        Ok(Self {
            store: Mutex::new(SecureStore { path }),
        })
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

/// Builds the full [`Connection`] a fresh `add_connection` call should
/// persist: an `input`'s fields plus a freshly generated id.
///
/// `access_key_id` and `secret_access_key` are both stored trimmed, so
/// accidental leading/trailing whitespace (e.g. from a copy-paste) doesn't
/// silently become part of the stored credential -- the same rationale, and
/// the same trimming, that [`merge_update`] applies on every edit, so a
/// connection's credentials get identical treatment whether they arrive via
/// `add_connection` or `update_connection`.
fn new_connection(input: ConnectionInput) -> Connection {
    Connection {
        id: Uuid::new_v4().to_string(),
        provider: input.provider,
        name: input.name,
        endpoint: input.endpoint,
        region: input.region,
        access_key_id: input.access_key_id.trim().to_string(),
        secret_access_key: input.secret_access_key.trim().to_string(),
        default_bucket: input.default_bucket,
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
    let secret_access_key = if input.secret_access_key.trim().is_empty() {
        existing.secret_access_key.clone()
    } else {
        input.secret_access_key.trim().to_string()
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
    }
}

/// Lists every saved connection, as [`ConnectionDto`]s (never the secret
/// access key).
#[tauri::command]
pub async fn list_connections(state: State<'_, AppState>) -> AppResult<Vec<ConnectionDto>> {
    let store = state.store.lock().await;
    let connections = store.load()?;
    Ok(connections.iter().map(ConnectionDto::from).collect())
}

/// Creates a new connection (backend-generated id) and persists it.
#[tauri::command]
pub async fn add_connection(
    state: State<'_, AppState>,
    input: ConnectionInput,
) -> AppResult<ConnectionDto> {
    let store = state.store.lock().await;
    let mut connections = store.load()?;

    let connection = new_connection(input);
    let dto = ConnectionDto::from(&connection);
    connections.push(connection);

    store.save(&connections)?;
    Ok(dto)
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
    let store = state.store.lock().await;
    let mut connections = store.load()?;

    let idx = connections
        .iter()
        .position(|c| c.id == id)
        .ok_or(AppError::ConnectionNotFound { id })?;

    let updated = merge_update(&connections[idx], input);
    let dto = ConnectionDto::from(&updated);
    connections[idx] = updated;

    store.save(&connections)?;
    Ok(dto)
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
    let store = state.store.lock().await;
    let mut connections = store.load()?;

    remove_by_id(&mut connections, &id);

    store.save(&connections)?;
    Ok(())
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
/// credentials.
///
/// The store lock is scoped to just the `load()` -- it's released before
/// the network-bound `provider.list_buckets().await`, so a slow/hanging S3
/// call never blocks other commands (e.g. `list_connections` from a second
/// window) out of the store for its duration.
#[tauri::command]
pub async fn list_buckets(
    state: State<'_, AppState>,
    connection_id: String,
) -> AppResult<Vec<Bucket>> {
    let connections = {
        let store = state.store.lock().await;
        store.load()?
    };

    let connection = connections
        .iter()
        .find(|c| c.id == connection_id)
        .ok_or(AppError::ConnectionNotFound { id: connection_id })?;

    let provider = from_connection(connection)?;
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
