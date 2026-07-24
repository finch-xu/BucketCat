//! 应用设置的持久化。M4c 仅有 `resume_enabled`；M6 会扩展。缺失/损坏一律回退默认，
//! 一个坏文件不该悄悄改变行为。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_true")]
    pub resume_enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self { resume_enabled: true }
    }
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
        save(&p, &Settings { resume_enabled: false }).unwrap();
        assert!(!load(&p).resume_enabled);
        // 原子写不留 .tmp
        assert!(!p.with_extension("json.tmp").exists());
    }
}
