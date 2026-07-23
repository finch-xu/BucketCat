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
/// background writer thread, and letting it drop before the process actually
/// exits would silently discard every log line written afterwards.
///
/// It wraps the guard in two layers, each fixing a different problem:
///
/// - `Mutex`: `WorkerGuard` holds an `std::sync::mpsc::Sender`, which is
///   `Send` but not `Sync`, while `Manager::manage` requires
///   `Send + Sync + 'static`.
/// - `Option`: `Builder::run` is `self.build(context)?.run(|_, _| {})`, and
///   `App::run`'s own docs say it *never returns* -- when the app finishes,
///   the process exits directly via `std::process::exit`, which does not run
///   `Drop` glue. So the guard must be dropped explicitly, from the
///   `RunEvent::Exit` callback that `App::run` invokes just before that
///   exit call. You cannot move a value out of Tauri's `State<T>`/`&T`
///   access, so the callback instead locks the `Mutex` and `.take()`s the
///   `Option`, which both moves the guard out and drops it in one step.
pub struct LogGuard(pub std::sync::Mutex<Option<WorkerGuard>>);

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

/// Resolves the filter directives to use: the caller's override when it
/// parses, otherwise [`DEFAULT_FILTER`]. The bool is true when an override
/// was present but rejected, so the caller can tell the user their setting
/// was ignored instead of silently dropping it.
///
/// Decision: an empty-string override (e.g. `BUCKETCAT_LOG=` with nothing
/// after the `=`) is treated as present-but-malformed, not as "unset". Left
/// to itself, `EnvFilter::try_new("")` happily returns `Ok` with zero
/// directives -- silently valid, but almost certainly not what a user who
/// exported an empty override meant. Reporting it as rejected surfaces the
/// mistake instead of hiding it, which is the entire point of this helper.
fn resolve_directives(override_value: Option<&str>) -> (String, bool) {
    match override_value {
        None => (DEFAULT_FILTER.to_string(), false),
        Some(value) if value.trim().is_empty() => (DEFAULT_FILTER.to_string(), true),
        Some(value) => match EnvFilter::try_new(value) {
            Ok(_) => (value.to_string(), false),
            Err(_) => (DEFAULT_FILTER.to_string(), true),
        },
    }
}

/// Reads the filter directives, preferring `BUCKETCAT_LOG` and falling back to
/// [`DEFAULT_FILTER`] when it is unset *or* malformed. A malformed override is
/// reported on stderr by name -- the whole point of the env var is debugging
/// an install, so silently dropping a typo defeats it.
fn filter() -> EnvFilter {
    let override_value = std::env::var(FILTER_ENV).ok();
    let (directives, override_rejected) = resolve_directives(override_value.as_deref());
    if override_rejected {
        eprintln!("bucketcat: ignoring malformed {FILTER_ENV} filter, using default");
    }
    EnvFilter::new(directives)
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

    #[test]
    fn resolve_directives_no_override_uses_default() {
        assert_eq!(
            resolve_directives(None),
            (DEFAULT_FILTER.to_string(), false)
        );
    }

    #[test]
    fn resolve_directives_valid_override_is_used_as_is() {
        assert_eq!(
            resolve_directives(Some("trace")),
            ("trace".to_string(), false)
        );
    }

    #[test]
    fn resolve_directives_malformed_override_falls_back_and_flags_it() {
        // Multiple `=` in a single directive is not valid EnvFilter syntax.
        assert_eq!(
            resolve_directives(Some("bucketcat=lib=info")),
            (DEFAULT_FILTER.to_string(), true)
        );
    }

    #[test]
    fn resolve_directives_empty_override_falls_back_and_flags_it() {
        // Deliberate decision (see resolve_directives' doc comment): an
        // empty override is treated as malformed, not as "unset", because
        // `EnvFilter` would otherwise accept it silently as zero directives.
        assert_eq!(
            resolve_directives(Some("")),
            (DEFAULT_FILTER.to_string(), true)
        );
    }
}
