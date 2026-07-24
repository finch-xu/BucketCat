//! Runtime toggle for the resume/checkpoint feature (M4c Task 9).
//!
//! The transfer engine gates its checkpoint *writer* on a shared
//! `Arc<AtomicBool>` (Task 5/6): when it reads `false`, no checkpoint file is
//! written for a running transfer. `lib.rs`'s `setup` builds that atomic once
//! from the persisted `Settings::resume_enabled` and clones the same `Arc`
//! into both the engine and this module's [`ResumeFlag`] managed state --
//! there is exactly one atomic in the process, never two, so a toggle made
//! here takes effect on the very next checkpoint write the engine attempts.
//!
//! Persisting the choice (so it survives a restart) is a separate concern
//! from flipping the runtime atomic (so it takes effect immediately); both
//! happen together in [`apply_resume_setting`], which is the pure core these
//! two commands shell around.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::commands::AppState;
use crate::error::{AppError, AppResult};
use crate::provider::clamp_expiry;
use crate::store::settings::{self, clamp_parts, clamp_tasks, Settings};
use crate::transfer::{
    bcpart_path, checkpoint, checkpoint_dir, plan_restore, Checkpoint, Direction, RestoreAction,
};

/// Tauri-managed handle to the runtime resume flag. Wraps the *same*
/// `Arc<AtomicBool>` the transfer engine was constructed with in `lib.rs`'s
/// `setup` (both are clones of one `Arc`, never separately constructed) --
/// see this module's doc comment for why that sharing is load-bearing.
pub struct ResumeFlag(pub Arc<AtomicBool>);

/// Stores `enabled` into `atomic` (the engine reads this directly, so this is
/// the "takes effect now" half) and persists it to `settings_path` via
/// [`settings::save`] (the "survives a restart" half).
///
/// Save happens before the atomic is stored: if the write to disk fails, the
/// command errors out and the runtime flag is left exactly as it was, rather
/// than the app believing a change is live that a restart would silently
/// revert (the persisted file, not the atomic, is the source of truth on
/// startup -- see `lib.rs`'s `setup`).
///
/// Pure with respect to Tauri -- no `State`/`AppHandle` -- so it is
/// unit-testable with a bare `AtomicBool` and a tempdir path; the two
/// `#[tauri::command]`s below are thin shells that resolve those two inputs
/// from managed state / the app handle and call straight through.
///
/// Goes through [`apply_settings_patch`] (load -> mutate -> save) rather than
/// writing a fresh `Settings` from just `enabled`, so toggling resume never
/// clobbers `max_tasks`/`max_parts`/`share_expiry_secs` already on disk (M6c
/// grew the struct past this one field).
pub fn apply_resume_setting(
    atomic: &AtomicBool,
    settings_path: &Path,
    enabled: bool,
) -> AppResult<()> {
    apply_settings_patch(settings_path, |s| s.resume_enabled = enabled)?;
    atomic.store(enabled, Ordering::Relaxed);
    Ok(())
}

/// The same `<app_config_dir>/settings.json` path `lib.rs`'s `setup` loads
/// `Settings` from at startup.
fn settings_path(app: &AppHandle) -> AppResult<PathBuf> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| AppError::Internal {
            message: format!("resolve app config dir: {e}"),
        })?;
    Ok(dir.join("settings.json"))
}

/// Pure load -> mutate -> save core shared by every M6c settings setter: reads
/// whatever is currently persisted (fail-safe defaults if missing/corrupt --
/// see [`settings::load`]), applies `f`, then writes the result back
/// atomically. Never touches a runtime atomic; callers that need the "takes
/// effect immediately" half (resume) layer that on top, same as
/// [`apply_resume_setting`] does.
pub fn apply_settings_patch(path: &Path, f: impl FnOnce(&mut Settings)) -> AppResult<()> {
    let mut s = settings::load(path);
    f(&mut s);
    settings::save(path, &s)
}

/// Returns the whole persisted `Settings`, so the Settings modal (Task 3) can
/// initialize every field from one round trip.
#[tauri::command]
pub fn get_settings(app: AppHandle) -> AppResult<Settings> {
    Ok(settings::load(&settings_path(&app)?))
}

/// Persists a new max-concurrent-tasks limit, clamped to `[1, 5]`. Takes
/// effect on the next engine construction (app restart) -- M6c reads
/// concurrency from settings only at `TransferEngine::new` time, there is no
/// runtime hot-update.
#[tauri::command]
pub fn set_max_tasks(app: AppHandle, n: usize) -> AppResult<()> {
    apply_settings_patch(&settings_path(&app)?, |s| s.max_tasks = clamp_tasks(n))
}

/// Persists a new max-parts-per-task limit, clamped to `[1, 8]`. Same
/// next-restart caveat as [`set_max_tasks`].
#[tauri::command]
pub fn set_max_parts(app: AppHandle, n: usize) -> AppResult<()> {
    apply_settings_patch(&settings_path(&app)?, |s| s.max_parts = clamp_parts(n))
}

/// Persists a new default Share-link expiry, clamped the same way
/// `provider::clamp_expiry` bounds an explicit per-call value (`[1, 604800]`
/// seconds). Frontend-consumed only -- no runtime atomic to flip.
#[tauri::command]
pub fn set_share_expiry(app: AppHandle, secs: u64) -> AppResult<()> {
    apply_settings_patch(&settings_path(&app)?, |s| {
        s.share_expiry_secs = clamp_expiry(secs)
    })
}

/// Current value of the runtime resume flag.
#[tauri::command]
pub fn get_resume_enabled(state: State<'_, ResumeFlag>) -> bool {
    state.0.load(Ordering::Relaxed)
}

/// Sets the runtime resume flag to `enabled` and persists the choice. See
/// [`apply_resume_setting`] for the store-then-persist contract.
#[tauri::command]
pub fn set_resume_enabled(
    app: AppHandle,
    state: State<'_, ResumeFlag>,
    enabled: bool,
) -> AppResult<()> {
    let path = settings_path(&app)?;
    apply_resume_setting(&state.0, &path, enabled)
}

/// The same `<app_data_dir>/checkpoints` directory `lib.rs`'s `setup` (and its
/// startup `restore_all`) compute `TransferEngine`'s checkpoint store from.
fn app_checkpoint_dir(app: &AppHandle) -> AppResult<PathBuf> {
    let base = app.path().app_data_dir().map_err(|e| AppError::Internal {
        message: format!("resolve app data dir: {e}"),
    })?;
    Ok(checkpoint_dir(&base))
}

/// Result of [`clean_checkpoint_residue`]: how many orphan checkpoints were
/// removed, and how many bytes that (plus any staged `.bcpart`) freed.
#[derive(Debug, Serialize)]
pub struct CleanResult {
    pub removed: usize,
    pub freed_bytes: u64,
}

/// Every scanned checkpoint whose `connection_id` is not in `known`, reduced
/// to what removing it needs: the task id (for [`checkpoint::remove`]), its
/// [`Direction`] (a download also has a staging `.bcpart` to drop), and its
/// `local_path` (from which [`clean_checkpoint_residue`] derives that
/// `.bcpart`'s path via [`bcpart_path`]).
///
/// Delegates the known-vs-orphan decision to [`plan_restore`] -- the same
/// pure function the startup restore (`transfer::engine::restore_all`) uses
/// -- rather than re-deriving it here, so a manual clean and the automatic
/// startup discard can never disagree about what counts as an orphan.
/// `known` is always wrapped `Ok` before reaching `plan_restore`: the
/// `connection_ids` read failure that would make `plan_restore` return an
/// empty plan (never treat "store unreadable" as "no connections exist") is
/// handled by the caller, one layer up in [`clean_checkpoint_residue`],
/// before `known` is ever built.
fn orphan_removals(
    scanned: Vec<(String, Checkpoint)>,
    known: &HashSet<String>,
) -> Vec<(String, Direction, PathBuf)> {
    plan_restore(scanned, &Ok(known.clone()))
        .into_iter()
        .filter_map(|action| match action {
            RestoreAction::DiscardOrphan(id, cp) => {
                Some((id, cp.direction, PathBuf::from(cp.local_path)))
            }
            RestoreAction::Restore(..) => None,
        })
        .collect()
}

/// Removes exactly the orphans [`orphan_removals`] selected from `dir` (the
/// checkpoint directory), totaling what it freed. Split out from
/// [`clean_checkpoint_residue`] so the byte-accounting and "best effort,
/// never abort on one failure" behavior is unit-testable against a tempdir,
/// without a live Tauri app or hub -- the command itself is then just
/// "resolve `known`/`dir`, delegate".
fn remove_orphans(dir: &Path, orphans: Vec<(String, Direction, PathBuf)>) -> CleanResult {
    let mut removed = 0usize;
    let mut freed_bytes = 0u64;
    for (id, direction, local_path) in orphans {
        if direction == Direction::Download {
            let bc = bcpart_path(&local_path);
            match std::fs::metadata(&bc) {
                Ok(meta) => freed_bytes += meta.len(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(task = %id, "stat .bcpart failed: {e}"),
            }
            if let Err(e) = std::fs::remove_file(&bc) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(task = %id, "removing .bcpart failed: {e}");
                }
            }
        }
        match std::fs::metadata(dir.join(format!("{id}.json"))) {
            Ok(meta) => freed_bytes += meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(task = %id, "stat checkpoint failed: {e}"),
        }
        checkpoint::remove(dir, &id);
        removed += 1;
    }
    CleanResult {
        removed,
        freed_bytes,
    }
}

/// Manual, user-triggered counterpart to the orphan discard `restore_all`
/// already performs on every launch (Advanced Settings' cleanup action,
/// closing an M4c follow-up): removes every checkpoint whose connection has
/// been deleted -- and, for a download, its staging `.bcpart` -- and reports
/// how many were removed and how many bytes that freed.
///
/// `connection_ids`'s `?` is the safety-critical line: if the connection
/// store can't be read, this returns `Err` *before* `orphan_removals` (let
/// alone [`remove_orphans`]) ever runs, exactly like the startup path -- a
/// transient read failure must never be mistaken for "no connections exist"
/// and discard every checkpoint on disk.
#[tauri::command]
pub async fn clean_checkpoint_residue(
    state: State<'_, AppState>,
    app: AppHandle,
) -> AppResult<CleanResult> {
    let known: HashSet<String> = state.hub().connection_ids().await?.into_iter().collect();
    let dir = app_checkpoint_dir(&app)?;
    let orphans = orphan_removals(checkpoint::scan(&dir), &known);
    Ok(remove_orphans(&dir, orphans))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_resume_setting_disables_both_atomic_and_persisted_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let atomic = AtomicBool::new(true);

        apply_resume_setting(&atomic, &path, false).unwrap();

        assert!(!atomic.load(Ordering::Relaxed), "atomic must flip to false");
        assert!(
            !settings::load(&path).resume_enabled,
            "persisted value must round-trip to false"
        );
    }

    #[test]
    fn apply_resume_setting_re_enables_both_atomic_and_persisted_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let atomic = AtomicBool::new(false);
        // Starting file already reflects the disabled state, mirroring a
        // real prior `apply_resume_setting(.., false)` call.
        settings::save(
            &path,
            &Settings {
                resume_enabled: false,
                ..Settings::default()
            },
        )
        .unwrap();

        apply_resume_setting(&atomic, &path, true).unwrap();

        assert!(
            atomic.load(Ordering::Relaxed),
            "atomic must flip back to true"
        );
        assert!(
            settings::load(&path).resume_enabled,
            "persisted value must round-trip to true"
        );
    }

    #[test]
    fn apply_resume_setting_preserves_other_persisted_fields() {
        // Regression: `apply_resume_setting` used to write a fresh `Settings`
        // built from just `enabled`, silently resetting max_tasks/max_parts/
        // share_expiry_secs to their defaults on every resume toggle.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        settings::save(
            &path,
            &Settings {
                resume_enabled: true,
                max_tasks: 5,
                max_parts: 8,
                share_expiry_secs: 120,
            },
        )
        .unwrap();
        let atomic = AtomicBool::new(true);

        apply_resume_setting(&atomic, &path, false).unwrap();

        let loaded = settings::load(&path);
        assert!(!loaded.resume_enabled);
        assert_eq!(loaded.max_tasks, 5, "must not reset max_tasks");
        assert_eq!(loaded.max_parts, 8, "must not reset max_parts");
        assert_eq!(
            loaded.share_expiry_secs, 120,
            "must not reset share_expiry_secs"
        );
    }

    #[test]
    fn apply_settings_patch_touches_only_the_patched_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        apply_settings_patch(&path, |s| s.max_tasks = clamp_tasks(2)).unwrap();
        apply_settings_patch(&path, |s| s.max_parts = clamp_parts(6)).unwrap();
        apply_settings_patch(&path, |s| s.share_expiry_secs = clamp_expiry(120)).unwrap();

        let loaded = settings::load(&path);
        assert_eq!(loaded.max_tasks, 2);
        assert_eq!(loaded.max_parts, 6);
        assert_eq!(loaded.share_expiry_secs, 120);
        assert!(loaded.resume_enabled, "untouched field keeps its default");
    }

    #[test]
    fn get_resume_enabled_reads_the_atomic_directly() {
        let atomic = Arc::new(AtomicBool::new(false));
        let flag = ResumeFlag(atomic.clone());

        assert!(!flag.0.load(Ordering::Relaxed));
        atomic.store(true, Ordering::Relaxed);
        assert!(flag.0.load(Ordering::Relaxed));
    }

    // --- clean_checkpoint_residue: orphan_removals + remove_orphans --------

    use crate::transfer::{DownloadState, MultipartState, ResumeState};

    /// A minimal checkpoint tagged with `conn` as its connection id and
    /// `direction`/`local_path` as given -- enough for `orphan_removals`
    /// (which only reads `connection_id` via `plan_restore`, then
    /// `direction`/`local_path`).
    fn cp(direction: Direction, conn: &str, local_path: &str) -> Checkpoint {
        let resume = match direction {
            Direction::Upload => ResumeState::Upload(MultipartState {
                upload_id: "u1".to_string(),
                completed: vec![],
                source_size: 100,
                source_mtime: 0,
            }),
            Direction::Download => ResumeState::Download(DownloadState::default()),
        };
        Checkpoint {
            direction,
            connection_id: conn.to_string(),
            bucket: "b".to_string(),
            key: "k".to_string(),
            local_path: local_path.to_string(),
            file_name: "k".to_string(),
            total: 100,
            resume,
        }
    }

    /// Only the checkpoint whose connection is NOT in `known` comes back --
    /// proving `orphan_removals` keys off `plan_restore`'s `DiscardOrphan`
    /// arm, not some independent re-derivation of "orphan".
    #[test]
    fn orphan_removals_keeps_only_the_unknown_connection() {
        let scanned = vec![
            ("t-known".to_string(), cp(Direction::Upload, "c1", "/tmp/a")),
            (
                "t-orphan".to_string(),
                cp(Direction::Download, "cX", "/tmp/b"),
            ),
        ];
        let known: HashSet<String> = HashSet::from(["c1".to_string()]);

        let orphans = orphan_removals(scanned, &known);

        assert_eq!(
            orphans.len(),
            1,
            "only the unknown-connection entry survives"
        );
        assert_eq!(orphans[0].0, "t-orphan");
        assert_eq!(orphans[0].1, Direction::Download);
        assert_eq!(orphans[0].2, PathBuf::from("/tmp/b"));
    }

    /// Both a download and an upload orphan carry their `local_path` through
    /// unchanged -- `orphan_removals` never special-cases direction itself;
    /// that decision (whether a `.bcpart` exists at all) is the caller's,
    /// via `bcpart_path`.
    #[test]
    fn orphan_removals_returns_local_path_for_both_directions() {
        let scanned = vec![
            (
                "t-up".to_string(),
                cp(Direction::Upload, "gone", "/tmp/up.bin"),
            ),
            (
                "t-down".to_string(),
                cp(Direction::Download, "gone", "/tmp/down.bin"),
            ),
        ];
        let known: HashSet<String> = HashSet::new();

        let mut orphans = orphan_removals(scanned, &known);
        orphans.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(orphans.len(), 2);
        assert_eq!(
            orphans[0],
            (
                "t-down".to_string(),
                Direction::Download,
                PathBuf::from("/tmp/down.bin")
            )
        );
        assert_eq!(
            orphans[1],
            (
                "t-up".to_string(),
                Direction::Upload,
                PathBuf::from("/tmp/up.bin")
            )
        );
    }

    /// File-level: a fake `<id>.json` plus a fake `.bcpart` at the derived
    /// path, both with known sizes -- `remove_orphans` must delete both and
    /// report `freed_bytes` as exactly their sum.
    #[test]
    fn remove_orphans_deletes_checkpoint_and_bcpart_and_sums_their_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let task_id = "t-orphan";
        let cp_bytes = b"{\"fake\":\"checkpoint\"}";
        std::fs::write(dir.path().join(format!("{task_id}.json")), cp_bytes).unwrap();

        let local_path = dir.path().join("final").join("photo.jpg");
        std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        let bc = bcpart_path(&local_path);
        let bc_bytes = b"partial-bytes-of-a-download";
        std::fs::write(&bc, bc_bytes).unwrap();

        let result = remove_orphans(
            dir.path(),
            vec![(task_id.to_string(), Direction::Download, local_path)],
        );

        assert_eq!(result.removed, 1);
        assert_eq!(result.freed_bytes, (cp_bytes.len() + bc_bytes.len()) as u64);
        assert!(
            !dir.path().join(format!("{task_id}.json")).exists(),
            "checkpoint file must be gone"
        );
        assert!(!bc.exists(), ".bcpart must be gone");
    }

    /// An upload orphan has no `.bcpart` to touch: only the checkpoint file's
    /// size is counted and removed.
    #[test]
    fn remove_orphans_upload_only_removes_the_checkpoint_file() {
        let dir = tempfile::tempdir().unwrap();
        let task_id = "t-upload-orphan";
        let cp_bytes = b"{\"fake\":\"upload-checkpoint\"}";
        std::fs::write(dir.path().join(format!("{task_id}.json")), cp_bytes).unwrap();

        let result = remove_orphans(
            dir.path(),
            vec![(
                task_id.to_string(),
                Direction::Upload,
                PathBuf::from("/tmp/never-written.bin"),
            )],
        );

        assert_eq!(result.removed, 1);
        assert_eq!(result.freed_bytes, cp_bytes.len() as u64);
        assert!(!dir.path().join(format!("{task_id}.json")).exists());
    }

    /// A missing `.bcpart` (e.g. already cleaned up) is not an error: it
    /// contributes zero bytes and `remove_orphans` still proceeds to remove
    /// the checkpoint file and counts it as removed.
    #[test]
    fn remove_orphans_tolerates_a_missing_bcpart() {
        let dir = tempfile::tempdir().unwrap();
        let task_id = "t-missing-bcpart";
        let cp_bytes = b"{\"fake\":\"checkpoint\"}";
        std::fs::write(dir.path().join(format!("{task_id}.json")), cp_bytes).unwrap();

        let result = remove_orphans(
            dir.path(),
            vec![(
                task_id.to_string(),
                Direction::Download,
                PathBuf::from("/tmp/nonexistent-final-path.bin"),
            )],
        );

        assert_eq!(result.removed, 1);
        assert_eq!(result.freed_bytes, cp_bytes.len() as u64);
    }
}
