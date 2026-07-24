//! Transfer commands. Thin, like every other command module: parse, delegate
//! to [`TransferEngine`], return a DTO.

use std::path::{Component, Path, PathBuf};

use tauri::State;

use crate::commands::AppState;
use crate::error::AppResult;
use crate::provider::{ObjectEntry, Provider};
use crate::transfer::{EnqueueSpec, TransferEngine, TransferTaskDto};

/// Objects per `list_objects_flat` page while walking a folder for download.
/// 1000 is S3's `ListObjectsV2` maximum, so the subtree is walked in the
/// fewest possible round-trips (mirrors the provider's own recursive walks).
const FLAT_LIST_PAGE_SIZE: i32 = 1000;

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

/// Queues a single-file download. Heads the object first so the panel shows
/// the real size immediately rather than counting up from an unknown total.
#[tauri::command]
pub async fn enqueue_download(
    state: State<'_, AppState>,
    engine: State<'_, TransferEngine>,
    connection_id: String,
    bucket: String,
    key: String,
    local_path: String,
) -> AppResult<TransferTaskDto> {
    let provider = state.hub().provider(&connection_id).await?;
    let head = provider.head_object(&bucket, &key).await?;
    let path = std::path::PathBuf::from(&local_path);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| key.rsplit('/').next().unwrap_or(&key).to_string());
    let spec = EnqueueSpec::for_download(connection_id, bucket, key, path, head.size, file_name);
    engine.enqueue(spec).await
}

/// The local path an object lands at under a folder download: `local_dir`
/// joined with `key`'s path relative to `prefix`, with any `..`/absolute
/// components stripped so a crafted key can never escape `local_dir`.
///
/// `key` comes from the remote store and is therefore attacker-influenceable:
/// a key like `docs/../../etc/passwd` must NOT resolve outside `local_dir`.
/// The relative part (whatever follows `prefix`) is rebuilt from *only* its
/// [`Component::Normal`] pieces, so `ParentDir` (`..`), `RootDir` (a leading
/// `/`), `CurDir` (`.`) and Windows `Prefix` components are all dropped. The
/// result is always a descendant of `local_dir`.
///
/// Returns `None` when nothing normal survives -- e.g. `key` equal to the
/// `prefix` marker (`"docs/"`), which strips to an empty relative path. That
/// must not silently produce `local_dir` itself. A `key` that does not start
/// with `prefix` (defensive; `list_objects_flat` only returns keys under
/// `prefix`) is treated as its own relative path and still sanitized, so it
/// can never escape either.
/// Maps a remote object `key` under `prefix` to its local download path,
/// preserving the folder's own name and sanitizing against path traversal.
/// `pub` so the live folder-download e2e can drive the *real* policy instead of
/// a copy that could silently drift from it.
pub fn local_target(prefix: &str, key: &str, local_dir: &Path) -> Option<PathBuf> {
    // A `/`-terminated key is a folder marker, not a file (the command already
    // skips these before calling; the guard keeps the helper self-contained and
    // stops the folder's own marker from producing a bogus target).
    if key.ends_with('/') {
        return None;
    }
    // Keep the downloaded folder's own name as the top local directory: strip
    // only the prefix's PARENT, not the prefix itself. For "photos/" the parent
    // is "" so "photos/cat1.txt" stays "photos/cat1.txt" (-> <dir>/photos/...);
    // for "a/b/photos/" the parent is "a/b/" so the same object becomes
    // "photos/cat1.txt" (-> <dir>/photos/..., not the whole a/b/ path).
    // Downloading a folder should reproduce that folder under `local_dir`, not
    // spill its contents loose into it -- and namespacing by the folder name
    // stops two folders downloaded into one directory from colliding.
    let trimmed = prefix.strip_suffix('/').unwrap_or(prefix);
    let parent = match trimmed.rfind('/') {
        Some(i) => &prefix[..i + 1], // up to and including the parent's slash
        None => "",
    };
    let relative = key.strip_prefix(parent).unwrap_or(key);
    let mut sanitized = PathBuf::new();
    for component in Path::new(relative).components() {
        if let Component::Normal(part) = component {
            sanitized.push(part);
        }
    }
    if sanitized.as_os_str().is_empty() {
        return None;
    }
    Some(local_dir.join(sanitized))
}

/// Queues a recursive folder download: one task per real object under
/// `prefix`, reconstructing the subtree beneath `local_dir`.
///
/// The subtree is walked with the delimiter-less [`Provider::list_objects_flat`]
/// in a pagination loop (like `delete_prefix`), which returns every object
/// under `prefix` INCLUDING zero-byte folder-marker objects. Those markers --
/// any key ending in `/`, both the `prefix/` marker itself and any nested
/// empty-folder marker -- are skipped: they are directory placeholders, not
/// files, and downloading one would create a spurious empty file named after
/// the folder.
///
/// Each object's local target is `local_dir` joined with its path relative to
/// the *parent* of `prefix` -- so the downloaded folder's own name becomes the
/// top local directory (`photos/` -> `<local_dir>/photos/...`) rather than its
/// contents spilling loose into `local_dir`. The path is sanitized by
/// [`local_target`] so a crafted key can never escape `local_dir`. The size
/// comes straight from the listing (which already
/// carries it); only when the server omitted it does this fall back to a
/// per-object `head_object`, avoiding a wasted round-trip otherwise.
///
/// Design §7 partial failure: an object that cannot be turned into a usable
/// local path (or whose head fallback / enqueue fails) is skipped with a
/// `warn!` and the walk continues -- one bad object must not sink the whole
/// folder. Returns the tasks that were actually enqueued.
#[tauri::command]
pub async fn enqueue_folder_download(
    state: State<'_, AppState>,
    engine: State<'_, TransferEngine>,
    connection_id: String,
    bucket: String,
    prefix: String,
    local_dir: String,
) -> AppResult<Vec<TransferTaskDto>> {
    let provider = state.hub().provider(&connection_id).await?;
    let local_dir = PathBuf::from(&local_dir);

    // Accumulate every object across pages, exactly like `delete_prefix`.
    let mut entries: Vec<ObjectEntry> = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let page = provider
            .list_objects_flat(&bucket, &prefix, token.as_deref(), FLAT_LIST_PAGE_SIZE)
            .await?;
        entries.extend(page.entries);
        match page.next_token {
            Some(next) => token = Some(next),
            None => break,
        }
    }

    let mut queued = Vec::with_capacity(entries.len());
    for entry in entries {
        // Skip folder markers: a key ending in `/` is a 0-byte directory
        // placeholder (the `prefix/` marker or a nested empty-folder marker),
        // not a file. `local_target` also rejects the exact `prefix` marker,
        // but a nested marker like `docs/sub/` strips to `sub` and would
        // otherwise be enqueued as a bogus file -- so the skip happens here.
        if entry.key.ends_with('/') {
            continue;
        }
        let Some(path) = local_target(&prefix, &entry.key, &local_dir) else {
            tracing::warn!(key = %entry.key, "skipping object with no usable local path");
            continue;
        };
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| {
                entry
                    .key
                    .rsplit('/')
                    .next()
                    .unwrap_or(&entry.key)
                    .to_string()
            });
        // Prefer the size the flat listing already carries; only head the
        // object when the server omitted it (a wasted round-trip otherwise).
        let total = match entry.size {
            Some(size) => size,
            None => match provider.head_object(&bucket, &entry.key).await {
                Ok(head) => head.size,
                Err(err) => {
                    tracing::warn!(key = %entry.key, "skipping object whose size could not be resolved: {err}");
                    continue;
                }
            },
        };
        let spec = EnqueueSpec::for_download(
            connection_id.clone(),
            bucket.clone(),
            entry.key,
            path,
            total,
            file_name,
        );
        match engine.enqueue(spec).await {
            Ok(task) => queued.push(task),
            Err(err) => tracing::warn!("skipping object that could not be enqueued: {err}"),
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

    // --- local_target: the relative structure under `prefix` is preserved,
    // but a crafted key can never escape `local_dir` (the whole point). -----

    #[test]
    fn local_target_preserves_the_subtree_under_the_prefix() {
        // The folder's own name (`docs`) is kept as the top directory, then the
        // subtree beneath it.
        assert_eq!(
            local_target("docs/", "docs/sub/a.txt", Path::new("/D")),
            Some(PathBuf::from("/D/docs/sub/a.txt"))
        );
    }

    #[test]
    fn local_target_of_a_direct_child_lands_under_the_folder() {
        // A direct child lands under the recreated folder, not loose in
        // `local_dir` -- downloading "docs/" gives you a "docs" directory.
        assert_eq!(
            local_target("docs/", "docs/a.txt", Path::new("/D")),
            Some(PathBuf::from("/D/docs/a.txt"))
        );
    }

    #[test]
    fn local_target_keeps_only_the_folder_name_for_a_nested_prefix() {
        // Downloading a folder that itself lives under other prefixes keeps only
        // the folder's own name, not the whole path to it.
        assert_eq!(
            local_target("a/b/photos/", "a/b/photos/cat.png", Path::new("/D")),
            Some(PathBuf::from("/D/photos/cat.png"))
        );
    }

    #[test]
    fn local_target_strips_parent_dir_so_a_crafted_key_cannot_escape() {
        // A key carrying `..` segments must not write outside `local_dir`.
        // The discriminating assertion is the *absence of `..`*: `starts_with`
        // alone is fooled by a lexical `/D/../secret` (it still "starts with"
        // /D), so a sanitizer regression would slip past a starts_with-only
        // test. See the red/green proof in the task report.
        let target = local_target("docs/", "docs/../secret", Path::new("/D"))
            .expect("a sanitized `..` key still has a Normal component");
        assert!(
            target.components().all(|c| c != Component::ParentDir),
            "sanitized path must contain no `..`: {}",
            target.display()
        );
        assert!(
            target.starts_with("/D"),
            "sanitized path must stay under local_dir: {}",
            target.display()
        );
        // Concretely: the `..` is dropped, not resolved -- the folder name is
        // kept, so it lands under `/D/docs`, never above it.
        assert_eq!(target, PathBuf::from("/D/docs/secret"));
    }

    #[test]
    fn local_target_strips_a_deep_dot_dot_traversal() {
        let target = local_target("docs/", "docs/../../etc/passwd", Path::new("/D"))
            .expect("normal components survive");
        assert!(target.components().all(|c| c != Component::ParentDir));
        assert!(target.starts_with("/D"));
        assert_eq!(target, PathBuf::from("/D/docs/etc/passwd"));
    }

    #[test]
    fn local_target_handles_a_prefix_without_a_trailing_slash() {
        // Real folder prefixes always end in `/`, but the helper must still be
        // sane without one: the parent of "docs" is empty, so the folder name
        // is kept and the object lands under `/D/docs`. (RootDir-dropping for a
        // genuinely absolute key is covered by the bare-absolute test below.)
        let target =
            local_target("docs", "docs/etc", Path::new("/D")).expect("normal components survive");
        assert_eq!(target, PathBuf::from("/D/docs/etc"));
        assert!(target.starts_with("/D"));
    }

    #[test]
    fn local_target_of_the_prefix_marker_is_none_not_local_dir_itself() {
        // The `prefix/` marker strips to an empty relative path; returning
        // `local_dir` itself would point a download at the folder, so it must
        // be `None`. (The command skips `/`-terminated keys before ever
        // reaching here, but the helper still must not manufacture a target.)
        assert_eq!(local_target("docs/", "docs/", Path::new("/D")), None);
    }

    #[test]
    fn local_target_of_a_key_not_under_the_prefix_is_still_contained() {
        // Defensive: `list_objects_flat` only returns keys under `prefix`, but
        // if one slips through it is treated as its own relative path and
        // still sanitized -- it can never escape `local_dir`.
        let target = local_target("docs/", "other/a.txt", Path::new("/D"))
            .expect("normal components survive");
        assert_eq!(target, PathBuf::from("/D/other/a.txt"));
        assert!(target.starts_with("/D"));
    }

    #[test]
    fn local_target_of_a_bare_absolute_key_stays_contained() {
        // The brief's literal example: a key that is itself an absolute path.
        // `strip_prefix` fails, the whole key becomes the relative part, and
        // the RootDir component is dropped -- so it lands under `local_dir`,
        // never at the real filesystem root.
        let target = local_target("docs/", "/etc/passwd", Path::new("/D"))
            .expect("normal components survive");
        assert_eq!(target, PathBuf::from("/D/etc/passwd"));
        assert!(target.starts_with("/D"));
        // The only RootDir is `local_dir`'s own leading `/`; the key's
        // absolute component was dropped, so no `..` and no jump to the real
        // root survived.
        assert!(target.components().all(|c| c != Component::ParentDir));
    }

    #[test]
    fn local_target_of_a_backslash_key_is_contained_on_every_platform() {
        // Object keys use `/`, but a crafted key could carry `\`. On Windows
        // `\` is also a separator, so `..\..\etc` decomposes into ParentDir/
        // ParentDir/Normal and is stripped like the `/` case; on Unix `\` is
        // an ordinary filename character, so it stays one opaque Normal
        // component. Either way the result is contained under `local_dir` and
        // carries no `..` component -- this test pins that on whichever
        // platform runs it (and verifies the real separator semantics on a
        // Windows CI).
        let target = local_target("docs/", "docs/..\\..\\etc", Path::new("/D"))
            .expect("at least one normal component survives");
        assert!(target.starts_with("/D"));
        assert!(target.components().all(|c| c != Component::ParentDir));
    }
}
