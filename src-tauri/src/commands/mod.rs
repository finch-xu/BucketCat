//! Tauri command handlers.
//!
//! Every `#[tauri::command]` fn lives here (or in a submodule of this
//! module). Commands are thin: they parse frontend input, delegate to
//! [`crate::provider`] / [`crate::store`], and translate any failure into
//! [`crate::error::AppError`] so the wire format the frontend sees is always
//! `{ code, params }`.
