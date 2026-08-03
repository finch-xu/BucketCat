//! Transfer engine (design §5): queue, chunked concurrency, progress.
//!
//! The engine depends only on [`crate::provider`]'s trait and
//! [`crate::provider::ProviderHub`] -- never on Tauri's `State` -- so it can
//! run entirely outside the IPC layer (design §3 principle 4).

pub mod checkpoint;
pub mod dispatch;
pub mod download;
pub mod engine;
pub mod limits;
pub mod model;
pub mod part;
pub mod partfile;
pub mod progress;
pub mod retry;
pub mod upload;

pub use checkpoint::{checkpoint_dir, remove, scan, write, Checkpoint};
pub use dispatch::DispatchRunner;
pub use download::DownloadRunner;
pub use engine::{
    plan_restore, restore_all, DownloadState, EnqueueSpec, MultipartState, ProgressHandle,
    RestoreAction, ResumeState, RunOutcome, StopKind, TaskContext, TaskControl, TransferEngine,
    TransferRunner, TransferSink,
};
pub use limits::SharedLimits;
pub use model::{next_status, Direction, TransferCommand, TransferStatus, TransferTaskDto};
pub use part::{
    chunks_for, plan_download, plan_upload_with, DownloadPlan, PartSpec, TransferTuning, UploadPlan,
};
pub use partfile::{bcpart_path, PartFile};
pub use progress::{
    eta_secs, spawn_aggregator, ProgressMsg, ProgressPayload, ProgressSink, PROGRESS_INTERVAL,
};
pub use retry::{backoff_delay, is_retryable, MAX_RETRIES};
pub use upload::UploadRunner;
