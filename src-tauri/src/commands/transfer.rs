//! Transfer commands. Thin, like every other command module: parse, delegate
//! to [`TransferEngine`], return a DTO.

use std::path::PathBuf;

use tauri::State;

use crate::error::AppResult;
use crate::transfer::{EnqueueSpec, TransferEngine, TransferTaskDto};

/// Object key for a file uploaded into `prefix`.
///
/// The remote key is built here, not on the frontend: only the last path
/// segment of `file_name` is kept, so a value carrying separators (however it
/// got there, since it arrives over IPC) cannot redirect the upload out of
/// the browsed prefix. The basename is then trimmed, to match
/// `src/lib/entries.ts`'s `uploadKey` -- the client-side copy used for the
/// duplicate-name guard -- character-for-character. Both sides must agree
/// exactly: the frontend decides "would this overwrite something?" from its
/// own copy *before* enqueueing, so a divergence here would make the guard
/// check one key while the backend silently, irreversibly overwrites
/// another.
pub fn upload_key(prefix: &str, file_name: &str) -> String {
    let base = file_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(file_name)
        .trim();
    if prefix.is_empty() {
        return base.to_string();
    }
    if prefix.ends_with('/') {
        format!("{prefix}{base}")
    } else {
        format!("{prefix}/{base}")
    }
}

/// Queues one upload task per local path. Paths that cannot be stat-ed (gone,
/// unreadable, a directory) are rejected **individually**: one bad path in a
/// multi-file drop must not discard the rest (design §7's partial-failure
/// rule). Rejected paths are logged and simply produce no task.
#[tauri::command]
pub async fn enqueue_uploads(
    engine: State<'_, TransferEngine>,
    connection_id: String,
    bucket: String,
    prefix: String,
    paths: Vec<String>,
) -> AppResult<Vec<TransferTaskDto>> {
    let mut queued = Vec::with_capacity(paths.len());
    for path in paths {
        let local = PathBuf::from(&path);
        let file_name = local
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let key = upload_key(&prefix, &file_name);

        match EnqueueSpec::for_upload(connection_id.clone(), bucket.clone(), key, local) {
            Ok(spec) => queued.push(engine.enqueue(spec).await?),
            Err(err) => tracing::warn!(path = %path, "skipping unusable upload path: {err}"),
        }
    }
    Ok(queued)
}

/// Every task the engine knows about, newest first. Called once when the
/// panel mounts; live updates arrive on `transfer://state` afterwards.
#[tauri::command]
pub async fn list_transfers(engine: State<'_, TransferEngine>) -> AppResult<Vec<TransferTaskDto>> {
    Ok(engine.snapshot().await)
}

#[tauri::command]
pub async fn pause_transfer(engine: State<'_, TransferEngine>, task_id: String) -> AppResult<()> {
    engine.pause(&task_id).await
}

#[tauri::command]
pub async fn resume_transfer(engine: State<'_, TransferEngine>, task_id: String) -> AppResult<()> {
    engine.resume(&task_id).await
}

#[tauri::command]
pub async fn cancel_transfer(engine: State<'_, TransferEngine>, task_id: String) -> AppResult<()> {
    engine.cancel(&task_id).await
}

#[tauri::command]
pub async fn retry_transfer(engine: State<'_, TransferEngine>, task_id: String) -> AppResult<()> {
    engine.retry(&task_id).await
}

#[tauri::command]
pub async fn clear_finished_transfers(engine: State<'_, TransferEngine>) -> AppResult<()> {
    engine.clear_finished().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_key_joins_prefix_and_name() {
        assert_eq!(upload_key("docs/", "a.txt"), "docs/a.txt");
        assert_eq!(upload_key("docs/sub/", "a.txt"), "docs/sub/a.txt");
    }

    #[test]
    fn bucket_root_needs_no_separator() {
        assert_eq!(upload_key("", "a.txt"), "a.txt");
    }

    #[test]
    fn a_missing_trailing_slash_is_added() {
        // Defence in depth: `pathToPrefix` always ends with "/", but a
        // hand-built prefix must not silently produce "docsa.txt".
        assert_eq!(upload_key("docs", "a.txt"), "docs/a.txt");
    }

    #[test]
    fn path_separators_in_the_name_are_not_allowed_to_escape() {
        // A file name can never contain "/" on a real filesystem, but the
        // value arrives over IPC, so it must not be able to redirect the
        // upload to another prefix.
        assert_eq!(upload_key("docs/", "../secrets/a.txt"), "docs/a.txt");
        assert_eq!(upload_key("docs/", "sub/a.txt"), "docs/a.txt");
    }

    #[test]
    fn windows_style_separators_are_also_reduced_to_the_basename() {
        assert_eq!(upload_key("docs/", "sub\\a.txt"), "docs/a.txt");
        assert_eq!(upload_key("docs/", "C:\\Users\\me\\a.txt"), "docs/a.txt");
    }

    #[test]
    fn trailing_and_leading_whitespace_on_the_basename_is_trimmed() {
        // Mirrors `src/lib/entries.ts`'s `uploadKey` as it stands today: the
        // basename is `.trim()`-ed *after* the last path segment is taken, so
        // a name like "a.txt " (e.g. from a source filesystem that allows
        // trailing whitespace) lands on the key without it.
        assert_eq!(upload_key("docs/", "a.txt "), "docs/a.txt");
        assert_eq!(upload_key("docs/", " a.txt"), "docs/a.txt");
        assert_eq!(upload_key("docs/", "  a.txt  "), "docs/a.txt");
        assert_eq!(upload_key("", "  a.txt  "), "a.txt");
        // Whitespace-only path segments before the last "/" are irrelevant --
        // only the final segment is trimmed.
        assert_eq!(upload_key("docs/", "sub/ a.txt "), "docs/a.txt");
    }
}
