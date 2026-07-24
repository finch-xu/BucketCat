//! Transfer commands. Thin, like every other command module: parse, delegate
//! to [`TransferEngine`], return a DTO.

use std::path::PathBuf;

use tauri::State;

use crate::error::AppResult;
use crate::transfer::{EnqueueSpec, TransferEngine, TransferTaskDto};

/// JavaScript's `String.prototype.trim` semantics, which are NOT Rust's
/// `str::trim`: ECMAScript strips Unicode `White_Space` *minus* U+0085
/// (NEL) *plus* U+FEFF (BOM), while Rust's `char::is_whitespace` includes
/// U+0085 and excludes U+FEFF.
///
/// This must match `uploadBaseName` in `src/lib/entries.ts` character for
/// character. The frontend uses its copy to decide whether an upload would
/// overwrite an existing object; if the two disagree, the guard clears one
/// key while this function writes another, and the overwrite is silent and
/// irreversible.
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

/// Object key for a file uploaded into `prefix`.
///
/// The remote key is built here, not on the frontend: only the last path
/// segment of `file_name` is kept, so a value carrying separators (however it
/// got there, since it arrives over IPC) cannot redirect the upload out of
/// the browsed prefix. The basename is then trimmed with [`js_trim`] (NOT
/// `str::trim` -- see its doc comment), to match `src/lib/entries.ts`'s
/// `uploadKey` -- the client-side copy used for the duplicate-name guard --
/// character-for-character. Both sides must agree exactly: the frontend
/// decides "would this overwrite something?" from its own copy *before*
/// enqueueing, so a divergence here would make the guard check one key while
/// the backend silently, irreversibly overwrites another.
///
/// Returns `""` when nothing survives basename extraction + trim (`""`,
/// whitespace-only, or a prefix-only path like `"docs/"`) -- never `prefix`
/// itself. Returning `prefix` unchanged would name the browsed folder's own
/// marker key, which a `PutObject` could then silently overwrite. Callers
/// must treat `""` as "not uploadable", exactly like `uploadKey` in
/// `src/lib/entries.ts`.
pub fn upload_key(prefix: &str, file_name: &str) -> String {
    let base = js_trim(file_name.rsplit(['/', '\\']).next().unwrap_or(file_name));
    if base.is_empty() {
        return String::new();
    }
    if prefix.is_empty() {
        return base.to_string();
    }
    if prefix.ends_with('/') {
        format!("{prefix}{base}")
    } else {
        format!("{prefix}/{base}")
    }
}

/// Resolves one local path into an [`EnqueueSpec`], or `None` (logging a
/// `warn!`) if the path is unusable. Two independent reasons produce `None`:
///
/// - `upload_key` finds no valid basename to write to (empty, whitespace-only,
///   or the path is a prefix like `"docs/"` with nothing after it) -- see its
///   doc comment. Enqueueing anyway would write to a bogus/empty key or, at
///   `prefix` itself, to the folder's own marker object.
/// - the path cannot be stat-ed (gone, unreadable, a directory).
///
/// Split out of `enqueue_uploads` so both skip paths are unit-testable
/// without a live `TransferEngine`.
fn resolve_upload(
    connection_id: String,
    bucket: String,
    prefix: &str,
    path: String,
) -> Option<EnqueueSpec> {
    let local = PathBuf::from(&path);
    let file_name = local
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let key = upload_key(prefix, &file_name);
    if key.is_empty() {
        tracing::warn!(path = %path, "skipping upload path with no valid basename");
        return None;
    }

    match EnqueueSpec::for_upload(connection_id, bucket, key, local) {
        Ok(spec) => Some(spec),
        Err(err) => {
            tracing::warn!(path = %path, "skipping unusable upload path: {err}");
            None
        }
    }
}

/// Queues one upload task per local path. Paths that cannot be stat-ed (gone,
/// unreadable, a directory), or that resolve to no valid remote key (see
/// [`resolve_upload`]), are rejected **individually**: one bad path in a
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
        if let Some(spec) = resolve_upload(connection_id.clone(), bucket.clone(), &prefix, path) {
            queued.push(engine.enqueue(spec).await?);
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
        // basename is trimmed *after* the last path segment is taken, so a
        // name like "a.txt " (e.g. from a source filesystem that allows
        // trailing whitespace) lands on the key without it.
        assert_eq!(upload_key("docs/", "a.txt "), "docs/a.txt");
        assert_eq!(upload_key("docs/", " a.txt"), "docs/a.txt");
        assert_eq!(upload_key("docs/", "  a.txt  "), "docs/a.txt");
        assert_eq!(upload_key("", "  a.txt  "), "a.txt");
        // Whitespace-only path segments before the last "/" are irrelevant --
        // only the final segment is trimmed.
        assert_eq!(upload_key("docs/", "sub/ a.txt "), "docs/a.txt");
    }

    // --- js_trim: pinning the two characters where Rust's `str::trim` and
    // JS's `String.prototype.trim` disagree (see `js_trim`'s doc comment). An
    // ASCII-only test suite would never catch a regression back to the bare
    // `str::trim`, since every ASCII whitespace case behaves identically
    // either way. ------------------------------------------------------

    #[test]
    fn js_trim_keeps_nel_u0085_unlike_rust_str_trim() {
        // U+0085 (NEL) satisfies Rust's `char::is_whitespace`, so bare
        // `str::trim` would strip it -- but ECMAScript's whitespace set
        // excludes it, so JS's `trim()` leaves it in place. `js_trim` must
        // match JS: keep it.
        assert_eq!(js_trim("a.txt\u{85}"), "a.txt\u{85}");
        assert_eq!(js_trim("\u{85}a.txt\u{85}"), "\u{85}a.txt\u{85}");
        // Sanity check against the actual divergence: bare `str::trim` DOES
        // strip it, which is exactly the bug this helper exists to avoid.
        assert_eq!("a.txt\u{85}".trim(), "a.txt");
    }

    #[test]
    fn js_trim_strips_bom_u_feff_unlike_rust_str_trim() {
        // U+FEFF (BOM/ZWNBSP) is in ECMAScript's whitespace set, so JS's
        // `trim()` strips it -- but Rust's `char::is_whitespace` excludes it,
        // so bare `str::trim` would leave it in place. `js_trim` must match
        // JS: strip it.
        assert_eq!(js_trim("a.txt\u{feff}"), "a.txt");
        assert_eq!(js_trim("\u{feff}a.txt\u{feff}"), "a.txt");
        // Sanity check against the actual divergence: bare `str::trim` does
        // NOT strip it, which is exactly the bug this helper exists to
        // avoid.
        assert_eq!("a.txt\u{feff}".trim(), "a.txt\u{feff}");
    }

    #[test]
    fn js_trim_strips_nbsp_like_both_implementations_agree() {
        // U+00A0 (NBSP) is whitespace under both definitions -- included for
        // completeness, per the task's full agreement table.
        assert_eq!(js_trim("a.txt\u{a0}"), "a.txt");
    }

    #[test]
    fn js_trim_still_strips_ordinary_ascii_whitespace() {
        assert_eq!(js_trim("  a.txt  "), "a.txt");
        assert_eq!(js_trim(""), "");
        assert_eq!(js_trim("   "), "");
    }

    // --- upload_key: the two divergent characters carried through the whole
    // basename + prefix pipeline, matching `uploadKey` in
    // `src/lib/entries.ts` (see the module doc's agreement table). ------

    #[test]
    fn upload_key_keeps_a_trailing_nel_the_way_js_does() {
        // This is the data-loss direction called out in the task: if Rust
        // stripped this (bare `str::trim` would), an existing remote
        // "docs/a.txt" would be silently destroyed while the frontend guard
        // checked the (JS-untouched) "docs/a.txt\u{85}" and saw no collision.
        assert_eq!(upload_key("docs/", "a.txt\u{85}"), "docs/a.txt\u{85}");
    }

    #[test]
    fn upload_key_strips_a_trailing_bom_the_way_js_does() {
        assert_eq!(upload_key("docs/", "a.txt\u{feff}"), "docs/a.txt");
    }

    // --- upload_key: an empty basename has no valid target (see the doc
    // comment) -- `""`, never `prefix` itself. -------------------------

    #[test]
    fn empty_basename_yields_empty_key_not_the_bare_prefix() {
        // Previously this returned `prefix` unchanged -- `upload_key("docs/",
        // "")` yielding "docs/", the folder's own marker key, which a
        // `PutObject` could then overwrite.
        assert_eq!(upload_key("docs/", ""), "");
        assert_eq!(upload_key("docs/", "   "), "");
        assert_eq!(upload_key("", ""), "");
        assert_eq!(upload_key("docs/", "sub/"), "");
    }

    // --- resolve_upload: mirrors the existing stat-failure skip for a path
    // that resolves to no valid remote key. ----------------------------

    #[test]
    fn resolve_upload_skips_a_path_with_no_valid_basename() {
        // An empty `file_name` (e.g. a path with nothing after the last
        // separator) makes `upload_key` return "" before any filesystem
        // access happens, so this is testable without a real file on disk.
        assert!(
            resolve_upload("c1".to_string(), "b".to_string(), "docs/", String::new()).is_none()
        );
    }

    #[test]
    fn resolve_upload_skips_a_whitespace_only_basename() {
        assert!(resolve_upload(
            "c1".to_string(),
            "b".to_string(),
            "docs/",
            "   ".to_string()
        )
        .is_none());
    }

    #[test]
    fn resolve_upload_skips_a_path_that_cannot_be_stat_ed() {
        // Mirrors the pre-existing behaviour for a path that fails
        // `EnqueueSpec::for_upload`'s stat: still `None`, not a task pointed
        // at a bogus key.
        assert!(resolve_upload(
            "c1".to_string(),
            "b".to_string(),
            "docs/",
            "/definitely/does/not/exist/a.txt".to_string(),
        )
        .is_none());
    }
}
