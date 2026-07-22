pub mod commands;
pub mod error;
pub mod provider;
pub mod store;

use commands::{
    add_connection, delete_connection, list_buckets, list_connections, test_connection,
    update_connection, AppState,
};
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
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
            list_buckets
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
