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
}

fn default_true() -> bool {
    true
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

impl Default for Settings {
    fn default() -> Self {
        Self {
            resume_enabled: true,
            max_tasks: default_tasks(),
            max_parts: default_parts(),
            share_expiry_secs: default_expiry(),
            close_to_tray: true,
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
}

/// Clamps a caller-requested max-concurrent-tasks setting to `[1, 5]`.
pub fn clamp_tasks(n: usize) -> usize {
    n.clamp(1, 5)
}

/// Clamps a caller-requested max-parts-per-task setting to `[1, 8]`.
pub fn clamp_parts(n: usize) -> usize {
    n.clamp(1, 8)
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
            },
        )
        .unwrap();
        let loaded = load(&p);
        assert!(!loaded.resume_enabled);
        assert_eq!(loaded.max_tasks, 2);
        assert_eq!(loaded.max_parts, 6);
        assert_eq!(loaded.share_expiry_secs, 120);
        assert!(!loaded.close_to_tray);
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
}
