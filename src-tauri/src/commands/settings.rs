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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::{AppHandle, Manager, State};

use crate::error::{AppError, AppResult};
use crate::store::settings::{self, Settings};

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
pub fn apply_resume_setting(
    atomic: &AtomicBool,
    settings_path: &Path,
    enabled: bool,
) -> AppResult<()> {
    settings::save(
        settings_path,
        &Settings {
            resume_enabled: enabled,
        },
    )?;
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
    fn get_resume_enabled_reads_the_atomic_directly() {
        let atomic = Arc::new(AtomicBool::new(false));
        let flag = ResumeFlag(atomic.clone());

        assert!(!flag.0.load(Ordering::Relaxed));
        atomic.store(true, Ordering::Relaxed);
        assert!(flag.0.load(Ordering::Relaxed));
    }
}
