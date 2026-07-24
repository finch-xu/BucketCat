pub mod commands;
pub mod error;
pub mod logging;
pub mod provider;
pub mod store;
pub mod transfer;

use std::sync::Arc;

use commands::{
    add_connection, cancel_transfer, clear_finished_transfers, create_folder, delete_connection,
    delete_objects, delete_prefix, enqueue_download, enqueue_uploads, list_buckets,
    list_connections, list_objects, list_transfers, pause_transfer, rename_object, resume_transfer,
    retry_transfer, test_connection, update_connection, AppState,
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
            let state = AppState::new(config_dir)?;
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
            app.manage(TransferEngine::new(
                hub,
                Arc::new(DispatchRunner {
                    upload: Arc::new(UploadRunner),
                    download: Arc::new(DownloadRunner),
                }),
                Arc::new(TauriStateSink(handle)),
                progress_tx,
                EngineConfig::default(),
            ));
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
            enqueue_uploads,
            enqueue_download,
            list_transfers,
            pause_transfer,
            resume_transfer,
            cancel_transfer,
            retry_transfer,
            clear_finished_transfers
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
