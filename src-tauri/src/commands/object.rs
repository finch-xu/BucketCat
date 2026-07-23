//! Object browse/mutate commands: paged listing, batch delete, rename,
//! folder creation. Thin layers over [`crate::provider::Provider`] — see
//! `connection.rs`'s module docs for the state/IO model every command here
//! shares. In particular: every command below goes through
//! `state.hub().provider(&connection_id)`, so a cache hit costs zero store
//! IO and zero client rebuilding (just an `Arc` clone) — only a cache miss
//! (first use of a connection, or the first lookup after an edit
//! invalidated the cache) pays for a store load and a client build.

use tauri::State;

use crate::commands::connection::AppState;
use crate::error::AppResult;
use crate::provider::{BatchResult, ListPage, Provider};

/// Objects (files + folders combined) per `list_objects` page. One page is
/// one ListObjectsV2 request; the frontend's infinite scroll requests the
/// next page via the returned `next_token`. 200 keeps a page render-light
/// while needing only ~5 requests per 1000 objects.
const LIST_PAGE_SIZE: i32 = 200;

/// Lists one page of objects + folders under `prefix` in `bucket`.
/// `prefix` is the current path plus any prefix-search text; `token`
/// resumes a previous page's `next_token`.
#[tauri::command]
pub async fn list_objects(
    state: State<'_, AppState>,
    connection_id: String,
    bucket: String,
    prefix: String,
    token: Option<String>,
) -> AppResult<ListPage> {
    let provider = state.hub().provider(&connection_id).await?;
    provider
        .list_objects(&bucket, &prefix, token.as_deref(), LIST_PAGE_SIZE)
        .await
}

/// Batch-deletes `keys`, reporting per-key partial failures instead of
/// aborting (design §7). An empty `keys` short-circuits to an empty
/// success without touching the store or network.
#[tauri::command]
pub async fn delete_objects(
    state: State<'_, AppState>,
    connection_id: String,
    bucket: String,
    keys: Vec<String>,
) -> AppResult<BatchResult> {
    if keys.is_empty() {
        return Ok(BatchResult {
            succeeded: 0,
            failed: Vec::new(),
        });
    }
    let provider = state.hub().provider(&connection_id).await?;
    provider.delete_objects(&bucket, &keys).await
}

/// Renames one object (copy + delete under the hood).
#[tauri::command]
pub async fn rename_object(
    state: State<'_, AppState>,
    connection_id: String,
    bucket: String,
    from_key: String,
    to_key: String,
) -> AppResult<()> {
    let provider = state.hub().provider(&connection_id).await?;
    provider.rename_object(&bucket, &from_key, &to_key).await
}

/// Creates a folder (zero-byte `prefix/` marker object).
#[tauri::command]
pub async fn create_folder(
    state: State<'_, AppState>,
    connection_id: String,
    bucket: String,
    prefix: String,
) -> AppResult<()> {
    let provider = state.hub().provider(&connection_id).await?;
    provider.create_folder(&bucket, &prefix).await
}
