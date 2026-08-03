//! 应用设置的持久化。M4c 仅有 `resume_enabled`；M6 会扩展。缺失/损坏一律回退默认，
//! 一个坏文件不该悄悄改变行为。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_true")]
    pub resume_enabled: bool,
    /// Tasks allowed to be `Running` at once (M6c); read by the engine at
    /// construction, clamped via [`clamp_tasks`].
    #[serde(default = "default_tasks")]
    pub max_tasks: usize,
    /// Parts a single task may have in flight (M6c); read by the engine at
    /// construction, clamped via [`clamp_parts`].
    #[serde(default = "default_parts")]
    pub max_parts: usize,
    /// Default presigned-URL lifetime offered by the Share feature, in
    /// seconds (M6c); frontend-consumed, clamped the same way
    /// `provider::clamp_expiry` bounds an explicit per-call value.
    #[serde(default = "default_expiry")]
    pub share_expiry_secs: u64,
    /// Closing the main window hides it to the tray instead of quitting, so
    /// in-flight transfers survive (M7). Read once at startup into the runtime
    /// atomic behind `commands::CloseToTrayFlag`, which the window's
    /// `CloseRequested` handler consults -- see that module's doc comment.
    /// Defaults to `true`: a close that silently kills a running upload is the
    /// worse surprise of the two.
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    /// Which built-in update source to fetch the manifest from. Values are the
    /// `id`s in [`crate::updater_source::SOURCES`]; an unknown value (a
    /// hand-edited file, or a source removed in a later version) falls back to
    /// the endpoint baked into `tauri.conf.json` rather than erroring, so a
    /// stale settings file can never lock a user out of updates entirely.
    #[serde(default = "default_update_source")]
    pub update_source: String,
    /// Check for a new version once on startup. The result only ever lights a
    /// dot on the settings entry -- nothing is downloaded and no dialog
    /// appears -- so this defaults on.
    #[serde(default = "default_true")]
    pub auto_check_update: bool,
    /// Which transfer tuning preset to use: "conservative", "balanced", or
    /// "aggressive". Defaults to "balanced".
    #[serde(default = "default_transfer_preset")]
    pub transfer_preset: String,
    /// File upload threshold in bytes; files below this use single-part
    /// PutObject. Clamped via [`clamp_threshold`].
    #[serde(default = "default_upload_threshold")]
    pub upload_threshold: u64,
    /// Lower bound on computed upload part size in bytes. Clamped via
    /// [`clamp_part_floor`].
    #[serde(default = "default_upload_part_floor")]
    pub upload_part_floor: u64,
    /// Target number of upload parts. Clamped via [`clamp_target_parts`].
    #[serde(default = "default_upload_target_parts")]
    pub upload_target_parts: u64,
    /// File download threshold in bytes; files below this download as a single
    /// Range GET. Clamped via [`clamp_threshold`].
    #[serde(default = "default_download_threshold")]
    pub download_threshold: u64,
    /// Lower bound on computed download chunk size in bytes. Clamped via
    /// [`clamp_part_floor`].
    #[serde(default = "default_download_chunk_floor")]
    pub download_chunk_floor: u64,
    /// Target number of download chunks. Clamped via [`clamp_target_parts`].
    #[serde(default = "default_download_target_parts")]
    pub download_target_parts: u64,
}

fn default_true() -> bool {
    true
}

fn default_update_source() -> String {
    crate::updater_source::DEFAULT_SOURCE.to_string()
}

fn default_tasks() -> usize {
    3
}

fn default_parts() -> usize {
    4
}

fn default_expiry() -> u64 {
    3600
}

fn default_transfer_preset() -> String {
    "balanced".to_string()
}

fn default_upload_threshold() -> u64 {
    crate::transfer::TransferTuning::balanced().upload_threshold
}

fn default_upload_part_floor() -> u64 {
    crate::transfer::TransferTuning::balanced().upload_part_floor
}

fn default_upload_target_parts() -> u64 {
    crate::transfer::TransferTuning::balanced().upload_target_parts
}

fn default_download_threshold() -> u64 {
    crate::transfer::TransferTuning::balanced().download_threshold
}

fn default_download_chunk_floor() -> u64 {
    crate::transfer::TransferTuning::balanced().download_chunk_floor
}

fn default_download_target_parts() -> u64 {
    crate::transfer::TransferTuning::balanced().download_target_parts
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            resume_enabled: true,
            max_tasks: default_tasks(),
            max_parts: default_parts(),
            share_expiry_secs: default_expiry(),
            close_to_tray: true,
            update_source: default_update_source(),
            auto_check_update: true,
            transfer_preset: default_transfer_preset(),
            upload_threshold: default_upload_threshold(),
            upload_part_floor: default_upload_part_floor(),
            upload_target_parts: default_upload_target_parts(),
            download_threshold: default_download_threshold(),
            download_chunk_floor: default_download_chunk_floor(),
            download_target_parts: default_download_target_parts(),
        }
    }
}

impl Settings {
    /// Returns the `(max_tasks, max_parts)` the transfer engine should be
    /// constructed with, clamped to the same `[1, 5]` / `[1, 8]` ranges the
    /// setter commands enforce on the write path.
    ///
    /// The write path already clamps, but `load` deliberately does not (it
    /// only deserializes and falls back to defaults on a missing/corrupt
    /// file), so a hand-edited or externally-written `settings.json` can
    /// still hold an out-of-range value. Clamping here — at the single point
    /// where settings cross into the engine — keeps `max_tasks: 0` from
    /// building a zero-permit `Semaphore` that would silently deadlock every
    /// transfer, and keeps `max_tasks: 999` from bypassing the documented
    /// concurrency cap. Mirrors the `.max(1)` guard the part limit already
    /// has at its own consume site.
    pub fn engine_bounds(&self) -> (usize, usize) {
        (clamp_tasks(self.max_tasks), clamp_parts(self.max_parts))
    }

    /// Returns a [`TransferTuning`](crate::transfer::TransferTuning) with all
    /// fields clamped to their valid ranges. Analogous to [`engine_bounds`]:
    /// the write path clamps, but a hand-edited `settings.json` can hold
    /// out-of-range values.
    pub fn tuning(&self) -> crate::transfer::TransferTuning {
        crate::transfer::TransferTuning {
            upload_threshold: clamp_threshold(self.upload_threshold),
            upload_part_floor: clamp_part_floor(self.upload_part_floor),
            upload_target_parts: clamp_target_parts(self.upload_target_parts),
            download_threshold: clamp_threshold(self.download_threshold),
            download_chunk_floor: clamp_part_floor(self.download_chunk_floor),
            download_target_parts: clamp_target_parts(self.download_target_parts),
        }
    }
}

/// Clamps a caller-requested max-concurrent-tasks setting to `[1, 5]`.
pub fn clamp_tasks(n: usize) -> usize {
    n.clamp(1, 5)
}

/// Clamps a caller-requested max-parts-per-task setting to `[1, 8]`.
pub fn clamp_parts(n: usize) -> usize {
    n.clamp(1, 8)
}

/// Clamps a transfer threshold (upload or download) to `[16MB, 1GB]`.
pub fn clamp_threshold(n: u64) -> u64 {
    const MB: u64 = 1024 * 1024;
    n.clamp(16 * MB, 1024 * MB)
}

/// Clamps a transfer part floor (upload or download) to `[8MB, 256MB]`.
pub fn clamp_part_floor(n: u64) -> u64 {
    const MB: u64 = 1024 * 1024;
    n.clamp(8 * MB, 256 * MB)
}

/// Clamps a transfer target parts count to `[4, 1000]`.
pub fn clamp_target_parts(n: u64) -> u64 {
    n.clamp(4, 1000)
}

/// 缺失/损坏 → `Settings::default()`（fail-safe）。
pub fn load(path: &Path) -> Settings {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
        .unwrap_or_default()
}

/// 原子写（temp + rename）。
pub fn save(path: &Path, s: &Settings) -> AppResult<()> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(s).map_err(|e| AppError::Internal {
        message: format!("serialize settings: {e}"),
    })?;
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp).map_err(|e| AppError::FileIo {
            path: tmp.display().to_string(),
            message: e.to_string(),
        })?;
        f.write_all(&bytes).map_err(|e| AppError::FileIo {
            path: tmp.display().to_string(),
            message: e.to_string(),
        })?;
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, path).map_err(|e| AppError::FileIo {
        path: path.display().to_string(),
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_defaults_to_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let s = load(&dir.path().join("settings.json"));
        assert!(s.resume_enabled, "default must be true (fail-safe)");
    }

    #[test]
    fn corrupt_file_defaults_to_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, b"not json").unwrap();
        assert!(load(&p).resume_enabled);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        save(
            &p,
            &Settings {
                resume_enabled: false,
                max_tasks: 2,
                max_parts: 6,
                share_expiry_secs: 120,
                close_to_tray: false,
                update_source: "github".to_string(),
                auto_check_update: false,
                transfer_preset: "aggressive".to_string(),
                upload_threshold: 16 * 1024 * 1024,
                upload_part_floor: 8 * 1024 * 1024,
                upload_target_parts: 100,
                download_threshold: 16 * 1024 * 1024,
                download_chunk_floor: 8 * 1024 * 1024,
                download_target_parts: 64,
            },
        )
        .unwrap();
        let loaded = load(&p);
        assert!(!loaded.resume_enabled);
        assert_eq!(loaded.max_tasks, 2);
        assert_eq!(loaded.max_parts, 6);
        assert_eq!(loaded.share_expiry_secs, 120);
        assert!(!loaded.close_to_tray);
        assert_eq!(loaded.update_source, "github");
        assert!(!loaded.auto_check_update);
        assert_eq!(loaded.transfer_preset, "aggressive");
        assert_eq!(loaded.upload_threshold, 16 * 1024 * 1024);
        assert_eq!(loaded.upload_part_floor, 8 * 1024 * 1024);
        assert_eq!(loaded.upload_target_parts, 100);
        assert_eq!(loaded.download_threshold, 16 * 1024 * 1024);
        assert_eq!(loaded.download_chunk_floor, 8 * 1024 * 1024);
        assert_eq!(loaded.download_target_parts, 64);
        // 原子写不留 .tmp
        assert!(!p.with_extension("json.tmp").exists());
    }

    #[test]
    fn old_file_without_new_fields_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        std::fs::write(&p, br#"{"resume_enabled":true}"#).unwrap(); // 旧文件
        let s = load(&p);
        assert_eq!(s.max_tasks, 3);
        assert_eq!(s.max_parts, 4);
        assert_eq!(s.share_expiry_secs, 3600);
        // A settings.json written before M7 has no `close_to_tray` key at
        // all; it must read back as enabled, not as `false`.
        assert!(s.close_to_tray);
        // Same contract for the updater fields: an existing install that
        // upgrades into the updater release must land on the GitHub source
        // with auto-check on, not on an empty source string that would make
        // every check fall back to the baked-in endpoint by accident.
        assert_eq!(s.update_source, crate::updater_source::DEFAULT_SOURCE);
        assert!(s.auto_check_update);
    }

    #[test]
    fn clamps_bound_the_ranges() {
        assert_eq!(clamp_tasks(0), 1);
        assert_eq!(clamp_tasks(99), 5);
        assert_eq!(clamp_parts(0), 1);
        assert_eq!(clamp_parts(99), 8);
    }

    #[test]
    fn engine_bounds_clamp_out_of_range_persisted_values() {
        // A hand-edited `settings.json` can hold values the write-path
        // clamps never saw. `max_tasks: 0` must not reach the engine (a
        // zero-permit semaphore deadlocks every transfer); `999` must not
        // bypass the documented cap.
        let low = Settings {
            max_tasks: 0,
            max_parts: 0,
            ..Default::default()
        };
        assert_eq!(low.engine_bounds(), (1, 1));
        let high = Settings {
            max_tasks: 999,
            max_parts: 999,
            ..Default::default()
        };
        assert_eq!(high.engine_bounds(), (5, 8));
        // In-range values pass through untouched.
        let ok = Settings {
            max_tasks: 3,
            max_parts: 4,
            ..Default::default()
        };
        assert_eq!(ok.engine_bounds(), (3, 4));
    }

    #[test]
    fn old_file_defaults_to_balanced_tuning() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        std::fs::write(&p, br#"{"resume_enabled":true}"#).unwrap();
        let s = load(&p);
        assert_eq!(s.transfer_preset, "balanced");
        assert_eq!(s.tuning(), crate::transfer::TransferTuning::balanced());
    }

    #[test]
    fn tuning_clamps_hand_edited_values() {
        let mut s = Settings::default();
        s.upload_threshold = 1;            // 低于 16MB 下限
        s.download_target_parts = 999_999; // 高于 1000 上限
        let t = s.tuning();
        assert_eq!(t.upload_threshold, 16 * 1024 * 1024);
        assert_eq!(t.download_target_parts, 1000);
    }
}
