//! Application logging: a daily-rotating file in the OS log directory.
//!
//! Design §7 requires that raw provider errors go to a log while the UI only
//! ever shows normalized, translated text. This module owns the sink half of
//! that contract.
//!
//! ## Never log credentials
//!
//! `Connection` / `ConnectionInput` have hand-written `Debug` impls that
//! redact `secret_access_key` (M2 Task 3), but that only protects the
//! accidental `{:?}` of a whole struct. The rule every call site must follow
//! is stricter: log the `connection_id`, never the connection.

use std::path::Path;

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::EnvFilter;

/// Default log directives: this crate at INFO, everything else (notably the
/// AWS SDK's very chatty per-request spans) only at WARN and above.
/// Overridable at runtime via the `BUCKETCAT_LOG` env var.
pub const DEFAULT_FILTER: &str = "bucketcat_lib=info,warn";

/// Env var that overrides [`DEFAULT_FILTER`], for debugging a user's install
/// without shipping a new build.
pub const FILTER_ENV: &str = "BUCKETCAT_LOG";

/// Tauri-managed holder for the appender's flush guard.
///
/// The guard must outlive the app: dropping it flushes and shuts down the
/// background writer thread, and letting it drop at the end of `setup` would
/// silently discard every log line written afterwards. `WorkerGuard` holds an
/// `std::sync::mpsc::Sender`, which is `Send` but not `Sync`, so it needs the
/// `Mutex` to satisfy `Manager::manage`'s `Send + Sync + 'static` bound.
pub struct LogGuard(pub std::sync::Mutex<WorkerGuard>);

/// Creates `log_dir` if needed and returns a non-blocking, daily-rotating
/// writer for `bucketcat.log` plus its flush guard.
///
/// Split out of [`init`] purely for testability: `init` installs a *global*
/// subscriber, which can only ever happen once per process, so it cannot be
/// exercised by more than one `#[test]`. This half can.
pub fn build_writer(log_dir: &Path) -> std::io::Result<(NonBlocking, WorkerGuard)> {
    std::fs::create_dir_all(log_dir)?;
    let appender = tracing_appender::rolling::daily(log_dir, "bucketcat.log");
    Ok(tracing_appender::non_blocking(appender))
}

/// Reads the filter directives, preferring `BUCKETCAT_LOG` and falling back to
/// [`DEFAULT_FILTER`] when it is unset *or* malformed.
fn filter() -> EnvFilter {
    EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

/// Installs the global subscriber writing into `log_dir`. Call exactly once,
/// from Tauri's `setup`. The returned guard must be kept alive for the
/// process's lifetime -- see [`LogGuard`].
pub fn init(log_dir: &Path) -> std::io::Result<WorkerGuard> {
    let (writer, guard) = build_writer(log_dir)?;
    tracing_subscriber::fmt()
        .with_env_filter(filter())
        .with_writer(writer)
        // The file is read by humans and by `grep`; ANSI escapes help neither.
        .with_ansi(false)
        .with_target(true)
        .init();
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_writer_creates_dir_and_writes_events() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("nested").join("logs");
        assert!(!log_dir.exists());

        let (writer, guard) = build_writer(&log_dir).unwrap();
        assert!(log_dir.is_dir());

        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(marker = "bc-log-probe", "probe event");
        });
        // Dropping the guard flushes the non-blocking writer's queue; without
        // it the assertion below races the background writer thread.
        drop(guard);

        let files: Vec<_> = std::fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(files.len(), 1, "expected exactly one daily log file");
        let body = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(body.contains("bc-log-probe"), "log body was: {body}");
        assert!(body.contains("probe event"), "log body was: {body}");
    }

    #[test]
    fn default_filter_parses() {
        // A malformed DEFAULT_FILTER would silently disable all logging at
        // runtime, so pin that it is a valid directive string.
        assert!(tracing_subscriber::EnvFilter::try_new(DEFAULT_FILTER).is_ok());
    }
}
