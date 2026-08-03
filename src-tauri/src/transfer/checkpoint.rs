//! 断点续传 checkpoint：每任务一个 JSON，原子写（temp + rename），启动扫描重建。
//! 纯 I/O + 序列化，无网络、无引擎内部依赖。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::transfer::{Direction, ResumeState};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub direction: Direction,
    pub connection_id: String,
    pub bucket: String,
    pub key: String,
    pub local_path: String,
    pub file_name: String,
    pub total: u64,
    pub resume: ResumeState,
}

/// `<base>/checkpoints`，不存在则创建。
pub fn checkpoint_dir(base: &Path) -> PathBuf {
    let dir = base.join("checkpoints");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// `pub(crate)` so callers outside this module (e.g. the settings command's
/// residue cleanup) that need the on-disk path a checkpoint lives at can
/// reuse this rather than re-deriving the `<id>.json` naming convention.
pub(crate) fn path_for(dir: &Path, task_id: &str) -> PathBuf {
    dir.join(format!("{task_id}.json"))
}

/// 原子写：写 `<id>.json.tmp` → flush → rename。
pub fn write(dir: &Path, task_id: &str, cp: &Checkpoint) -> AppResult<()> {
    let final_path = path_for(dir, task_id);
    let tmp = final_path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(cp).map_err(|e| AppError::Internal {
        message: format!("serialize checkpoint for {task_id}: {e}"),
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
    std::fs::rename(&tmp, &final_path).map_err(|e| AppError::FileIo {
        path: final_path.display().to_string(),
        message: e.to_string(),
    })
}

/// 尽力删除（不存在不算错）。
pub fn remove(dir: &Path, task_id: &str) {
    let p = path_for(dir, task_id);
    if let Err(e) = std::fs::remove_file(&p) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %p.display(), "removing checkpoint failed: {e}");
        }
    }
}

/// 扫描 `*.json`；反序列化失败的跳过 + warn + 删除；忽略 `.tmp`。
pub fn scan(dir: &Path) -> Vec<(String, Checkpoint)> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out, // 目录不存在 = 无残留
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|x| x != "json").unwrap_or(true) {
            continue; // 忽略 .tmp 及其它
        }
        let id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        match std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Checkpoint>(&b).ok())
        {
            Some(cp) => out.push((id, cp)),
            None => {
                tracing::warn!(path = %path.display(), "corrupt checkpoint discarded");
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::{MultipartState, ResumeState};

    fn upload_cp() -> Checkpoint {
        Checkpoint {
            direction: Direction::Upload,
            connection_id: "c1".into(),
            bucket: "b".into(),
            key: "k".into(),
            local_path: "/tmp/x".into(),
            file_name: "x".into(),
            total: 100,
            resume: ResumeState::Upload(MultipartState {
                upload_id: "u1".into(),
                completed: vec![],
                source_size: 100,
                source_mtime: 42,
                part_size: 16 * 1024 * 1024,
            }),
        }
    }

    #[test]
    fn write_then_scan_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "task-a", &upload_cp()).unwrap();
        let found = scan(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "task-a");
        assert_eq!(found[0].1.key, "k");
        // 原子写：不留 .tmp
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "tmp").unwrap_or(false))
            .collect();
        assert!(leftovers.is_empty(), "no .tmp must remain");
    }

    #[test]
    fn scan_skips_and_removes_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "good", &upload_cp()).unwrap();
        std::fs::write(dir.path().join("bad.json"), b"{ not json").unwrap();
        let found = scan(dir.path());
        assert_eq!(found.len(), 1, "only the good one survives");
        assert!(
            !dir.path().join("bad.json").exists(),
            "corrupt file is removed"
        );
    }

    #[test]
    fn write_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t", &upload_cp()).unwrap();
        let mut cp = upload_cp();
        cp.key = "k2".into();
        write(dir.path(), "t", &cp).unwrap();
        let found = scan(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.key, "k2");
    }
}
