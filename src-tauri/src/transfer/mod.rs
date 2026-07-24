//! Transfer engine (design §5): queue, chunked concurrency, progress.
//!
//! The engine depends only on [`crate::provider`]'s trait and
//! [`crate::provider::ProviderHub`] -- never on Tauri's `State` -- so it can
//! run entirely outside the IPC layer (design §3 principle 4).

pub mod engine;
pub mod model;
pub mod part;
pub mod partfile;
pub mod progress;
pub mod retry;
pub mod upload;

pub use engine::{
    DownloadState, EngineConfig, EnqueueSpec, MultipartState, ProgressHandle, ResumeState,
    RunOutcome, StopKind, TaskContext, TaskControl, TransferEngine, TransferRunner, TransferSink,
};
pub use model::{next_status, Direction, TransferCommand, TransferStatus, TransferTaskDto};
pub use part::{plan_upload, PartSpec, UploadPlan};
pub use partfile::{bcpart_path, PartFile};
pub use progress::{
    eta_secs, spawn_aggregator, ProgressMsg, ProgressPayload, ProgressSink, PROGRESS_INTERVAL,
};
pub use retry::{backoff_delay, is_retryable, MAX_RETRIES};
pub use upload::UploadRunner;
