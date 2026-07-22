//! Object browse/mutate commands: paged listing, batch delete, rename,
//! folder creation. Thin layers over [`crate::provider::Provider`] — see
//! `connection.rs`'s module docs for the state/IO model every command here
//! shares (full store load per call, lock scoped away from network IO).

use tauri::State;

use crate::commands::connection::AppState;
use crate::error::{AppError, AppResult};
use crate::provider::{from_connection, BatchResult, ListPage, Provider};
use crate::store::Connection;

/// Objects (files + folders combined) per `list_objects` page. One page is
/// one ListObjectsV2 request; the frontend's infinite scroll requests the
/// next page via the returned `next_token`. 200 keeps a page render-light
/// while needing only ~5 requests per 1000 objects.
const LIST_PAGE_SIZE: i32 = 200;

/// Finds the saved connection with `connection_id`, or fails with
/// `storage/connection-not-found`.
async fn connection_by_id(
    state: &State<'_, AppState>,
    connection_id: String,
) -> AppResult<Connection> {
    let connections = state.load_connections().await?;
    connections
        .into_iter()
        .find(|c| c.id == connection_id)
        .ok_or(AppError::ConnectionNotFound { id: connection_id })
}

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
    let connection = connection_by_id(&state, connection_id).await?;
    let provider = from_connection(&connection)?;
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
    let connection = connection_by_id(&state, connection_id).await?;
    let provider = from_connection(&connection)?;
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
    let connection = connection_by_id(&state, connection_id).await?;
    let provider = from_connection(&connection)?;
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
    let connection = connection_by_id(&state, connection_id).await?;
    let provider = from_connection(&connection)?;
    provider.create_folder(&bucket, &prefix).await
}
