pub mod commands;
pub mod error;
pub mod logging;
pub mod provider;
pub mod store;
pub mod transfer;

use commands::{
    add_connection, create_folder, delete_connection, delete_objects, list_buckets,
    list_connections, list_objects, rename_object, test_connection, update_connection, AppState,
};
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
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
            rename_object,
            create_folder
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
