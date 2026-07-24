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
            },
        )
        .unwrap();
        let loaded = load(&p);
        assert!(!loaded.resume_enabled);
        assert_eq!(loaded.max_tasks, 2);
        assert_eq!(loaded.max_parts, 6);
        assert_eq!(loaded.share_expiry_secs, 120);
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
    }

    #[test]
    fn clamps_bound_the_ranges() {
        assert_eq!(clamp_tasks(0), 1);
        assert_eq!(clamp_tasks(99), 5);
        assert_eq!(clamp_parts(0), 1);
        assert_eq!(clamp_parts(99), 8);
    }
}
