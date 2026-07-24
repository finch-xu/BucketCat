//! Tauri command handlers.
//!
//! Every `#[tauri::command]` fn lives here (or in a submodule of this
//! module). Commands are thin: they parse frontend input, delegate to
//! [`crate::provider`] / [`crate::store`], and translate any failure into
//! [`crate::error::AppError`] so the wire format the frontend sees is always
//! `{ code, params }`.

pub mod connection;
pub mod object;
pub mod settings;
pub mod transfer;

pub use connection::{
    add_connection, delete_connection, list_buckets, list_connections, test_connection,
    update_connection, AppState,
};
pub use object::{
    create_folder, delete_objects, delete_prefix, head_object, list_objects, presign_get,
    rename_object,
};
pub use settings::{get_resume_enabled, set_resume_enabled, ResumeFlag};
pub use transfer::{
    cancel_transfer, clear_finished_transfers, enqueue_download, enqueue_folder_download,
    enqueue_uploads, list_transfers, pause_transfer, resume_transfer, retry_transfer,
};
