pub mod commands;
pub mod error;
pub mod logging;
pub mod provider;
pub mod store;
pub mod transfer;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use commands::{
    add_connection, cancel_transfer, clean_checkpoint_residue, clear_finished_transfers,
    create_folder, delete_connection, delete_objects, delete_prefix, enqueue_download,
    enqueue_folder_download, enqueue_uploads, get_resume_enabled, get_settings, head_object,
    list_buckets, list_connections, list_objects, list_transfers, pause_transfer, presign_get,
    rename_object, resume_transfer, retry_transfer, set_max_parts, set_max_tasks,
    set_resume_enabled, set_share_expiry, test_connection, update_connection, AppState, ResumeFlag,
};
use tauri::{AppHandle, Emitter, Manager};

use crate::transfer::{
    spawn_aggregator, DispatchRunner, DownloadRunner, EngineConfig, ProgressPayload, ProgressSink,
    TransferEngine, TransferSink, TransferTaskDto, UploadRunner,
};

/// Event names. `:` and `/` are both legal in Tauri event names; the
/// `transfer://` prefix keeps the app's events obviously namespaced.
const EVENT_PROGRESS: &str = "transfer://progress";
const EVENT_STATE: &str = "transfer://state";

/// Pushes throttled progress batches to the webview.
struct TauriProgressSink(AppHandle);

impl ProgressSink for TauriProgressSink {
    fn flush(&self, batch: Vec<ProgressPayload>) {
        // An emit failure means the webview is gone (shutting down); there is
        // nothing useful to do, and panicking inside the aggregator would take
        // the whole progress stream down.
        if let Err(err) = self.0.emit(EVENT_PROGRESS, batch) {
            tracing::debug!("progress emit failed: {err}");
        }
    }
}

/// Pushes state transitions to the webview, unthrottled.
struct TauriStateSink(AppHandle);

impl TransferSink for TauriStateSink {
    fn state_changed(&self, task: &TransferTaskDto) {
        if let Err(err) = self.0.emit(EVENT_STATE, task) {
            tracing::debug!("state emit failed: {err}");
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Logging first: everything below this line is worth logging, and
            // a failure here must not take the app down -- a desktop app that
            // refuses to start because it cannot open a log file is worse
            // than one that runs without logs.
            match app.path().app_log_dir() {
                Ok(log_dir) => match logging::init(&log_dir) {
                    Ok(guard) => {
                        app.manage(logging::LogGuard(std::sync::Mutex::new(Some(guard))));
                        tracing::info!(
                            version = env!("CARGO_PKG_VERSION"),
                            dir = %log_dir.display(),
                            "bucketcat starting"
                        );
                    }
                    Err(err) => eprintln!("bucketcat: logging disabled: {err}"),
                },
                Err(err) => eprintln!("bucketcat: no log directory: {err}"),
            }

            let config_dir = app.path().app_config_dir()?;
            let state = AppState::new(config_dir.clone())?;
            app.manage(state);

            // The engine holds the hub directly rather than going through
            // Tauri state -- it must be usable outside the IPC layer
            // (design §3 principle 4).
            let hub = app.state::<AppState>().hub();
            let handle = app.handle().clone();
            // `spawn_aggregator` calls `tokio::spawn` internally, which
            // panics without a live Tokio runtime on the current thread.
            // `setup` runs before Tauri's own runtime is entered, so this has
            // to be driven through `async_runtime::block_on` rather than
            // called directly.
            let progress_tx = tauri::async_runtime::block_on(async {
                spawn_aggregator(Arc::new(TauriProgressSink(handle.clone())))
            });
            // The persisted resume flag gates both the checkpoint *writer*
            // (threaded into the engine) and the startup *restore* below. One
            // atomic feeds both, read from the same config dir the SecureStore
            // uses, so turning resume off in settings both stops writing and
            // skips rebuilding.
            let settings = store::settings::load(&config_dir.join("settings.json"));
            let resume_enabled = Arc::new(AtomicBool::new(settings.resume_enabled));
            // `ResumeFlag` wraps a clone of the *same* `Arc` handed to the
            // engine below -- one atomic, read by the engine's checkpoint
            // gate and read/written by the `get_resume_enabled`/
            // `set_resume_enabled` commands, never two independent flags
            // (see `commands::settings`'s module doc for why that sharing
            // matters).
            app.manage(ResumeFlag(resume_enabled.clone()));
            // Checkpoints live under the app data dir.
            let checkpoint_dir = transfer::checkpoint::checkpoint_dir(&app.path().app_data_dir()?);
            let engine = TransferEngine::new(
                hub.clone(),
                Arc::new(DispatchRunner {
                    upload: Arc::new(UploadRunner),
                    download: Arc::new(DownloadRunner),
                }),
                Arc::new(TauriStateSink(handle)),
                progress_tx,
                EngineConfig {
                    max_tasks: settings.max_tasks,
                    max_parts: settings.max_parts,
                },
                Some(checkpoint_dir.clone()),
                resume_enabled.clone(),
            );

            // Startup restore (M4c Task 6): rebuild each unfinished transfer as
            // a `Paused` row, offline, and discard any whose connection is gone.
            // Delegated to `transfer::restore_all` -- the single shared entry
            // point the e2e's cross-restart tests also route through, so the
            // two can never diverge on the safety-critical error path (a read
            // failure leaves every checkpoint untouched rather than mistaking
            // them all for orphans and deleting them).
            //
            // Same `block_on` reason as `spawn_aggregator` above: `setup` runs
            // before Tauri's runtime is entered, and the restore awaits the
            // task lock. Gated by `resume_enabled`, exactly as before.
            if resume_enabled.load(Ordering::Relaxed) {
                tauri::async_runtime::block_on(async {
                    transfer::restore_all(&engine, &hub, &checkpoint_dir).await;
                });
            }

            app.manage(engine);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_connections,
            add_connection,
            update_connection,
            delete_connection,
            test_connection,
            list_buckets,
            list_objects,
            delete_objects,
            delete_prefix,
            rename_object,
            create_folder,
            presign_get,
            head_object,
            enqueue_uploads,
            enqueue_download,
            enqueue_folder_download,
            list_transfers,
            pause_transfer,
            resume_transfer,
            cancel_transfer,
            retry_transfer,
            clear_finished_transfers,
            get_resume_enabled,
            set_resume_enabled,
            get_settings,
            set_max_tasks,
            set_max_parts,
            set_share_expiry,
            clean_checkpoint_residue
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // `Builder::run` (the one-liner this replaces) is just
    // `self.build(context)?.run(|_, _| {})` -- and `App::run`'s own docs say
    // it *never returns*: when the app finishes, the process is exited
    // directly via `std::process::exit`, which does not run `Drop` glue. Left
    // alone, that means the `WorkerGuard` inside `LogGuard` never flushes its
    // last lines. `RunEvent::Exit` is dispatched to this callback before that
    // exit call, so this is the one place we get to flush deliberately. Do
    // not delete this as dead ceremony -- without it, Finding 1's whole fix
    // is a no-op.
    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            // `try_state`, not `state`: if logging init failed above (see the
            // `setup` match), nothing was ever `manage`d and `state::<LogGuard>()`
            // would panic here, during shutdown.
            if let Some(guard) = app_handle.try_state::<logging::LogGuard>() {
                if let Ok(mut guard) = guard.0.lock() {
                    // Drops the WorkerGuard, flushing the appender's queue.
                    guard.take();
                }
            }
        }
    });
}
