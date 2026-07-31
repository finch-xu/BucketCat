//! In-app update: source listing, check, download+install, restart.
//!
//! The frontend never talks to `tauri-plugin-updater` directly. It calls the
//! commands here, which is what lets the update source be switched at runtime:
//! the plugin freezes `tauri.conf.json`'s `endpoints` when it is registered,
//! so a persisted choice could otherwise only take effect after a restart.
//! [`build_updater`] sidesteps that by constructing a throwaway
//! `UpdaterBuilder` per call and injecting the endpoint into it, which also
//! means the settings dropdown takes effect on the very next check.
//!
//! Keeping it all on this side has two knock-on benefits: `capabilities/`
//! needs no `updater:*` entry (Tauri's ACL only gates *plugin* commands, and
//! the webview never invokes one), and `src/lib/api.ts` stays the single place
//! the frontend does IPC.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::UpdaterExt;

use crate::commands::settings::settings_path;
use crate::error::{AppError, AppResult};
use crate::logging::LogGuard;
use crate::store::settings;
use crate::updater_source::{self, SOURCES};

/// Download-progress event, mirroring `transfer://progress`'s namespacing.
const EVENT_PROGRESS: &str = "update://progress";

/// One entry of the built-in source dropdown.
///
/// `manifest_url` is sent along so the settings pane can show users which host
/// their app will talk to. They cannot edit it -- see `updater_source`.
#[derive(Debug, Serialize, Clone)]
pub struct UpdateSourceDto {
    pub id: String,
    pub manifest_url: String,
    pub release_page_url: String,
}

/// What a successful check found, trimmed down from the plugin's `Update`.
#[derive(Debug, Serialize, Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    /// Release notes from the manifest's `notes` field. Frequently empty (the
    /// manifest is generated against a draft release whose body has not been
    /// written yet), so the UI must render fine without it.
    pub body: Option<String>,
    /// Whether this install can replace itself in place. See [`installable`].
    pub installable: bool,
}

/// `update://progress` payload. Order is Started once, Progress many, Finished
/// once.
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum UpdaterProgress {
    Started { content_length: Option<u64> },
    Progress { chunk_length: u64 },
    Finished,
}

/// Whether the running install can apply an update to itself.
///
/// Only Linux says no, and only sometimes: Tauri can swap an AppImage in
/// place, but a `.deb`/`.rpm` install is owned by the system package manager
/// and `download_and_install` would fail partway. `APPIMAGE` is set by the
/// AppImage runtime itself, so its absence is the reliable signal. Computing
/// this here rather than in TypeScript keeps the manifest URL and the version
/// comparison from being reimplemented on the frontend.
fn installable() -> bool {
    if cfg!(target_os = "linux") {
        std::env::var_os("APPIMAGE").is_some()
    } else {
        true
    }
}

/// Builds a one-shot updater pointed at whichever source is persisted.
///
/// An unknown persisted id leaves the endpoint alone, falling back to
/// `tauri.conf.json` -- see [`updater_source::manifest_url_for`] for why that
/// beats erroring.
fn build_updater(app: &AppHandle) -> AppResult<tauri_plugin_updater::Updater> {
    let source = settings::load(&settings_path(app)?).update_source;
    let mut builder = app.updater_builder();

    if let Some(url) = updater_source::manifest_url_for(&source) {
        let parsed = url.parse().map_err(|e| AppError::UpdateCheckFailed {
            message: format!("invalid manifest URL '{url}': {e}"),
        })?;
        builder = builder
            .endpoints(vec![parsed])
            .map_err(|e| AppError::UpdateCheckFailed {
                message: format!("set updater endpoint: {e}"),
            })?;
        tracing::info!(source = %source, url = %url, "updater endpoint overridden");
    } else {
        tracing::info!(
            source = %source,
            "unknown update source; falling back to the tauri.conf.json endpoint"
        );
    }

    builder.build().map_err(|e| AppError::UpdateCheckFailed {
        message: format!("build updater: {e}"),
    })
}

/// The built-in update sources, in display order.
#[tauri::command]
pub fn list_update_sources() -> Vec<UpdateSourceDto> {
    SOURCES
        .iter()
        .map(|s| UpdateSourceDto {
            id: s.id.to_string(),
            manifest_url: s.manifest_url.to_string(),
            release_page_url: s.release_page_url.to_string(),
        })
        .collect()
}

/// Checks the configured source. `None` means already up to date.
#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> AppResult<Option<UpdateInfo>> {
    let updater = build_updater(&app)?;
    let found = updater
        .check()
        .await
        .map_err(|e| {
            tracing::warn!("update check failed: {e}");
            AppError::UpdateCheckFailed {
                message: e.to_string(),
            }
        })?;

    Ok(found.map(|u| {
        tracing::info!(from = %u.current_version, to = %u.version, "update available");
        UpdateInfo {
            version: u.version.clone(),
            current_version: u.current_version.clone(),
            body: u.body.clone(),
            installable: installable(),
        }
    }))
}

/// Downloads and applies the update, streaming progress over
/// [`EVENT_PROGRESS`].
///
/// Does **not** restart afterwards. BucketCat may be mid-transfer, and only
/// the user knows when it is safe to go down -- the settings pane blocks this
/// command entirely while transfers are active and offers an explicit restart
/// button once it succeeds. (On Windows this is moot: the NSIS installer takes
/// over and terminates the process itself, which is exactly why the transfer
/// guard matters most there.)
#[tauri::command]
pub async fn download_install_update(app: AppHandle) -> AppResult<()> {
    if !installable() {
        return Err(AppError::UpdateInstallFailed {
            message: "this install medium cannot self-update".to_string(),
        });
    }

    // Re-checked rather than carried over from `check_for_update`: the
    // plugin's `Update` holds the resolved download URL and signature and is
    // not storable in managed state, and one extra request against a static
    // JSON file is a fair price for not inventing a cache with its own
    // staleness rules.
    let updater = build_updater(&app)?;
    let Some(update) = updater.check().await.map_err(|e| {
        tracing::warn!("re-check before install failed: {e}");
        AppError::UpdateCheckFailed {
            message: e.to_string(),
        }
    })?
    else {
        tracing::warn!("download_install_update called with nothing to install");
        return Ok(());
    };

    // `Started` is emitted from inside the first chunk callback rather than
    // before the call, because `content_length` only becomes known once the
    // response headers are in.
    let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let app_chunk = app.clone();
    let app_done = app.clone();

    update
        .download_and_install(
            move |chunk_length, content_length| {
                if !started.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let _ = app_chunk.emit(
                        EVENT_PROGRESS,
                        UpdaterProgress::Started { content_length },
                    );
                }
                let _ = app_chunk.emit(
                    EVENT_PROGRESS,
                    UpdaterProgress::Progress {
                        chunk_length: chunk_length as u64,
                    },
                );
            },
            move || {
                let _ = app_done.emit(EVENT_PROGRESS, UpdaterProgress::Finished);
            },
        )
        .await
        .map_err(|e| {
            tracing::error!("update install failed: {e}");
            AppError::UpdateInstallFailed {
                message: e.to_string(),
            }
        })?;

    tracing::info!("update installed; awaiting user-initiated restart");
    Ok(())
}

/// Restarts into the freshly installed version.
///
/// Flushes the log appender first. `lib.rs` normally does that from the
/// `RunEvent::Exit` arm, but `AppHandle::restart` re-executes the binary
/// without guaranteeing that arm runs -- and the lines written immediately
/// before an update restart are precisely the ones worth having if the new
/// build fails to come up.
#[tauri::command]
pub fn restart_app(app: AppHandle) {
    tracing::info!("restarting to apply the update");
    if let Some(guard) = app.try_state::<LogGuard>() {
        if let Ok(mut guard) = guard.0.lock() {
            guard.take();
        }
    }
    app.restart()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_source_is_offered_to_the_frontend() {
        let listed = list_update_sources();
        assert_eq!(listed.len(), SOURCES.len());
        for (dto, src) in listed.iter().zip(SOURCES.iter()) {
            assert_eq!(dto.id, src.id);
            assert_eq!(dto.manifest_url, src.manifest_url);
            assert_eq!(dto.release_page_url, src.release_page_url);
        }
    }

    #[test]
    fn progress_serializes_with_a_phase_tag() {
        // The frontend switches on `phase`; renaming it silently breaks the
        // progress bar without breaking the build.
        let started = serde_json::to_value(UpdaterProgress::Started {
            content_length: Some(42),
        })
        .unwrap();
        assert_eq!(started["phase"], "started");
        assert_eq!(started["content_length"], 42);

        let progress =
            serde_json::to_value(UpdaterProgress::Progress { chunk_length: 7 }).unwrap();
        assert_eq!(progress["phase"], "progress");
        assert_eq!(progress["chunk_length"], 7);

        assert_eq!(
            serde_json::to_value(UpdaterProgress::Finished).unwrap()["phase"],
            "finished"
        );
    }

    #[test]
    fn non_linux_targets_can_always_self_update() {
        // On Linux the answer depends on the APPIMAGE env var of the running
        // process, which a test must not fabricate; everywhere else it is a
        // constant and worth pinning.
        if !cfg!(target_os = "linux") {
            assert!(installable());
        }
    }
}
