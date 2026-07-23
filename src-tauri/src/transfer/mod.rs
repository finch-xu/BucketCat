//! Transfer engine (design §5): queue, chunked concurrency, progress.
//!
//! The engine depends only on [`crate::provider`]'s trait and
//! [`crate::provider::ProviderHub`] -- never on Tauri's `State` -- so it can
//! run entirely outside the IPC layer (design §3 principle 4).

pub mod model;

pub use model::{next_status, Direction, TransferCommand, TransferStatus, TransferTaskDto};
