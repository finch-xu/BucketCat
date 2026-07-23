//! Transfer engine (design §5): queue, chunked concurrency, progress.
//!
//! The engine depends only on [`crate::provider`]'s trait and
//! [`crate::provider::ProviderHub`] -- never on Tauri's `State` -- so it can
//! run entirely outside the IPC layer (design §3 principle 4).

pub mod model;
pub mod part;
pub mod retry;

pub use model::{next_status, Direction, TransferCommand, TransferStatus, TransferTaskDto};
pub use part::{plan_upload, PartSpec, UploadPlan};
pub use retry::{backoff_delay, is_retryable, MAX_RETRIES};
