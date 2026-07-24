//! The `.bcpart` staging file for downloads (design §5): bytes land at their
//! Range offset in a preallocated temp file, and only a fully-written file is
//! renamed into place -- so a crash or cancel never leaves a truncated file
//! masquerading as a complete download.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

/// The staging path for `final_path`: the same path plus a `.bcpart` suffix,
/// so it lands on the same filesystem and the finish `rename` is atomic.
pub fn bcpart_path(final_path: &Path) -> PathBuf {
    let mut s = final_path.as_os_str().to_os_string();
    s.push(".bcpart");
    PathBuf::from(s)
}

/// A download's staging file. Concurrent [`PartFile::write_at`] calls are
/// safe because they use positioned writes (`write_at`/`seek_write`), which
/// do not share the file's seek cursor.
#[derive(Debug)]
pub struct PartFile {
    file: File,
    bcpart: PathBuf,
    target: PathBuf,
}

impl PartFile {
    /// Creates `<final_path>.bcpart` (creating parent dirs) and preallocates
    /// it to `total` bytes so every chunk's offset is already valid.
    pub fn create(final_path: &Path, total: u64) -> AppResult<Self> {
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| file_io(final_path, err))?;
        }
        let bcpart = bcpart_path(final_path);
        let file = File::create(&bcpart).map_err(|err| file_io(&bcpart, err))?;
        file.set_len(total).map_err(|err| file_io(&bcpart, err))?;
        Ok(Self {
            file,
            bcpart,
            target: final_path.to_path_buf(),
        })
    }

    /// Writes `data` at `offset`. Safe to call concurrently from many tasks.
    pub fn write_at(&self, offset: u64, data: &[u8]) -> AppResult<()> {
        write_at_impl(&self.file, offset, data).map_err(|err| file_io(&self.bcpart, err))
    }

    /// Flushes and renames the staging file onto the target. Consumes `self`
    /// so a finished download can't be written to again.
    pub fn finish(self) -> AppResult<()> {
        self.file
            .sync_all()
            .map_err(|err| file_io(&self.bcpart, err))?;
        std::fs::rename(&self.bcpart, &self.target).map_err(|err| file_io(&self.target, err))?;
        Ok(())
    }

    /// Deletes the staging file, best-effort. Called on cancel/failure; a
    /// failure to clean up is logged by the caller, never fatal.
    pub fn abort(self) {
        let _ = std::fs::remove_file(&self.bcpart);
    }

    /// Reopens an existing `.bcpart` for a resumed download. Does NOT
    /// `set_len` (the file already holds the finished chunks) and does not
    /// truncate. Fails with `local/file-io` if the staging file vanished
    /// between the pause and the resume.
    pub fn reopen(final_path: &Path, _total: u64, bcpart: &Path) -> AppResult<Self> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(bcpart)
            .map_err(|err| file_io(bcpart, err))?;
        Ok(Self {
            file,
            bcpart: bcpart.to_path_buf(),
            target: final_path.to_path_buf(),
        })
    }

    /// This staging file's `.bcpart` path, for the runner to record in
    /// `DownloadState` so a later cancel/resume can find it.
    pub fn bcpart_path(&self) -> &Path {
        &self.bcpart
    }
}

fn file_io(path: &Path, err: std::io::Error) -> AppError {
    AppError::FileIo {
        path: path.display().to_string(),
        message: err.to_string(),
    }
}

#[cfg(unix)]
fn write_at_impl(file: &File, offset: u64, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(data, offset)
}

#[cfg(windows)]
fn write_at_impl(file: &File, offset: u64, data: &[u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    // `seek_write` may write short like `write`, so loop until the whole
    // buffer is placed. Unlike a shared cursor, each call is positioned, so
    // concurrent writers to disjoint regions never interfere.
    let mut written = 0usize;
    while written < data.len() {
        let n = file.seek_write(&data[written..], offset + written as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "seek_write wrote zero bytes",
            ));
        }
        written += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcpart_path_appends_the_suffix() {
        assert_eq!(
            bcpart_path(std::path::Path::new("/tmp/a/photo.jpg")),
            std::path::PathBuf::from("/tmp/a/photo.jpg.bcpart")
        );
    }

    #[tokio::test]
    async fn concurrent_writes_at_offsets_assemble_the_whole_file() {
        // The whole point of positioned writes: four tasks writing different
        // 4-byte regions of the same file, concurrently, must not corrupt each
        // other (a shared seek cursor would).
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.bin");
        let pf = std::sync::Arc::new(PartFile::create(&target, 16).unwrap());
        assert!(target.with_extension("bin.bcpart").exists());
        assert!(!target.exists(), "target must not appear until finish");

        let mut set = tokio::task::JoinSet::new();
        for i in 0..4u64 {
            let pf = std::sync::Arc::clone(&pf);
            set.spawn(async move {
                let byte = b'a' + i as u8;
                pf.write_at(i * 4, &[byte; 4]).unwrap();
            });
        }
        while set.join_next().await.is_some() {}

        std::sync::Arc::try_unwrap(pf).unwrap().finish().unwrap();
        assert!(target.exists(), "finish must rename into place");
        assert!(!target.with_extension("bin.bcpart").exists(), "bcpart gone");
        let body = std::fs::read(&target).unwrap();
        assert_eq!(&body, b"aaaabbbbccccdddd");
    }

    #[test]
    fn abort_removes_the_bcpart_and_never_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.bin");
        let pf = PartFile::create(&target, 8).unwrap();
        pf.write_at(0, b"partial").unwrap();
        let bcpart = bcpart_path(&target);
        assert!(bcpart.exists());
        pf.abort();
        assert!(!bcpart.exists(), "abort deletes the .bcpart");
        assert!(!target.exists(), "abort never creates the target");
    }

    #[test]
    fn create_preallocates_the_full_length() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.bin");
        let pf = PartFile::create(&target, 4096).unwrap();
        let meta = std::fs::metadata(bcpart_path(&target)).unwrap();
        assert_eq!(meta.len(), 4096, "the .bcpart is preallocated to total");
        pf.abort();
    }
}
