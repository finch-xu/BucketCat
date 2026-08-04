//! Transfer scheduling: the task table, the task-level semaphore, and the
//! state machine's only writer.
//!
//! ## Who may change a task's status
//!
//! Exactly one place: the driver task spawned per transfer. Every task in a
//! non-terminal, *active* status (`Queued` or `Running`) has a live driver;
//! `Paused` and `Failed` tasks have none. That invariant is what makes
//! pause/resume race-free without any extra bookkeeping:
//!
//! - `pause` / `cancel` only *request* a stop (write the intent, fire the
//!   token). The driver observes it, unwinds, and applies the transition.
//! - `resume` / `retry` are only legal from `Paused` / `Failed` -- i.e. from
//!   states with no driver -- so they can safely install a fresh
//!   [`TaskControl`] and spawn a new driver.
//! - `cancel` on a task with no live driver applies the transition itself,
//!   since nobody else will.
//!
//! The one visible consequence: pausing a running task takes as long as the
//! runner needs to notice its token (milliseconds), instead of flipping the
//! UI instantly and hoping the runner catches up.
//!
//! `pause` and `cancel` read the status **and** fire the token under a single
//! lock. Doing it in two steps used to let a stale `Pause` intent reach a
//! driver whose task had already gone back to `Queued`, where `Pause` is not a
//! legal transition: the driver applied nothing and exited, leaving a `Queued`
//! task with no driver -- and `resume`, `retry` and `pause` are all illegal
//! from `Queued`, so the row was unrecoverable. [`EngineInner::apply_stop`]
//! now reports whether it applied anything and the driver warns when it did
//! not, because the silent no-op is what made that bug invisible.
//!
//! ## Admission order is not enqueue order
//!
//! Tasks are admitted in whatever order the scheduler happens to poll their
//! drivers, **not** in enqueue order: each [`TransferEngine::enqueue`] spawns
//! an independent driver, and those drivers race each other for the semaphore
//! permit. Enqueueing `a` then `b` with one free slot may well start `b`.
//!
//! What *is* guaranteed: tokio's semaphore is FIFO among already-*registered*
//! waiters, so once the permits are exhausted the order of everyone already
//! queued is fixed and no task can starve behind a stream of newcomers. Only
//! the initial race -- between drivers that have not yet reached the
//! semaphore -- is unordered.
//!
//! An ordered admission queue (a single scheduler loop handing out permits in
//! `seq` order) is deliberately deferred: it buys a nicety the UI does not
//! currently promise, at the cost of a second coordination point in the one
//! part of the engine whose invariants are hardest to keep.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::provider::{Provider, ProviderHub, UploadedPart};
use crate::transfer::checkpoint::{self, Checkpoint};
use crate::transfer::limits::SharedLimits;
use crate::transfer::model::{
    next_status, Direction, TaskNotice, TransferCommand, TransferStatus, TransferTaskDto,
};
use crate::transfer::part::{chunks_for, TransferTuning};
use crate::transfer::partfile::bcpart_path;
use crate::transfer::progress::ProgressMsg;

/// Why a task was asked to stop. See decision D2 in the plan: pause and
/// cancel share one [`CancellationToken`] and are told apart by this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    Pause,
    Cancel,
}

const STOP_NONE: u8 = 0;
const STOP_PAUSE: u8 = 1;
const STOP_CANCEL: u8 = 2;

/// A task's stop switch: the intent is written *before* the token fires, so
/// any reader woken by the token is guaranteed to see it.
#[derive(Debug, Clone)]
pub struct TaskControl {
    token: CancellationToken,
    stop: Arc<AtomicU8>,
}

impl TaskControl {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            stop: Arc::new(AtomicU8::new(STOP_NONE)),
        }
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub fn request(&self, kind: StopKind) {
        let value = match kind {
            StopKind::Pause => STOP_PAUSE,
            StopKind::Cancel => STOP_CANCEL,
        };
        // Ordering matters: a reader that wakes on the token must already be
        // able to see the intent. `SeqCst` on both sides makes that explicit.
        self.stop.store(value, Ordering::SeqCst);
        self.token.cancel();
    }

    pub fn requested(&self) -> Option<StopKind> {
        match self.stop.load(Ordering::SeqCst) {
            STOP_PAUSE => Some(StopKind::Pause),
            STOP_CANCEL => Some(StopKind::Cancel),
            _ => None,
        }
    }
}

/// In-flight multipart bookkeeping, kept so an in-session pause can pick up
/// where it left off. Also embedded in [`crate::transfer::checkpoint::Checkpoint`]
/// (M4c) so it must round-trip through JSON, hence `Serialize`/`Deserialize`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MultipartState {
    pub upload_id: String,
    pub completed: Vec<UploadedPart>,
    /// The source file's size, in bytes, captured when the multipart upload
    /// was created. A cross-restart resume (M4c) will compare this against
    /// the file's current size before trusting the resume.
    pub source_size: u64,
    /// The source file's mtime, unix milliseconds, captured alongside
    /// `source_size`. `0` means the fingerprint was never captured -- either
    /// an in-memory state predating this field, or a `stat` that failed.
    pub source_mtime: i64,
    /// The `part_size` the multipart plan used when this upload was created
    /// (`UploadPlan::Multipart`'s `part_size`). Chunking is user-configurable
    /// (M6+), so a checkpoint cut under one tuning cannot be trusted under
    /// another -- its `completed` part numbers would map to different byte
    /// ranges. `#[serde(default)]` makes a pre-this-field checkpoint read as
    /// `0`, which the resume path treats as an unconditional mismatch (see
    /// `upload.rs`'s resume reconcile).
    #[serde(default)]
    pub part_size: u64,
}

/// In-flight download bookkeeping for an in-session pause/resume. Also
/// embedded in the M4c checkpoint file, so it needs the same round-trip
/// derives as [`MultipartState`]; a cross-restart resume re-`head`s and
/// checks the ETag before trusting it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadState {
    pub etag: Option<String>,
    pub completed_parts: Vec<i32>,
    pub bcpart: std::path::PathBuf,
    /// The `chunk_size` the download plan used when this download was
    /// started ([`crate::transfer::part::DownloadPlan`]'s `chunk_size`). Same
    /// rationale as [`MultipartState::part_size`]: a checkpoint cut under a
    /// different tuning cannot be trusted, since its `completed_parts` chunk
    /// numbers would map to different byte ranges under the current plan.
    /// `#[serde(default)]` makes a pre-this-field checkpoint read as `0`,
    /// which the resume path treats as an unconditional mismatch (see
    /// `download.rs`'s resume reconcile).
    #[serde(default)]
    pub part_size: u64,
}

/// Per-task resume state, one variant per direction. Generalized from the
/// upload-only `MultipartState` so the cleanup path (and the runner's resume
/// slot) can serve both directions. Serde derives make it the payload M4c
/// persists inside a [`crate::transfer::checkpoint::Checkpoint`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResumeState {
    Upload(MultipartState),
    Download(DownloadState),
}

/// A runner's handle for reporting bytes.
///
/// It owns the shared counter the engine reads in [`TransferEngine::snapshot`],
/// so a task listed mid-flight shows real progress without the runner and the
/// task table having to lock each other.
#[derive(Debug, Clone)]
pub struct ProgressHandle {
    tx: mpsc::UnboundedSender<ProgressMsg>,
    task_id: String,
    total: u64,
    transferred: Arc<AtomicU64>,
}

impl ProgressHandle {
    pub fn add(&self, bytes: u64) {
        self.transferred.fetch_add(bytes, Ordering::Relaxed);
        // A closed channel means the aggregator is gone (shutdown); dropping
        // progress then is correct, not an error worth propagating.
        let _ = self.tx.send(ProgressMsg::Delta {
            task_id: self.task_id.clone(),
            bytes,
            total: self.total,
        });
    }

    /// Undoes bytes an earlier `add` reported for a download attempt that then
    /// failed mid-stream (Task 4): a retryable chunk read must not leave the
    /// bytes it already streamed permanently counted, or a task that retries
    /// several times over-reports and the bar never reaches 100% even once
    /// every chunk truly lands.
    ///
    /// `fetch_update` rather than a plain `fetch_sub`: the runner reports and
    /// retracts from concurrent chunk tasks, and a straight subtract could
    /// underflow the `u64` if two retractions raced past a small counter.
    /// Saturating at 0 makes that impossible -- the DTO-side `transferred`
    /// this counter backs (see `EngineInner::apply`/`TransferEngine::snapshot`)
    /// can never go negative anyway.
    ///
    /// Also sends [`ProgressMsg::Retract`] so the aggregator's own copy of
    /// `transferred` (the one batched over IPC) stays in sync with this
    /// atomic -- without it, a task that keeps running after a retract (Task
    /// 3's upload-retry case) would over-report on the wire forever, since
    /// nothing would ever bring the aggregator's figure back down.
    pub fn retract(&self, bytes: u64) {
        let _ = self
            .transferred
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_sub(bytes))
            });
        // Same "closed channel is fine" stance as `add`: a gone aggregator
        // means the engine is shutting down, and there is nobody left to
        // compensate for.
        let _ = self.tx.send(ProgressMsg::Retract {
            task_id: self.task_id.clone(),
            bytes,
        });
    }
}

/// The static half of a [`Checkpoint`] -- everything about a task that never
/// changes for the life of the transfer. Captured once when the driver spawns
/// so [`CheckpointWriter::persist`] can rebuild the full checkpoint from just
/// the live [`ResumeState`] without touching the task table again.
///
/// Holds `connection_id` (an opaque handle into the secure store), never a
/// credential -- the checkpoint file must be safe to leave on disk.
#[derive(Debug, Clone)]
struct CheckpointStatics {
    direction: Direction,
    connection_id: String,
    bucket: String,
    key: String,
    local_path: String,
    file_name: String,
    total: u64,
}

/// A per-task handle that mirrors the in-memory [`ResumeState`] to a checkpoint
/// file (M4c, decision D-T5). Best-effort and coalesced:
///
/// - **Gated**: a write is a no-op while `enabled` (the runtime resume flag) is
///   `false`, so turning resume off stops persisting without touching any call
///   site.
/// - **Coalesced**: a non-`force` write within ~2s of the last one is skipped,
///   so a burst of per-part resume-sets costs a bounded number of files rather
///   than one per part. `force` writes (the first set, and the pause point)
///   always land so a cancel-before-any-part or a pause is never lost.
/// - **Never fatal**: a write failure is logged and swallowed -- a checkpoint
///   is an optimisation, and failing the transfer over one would be a
///   regression the user cannot act on.
///
/// Cheap to clone; the throttle timestamp and the flag are shared through
/// `Arc`, so every clone made for a runner's job coalesces against the same
/// clock.
#[derive(Clone)]
pub struct CheckpointWriter {
    dir: PathBuf,
    task_id: String,
    statics: CheckpointStatics,
    enabled: Arc<AtomicBool>,
    /// Last successful (or skipped-because-recent) write, on the tokio clock so
    /// the throttle follows a test's virtual time. Behind a `std` mutex: the
    /// critical section never awaits, so this must not be a tokio mutex.
    last: Arc<std::sync::Mutex<Option<tokio::time::Instant>>>,
}

impl CheckpointWriter {
    /// Coalesced, gated, best-effort write of the task's checkpoint. See the
    /// type docs; `force` bypasses only the throttle, never the gate.
    pub fn persist(&self, resume: &ResumeState, force: bool) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        if !force {
            let mut last = self.last.lock().unwrap();
            let now = tokio::time::Instant::now();
            if let Some(prev) = *last {
                if now.duration_since(prev) < Duration::from_secs(2) {
                    return;
                }
            }
            *last = Some(now);
        }
        let cp = Checkpoint {
            direction: self.statics.direction,
            connection_id: self.statics.connection_id.clone(),
            bucket: self.statics.bucket.clone(),
            key: self.statics.key.clone(),
            local_path: self.statics.local_path.clone(),
            file_name: self.statics.file_name.clone(),
            total: self.statics.total,
            resume: resume.clone(),
        };
        if let Err(err) = checkpoint::write(&self.dir, &self.task_id, &cp) {
            // Best-effort: a checkpoint is a resume optimisation, so a write
            // failure is logged and swallowed -- never propagated into the
            // transfer's own result.
            tracing::warn!(task = %self.task_id, "checkpoint write failed: {err}");
        }
    }
}

/// Everything a runner needs. Deliberately concrete: the runner is chosen at
/// engine construction, so there is no reason to abstract this further.
pub struct TaskContext {
    pub task: TransferTaskDto,
    pub hub: Arc<ProviderHub>,
    pub control: TaskControl,
    pub part_limit: usize,
    /// The tuning snapshot ([`SharedLimits::tuning`]) read at the moment this
    /// task was admitted. Captured once here, not read again for the life of
    /// the task, so a settings change mid-transfer never reshapes a plan a
    /// runner has already committed to (design §4.4).
    pub tuning: TransferTuning,
    pub progress: ProgressHandle,
    pub resume: Arc<Mutex<Option<ResumeState>>>,
    /// The checkpoint mirror, or `None` when checkpointing is disabled (no
    /// checkpoint dir configured). Cloned into the runner's job so the resume
    /// call sites can persist without reaching back into the engine.
    pub checkpoint: Option<CheckpointWriter>,
    /// Reports a transient in-flight retry notice (Task 7, design §7.4).
    /// `Some(notice)` on a retryable failure the runner is about to retry,
    /// `None` to clear a previously-emitted one once the step succeeds.
    /// Wired to [`EngineInner::set_notice`]; the closure spawns its own task
    /// (see [`EngineInner::drive`]) so a runner never awaits the task-table
    /// lock just to surface a notice.
    pub notice: Arc<dyn Fn(Option<TaskNotice>) + Send + Sync>,
}

impl TaskContext {
    /// Mirrors `resume` to the checkpoint file if checkpointing is enabled for
    /// this task. A no-op when there is no writer. See [`CheckpointWriter`].
    pub fn persist_checkpoint(&self, resume: &ResumeState, force: bool) {
        if let Some(writer) = &self.checkpoint {
            writer.persist(resume, force);
        }
    }
}

/// What a runner actually did. The engine cannot infer this from a stop
/// request alone: a stop can be requested while the transfer is already
/// committing, and recording "canceled" for an object that exists on the
/// server would be a lie the user cannot act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The transfer finished; the bytes are on the server.
    Completed,
    /// The runner observed a stop request and unwound early.
    Stopped,
}

/// Performs one transfer.
///
/// The runner -- not the engine -- decides whether the transfer happened: it
/// is the only party that knows whether the bytes landed. Return
/// [`RunOutcome::Completed`] once the transfer is committed and
/// [`RunOutcome::Stopped`] only when a stop request from
/// [`TaskControl::requested`] was observed *and* honoured by unwinding
/// without finishing.
///
/// A `Completed` outcome wins even over a stop that was requested while the
/// runner was committing; the engine will record `Completed` and the pending
/// pause/cancel is dropped. Reporting `Stopped` with no stop actually
/// requested is a runner bug, and the engine treats it as one (the task
/// fails with an `internal` code rather than being stranded in `Running`).
#[async_trait]
pub trait TransferRunner: Send + Sync + 'static {
    async fn run(&self, ctx: TaskContext) -> AppResult<RunOutcome>;
}

/// Where status changes go. Unlike progress, these are never throttled or
/// coalesced -- dropping one would leave the panel stuck (plan decision D3).
pub trait TransferSink: Send + Sync + 'static {
    fn state_changed(&self, task: &TransferTaskDto);
}

/// What to enqueue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnqueueSpec {
    pub direction: Direction,
    pub connection_id: String,
    pub bucket: String,
    pub key: String,
    pub local_path: String,
    pub file_name: String,
    pub total: u64,
}

impl EnqueueSpec {
    /// Builds an upload spec by stat-ing the local file, so size and display
    /// name always come from the filesystem rather than from the frontend.
    pub fn for_upload(
        connection_id: String,
        bucket: String,
        key: String,
        local_path: PathBuf,
    ) -> AppResult<Self> {
        let meta = std::fs::metadata(&local_path).map_err(|err| AppError::FileIo {
            path: local_path.display().to_string(),
            message: err.to_string(),
        })?;
        if !meta.is_file() {
            return Err(AppError::FileIo {
                path: local_path.display().to_string(),
                // Directory uploads need a recursive walk and one task per
                // file; that is M4c's scope, so reject rather than silently
                // uploading nothing.
                message: "not a regular file".to_string(),
            });
        }
        let file_name = local_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| key.clone());

        Ok(Self {
            direction: Direction::Upload,
            connection_id,
            bucket,
            key,
            local_path: local_path.to_string_lossy().to_string(),
            file_name,
            total: meta.len(),
        })
    }

    /// Builds a download spec. Unlike `for_upload`, the local file does not
    /// exist yet, so `total` comes from a prior `head_object` (done in the
    /// command) rather than from the filesystem.
    pub fn for_download(
        connection_id: String,
        bucket: String,
        key: String,
        local_path: PathBuf,
        total: u64,
        file_name: String,
    ) -> Self {
        Self {
            direction: Direction::Download,
            connection_id,
            bucket,
            key,
            local_path: local_path.to_string_lossy().to_string(),
            file_name,
            total,
        }
    }
}

struct TaskRecord {
    dto: TransferTaskDto,
    control: TaskControl,
    resume: Arc<Mutex<Option<ResumeState>>>,
    transferred: Arc<AtomicU64>,
}

/// Bytes a checkpoint's completed parts already account for -- the preset for a
/// restored task's `transferred` (see [`TransferEngine::restore_paused`]) so a
/// freshly relaunched panel shows real progress rather than `0/total`.
///
/// - **Upload**: the sum of the recorded multipart parts' sizes, straight from
///   the [`UploadedPart`]s the previous run committed.
/// - **Download**: the sum of the plan chunks whose 1-based `number` the
///   checkpoint marks done. The chunk table is rebuilt from the recorded
///   `part_size` ([`DownloadState::part_size`]) via [`chunks_for`], exactly
///   reproducing the plan the previous run cut its checkpoint under -- so no
///   `.bcpart` or network read is needed to size them, and no assumption is
///   made about the *current* tuning. `part_size == 0` (a pre-M6 checkpoint,
///   or one the resume path has already deemed unusable) restores `0`: the
///   resume path discards such a checkpoint wholesale, so the displayed
///   progress must agree that nothing survived.
fn checkpoint_completed_bytes(cp: &Checkpoint) -> u64 {
    match &cp.resume {
        ResumeState::Upload(ms) => ms.completed.iter().map(|p| p.size).sum(),
        ResumeState::Download(ds) => {
            if ds.part_size == 0 {
                return 0;
            }
            let done: std::collections::HashSet<i32> = ds.completed_parts.iter().copied().collect();
            chunks_for(cp.total, ds.part_size)
                .iter()
                .filter(|c| done.contains(&c.number))
                .map(|c| c.length)
                .sum()
        }
    }
}

struct EngineInner {
    hub: Arc<ProviderHub>,
    runner: Arc<dyn TransferRunner>,
    sink: Arc<dyn TransferSink>,
    progress: mpsc::UnboundedSender<ProgressMsg>,
    tasks: Mutex<HashMap<String, TaskRecord>>,
    /// The task-admission semaphore, the per-task part limit, and the tuning
    /// snapshot every newly-admitted task reads -- all hot-adjustable, see
    /// [`SharedLimits`].
    limits: Arc<SharedLimits>,
    seq: AtomicU64,
    /// Where per-task checkpoints live, or `None` to disable checkpointing
    /// entirely (the engine's own unit tests pass `None` to keep behaviour
    /// unchanged).
    checkpoint_dir: Option<PathBuf>,
    /// The runtime resume flag, shared into every [`CheckpointWriter`]. Toggling
    /// it (M4c Task 6/9) turns checkpoint writing on or off without rebuilding
    /// the engine.
    resume_enabled: Arc<AtomicBool>,
    /// Process-lifetime count of tasks that have reached [`TransferStatus::Completed`].
    /// Monotonic on purpose: `clear_finished` reaps terminal rows (and the
    /// `Completed`-transition bookkeeping in [`EngineInner::drive`] can run
    /// before a row is ever swept), so a table scan for `Completed` rows would
    /// undercount or miss tasks the tray still needs to know finished. See
    /// [`EngineSummary::completed_count`].
    completed_tasks: AtomicU64,
}

impl EngineInner {
    /// Applies a state-machine command and, if it was legal, emits the new
    /// state. The emit happens **after** the lock is released so a slow sink
    /// can never stall the task table.
    ///
    /// `error` is `Some((code, params))` only for a `Fail` transition; any
    /// other transition (including a second `Fail` call that turns out
    /// illegal) leaves both `error_code` and `error_params` cleared. Every
    /// legal transition also clears `notice` -- a retry notice is only
    /// meaningful while the task is actively `Running`, and once the driver
    /// moves it anywhere else (`Paused`, `Failed`, `Completed`, `Canceled`,
    /// even back to `Running` via `Start`) a stale one would be misleading.
    async fn apply(
        self: &Arc<Self>,
        task_id: &str,
        cmd: TransferCommand,
        error: Option<(String, HashMap<String, String>)>,
    ) -> bool {
        let updated = {
            let mut tasks = self.tasks.lock().await;
            let Some(record) = tasks.get_mut(task_id) else {
                return false;
            };
            let Some(next) = next_status(record.dto.status, cmd) else {
                return false;
            };
            record.dto.status = next;
            match (next, error) {
                (TransferStatus::Failed, Some((code, params))) => {
                    record.dto.error_code = Some(code);
                    record.dto.error_params = Some(params);
                }
                _ => {
                    record.dto.error_code = None;
                    record.dto.error_params = None;
                }
            }
            record.dto.notice = None;
            record.dto.transferred = record.transferred.load(Ordering::Relaxed);
            record.dto.clone()
        };
        tracing::debug!(task = %updated.id, status = ?updated.status, "transfer state");
        self.sink.state_changed(&updated);
        true
    }

    /// Updates a task's transient retry notice and broadcasts the new DTO.
    /// Unlike [`EngineInner::apply`] this never touches `status` -- a runner
    /// can emit and clear a notice many times while a task stays `Running`.
    ///
    /// Each call to the `notice` closure built in [`EngineInner::drive`]
    /// dispatches its own `tokio::spawn`, so calls can arrive **out of
    /// order** relative to each other and relative to the driver's own
    /// `apply()` calls -- there is no queue, no sequence number, nothing
    /// serializing them beyond whichever gets scheduled first. Two distinct
    /// late-arrival races follow from that, both benign no-ops here:
    ///
    /// - **The task row is gone.** It finished (`Completed`/`Canceled`) and
    ///   was later reaped by [`TransferEngine::clear_finished`] before this
    ///   call ran. `tasks.get_mut` returns `None` and this returns early.
    /// - **The task row is still there, but no longer `Running`.** A
    ///   `Some(TaskNotice)` emitted right before a part/chunk permanently
    ///   fails is never followed by a compensating `None` on that path (see
    ///   `upload.rs`/`download.rs`'s retry loops -- the failure just returns
    ///   `Err`), so if that emit's own `set_notice` call is still queued
    ///   behind the driver's `apply(Fail)`, it would otherwise land *after*
    ///   `apply` already cleared `notice` to `None` and permanently resurrect
    ///   a stale notice on a terminal (or `Paused`) DTO -- a row with no live
    ///   driver left to ever clear it again. Guarding on `status ==
    ///   Running` here is what closes that window: a notice is only ever
    ///   meaningful while the task is actively running, so a write that
    ///   arrives once it no longer is gets dropped instead of applied.
    async fn set_notice(self: &Arc<Self>, task_id: &str, notice: Option<TaskNotice>) {
        let updated = {
            let mut tasks = self.tasks.lock().await;
            let Some(record) = tasks.get_mut(task_id) else {
                return;
            };
            if record.dto.status != TransferStatus::Running {
                return;
            }
            record.dto.notice = notice;
            record.dto.transferred = record.transferred.load(Ordering::Relaxed);
            record.dto.clone()
        };
        self.sink.state_changed(&updated);
    }

    fn spawn_driver(self: &Arc<Self>, task_id: String) {
        let inner = Arc::clone(self);
        tokio::spawn(async move { inner.drive(task_id).await });
    }

    async fn drive(self: Arc<Self>, task_id: String) {
        let Some((dto, control, resume, transferred)) = ({
            let tasks = self.tasks.lock().await;
            tasks.get(&task_id).map(|r| {
                (
                    r.dto.clone(),
                    r.control.clone(),
                    Arc::clone(&r.resume),
                    Arc::clone(&r.transferred),
                )
            })
        }) else {
            return;
        };
        let token = control.token();

        // Wait for a global slot -- but stay cancellable while queued, so a
        // user can cancel a task that never got to run.
        let permit = tokio::select! {
            // `biased;` is load-bearing, not a micro-optimisation. When a task
            // is cancelled before it is ever polled, both branches are ready at
            // the first poll: the token is already cancelled *and* a permit is
            // free. Unbiased `select!` picks a branch at random, so half the
            // time a task the user already cancelled would still be started.
            // Biased polling makes the cancellation win every time.
            biased;
            _ = token.cancelled() => {
                if !self.apply_stop(&task_id, &control).await {
                    Self::warn_stranded(&task_id, &control);
                }
                return;
            }
            permit = self.limits.acquire() => match permit {
                Ok(permit) => permit,
                // The semaphore is only ever closed on shutdown.
                Err(_) => return,
            },
        };

        if !self.apply(&task_id, TransferCommand::Start, None).await {
            drop(permit);
            if control.requested().is_some() && !self.apply_stop(&task_id, &control).await {
                Self::warn_stranded(&task_id, &control);
            }
            return;
        }

        // `dto` is moved into the context below, so read what the progress
        // handle and the checkpoint writer need out of it first.
        let total = dto.total;
        // Build the checkpoint writer from the task's static fields (decision
        // D-T5). Only when a checkpoint dir is configured; otherwise the runner
        // sees `None` and every `persist_checkpoint` is a no-op.
        let checkpoint = self.checkpoint_dir.as_ref().map(|dir| CheckpointWriter {
            dir: dir.clone(),
            task_id: task_id.clone(),
            statics: CheckpointStatics {
                direction: dto.direction,
                connection_id: dto.connection_id.clone(),
                bucket: dto.bucket.clone(),
                key: dto.key.clone(),
                local_path: dto.local_path.clone(),
                file_name: dto.file_name.clone(),
                total: dto.total,
            },
            enabled: Arc::clone(&self.resume_enabled),
            last: Arc::new(std::sync::Mutex::new(None)),
        });
        // The notice closure cannot call `set_notice` directly: `set_notice`
        // is async (it takes the tasks `Mutex`), and a runner must never be
        // made to await the task table just to surface a retry notice. So it
        // clones the pieces it needs and spawns its own short-lived task --
        // exactly the "fire and forget" shape `ProgressHandle::add` uses for
        // progress, just via `tokio::spawn` instead of an mpsc send since
        // `set_notice` mutates the same table `apply` does.
        let notice_inner = Arc::clone(&self);
        let notice_task_id = task_id.clone();
        let notice: Arc<dyn Fn(Option<TaskNotice>) + Send + Sync> = Arc::new(move |n| {
            let inner = Arc::clone(&notice_inner);
            let task_id = notice_task_id.clone();
            tokio::spawn(async move { inner.set_notice(&task_id, n).await });
        });

        // Read fresh, right before this task starts: a settings change
        // (Task 8's command layer, via `SharedLimits::set_max_parts`/
        // `set_tuning`) must apply to the *next* task admitted, without
        // touching any task already running (design §4.4).
        let ctx = TaskContext {
            task: dto,
            hub: Arc::clone(&self.hub),
            control: control.clone(),
            part_limit: self.limits.part_limit(),
            tuning: self.limits.tuning(),
            progress: ProgressHandle {
                tx: self.progress.clone(),
                task_id: task_id.clone(),
                total,
                transferred,
            },
            resume,
            checkpoint,
            notice,
        };

        // The runner gets its own task so that a panic inside it cannot take
        // the driver down with it. A dead driver leaves the task in `Running`
        // forever: `cancel` only *requests* a stop that nobody is left to
        // observe, and `clear_finished` skips non-terminal rows, so the row
        // would be immortal. (Throughput is unaffected either way -- unwinding
        // releases the permit -- so this is purely about the stuck row.)
        //
        // `AssertUnwindSafe(fut).catch_unwind()` would be the tidier spelling,
        // but `FutureExt` lives in `futures-util`, which is only a transitive
        // dependency here. `JoinHandle` gives exactly the same guarantee using
        // nothing but tokio, which this crate already depends on directly.
        let runner = Arc::clone(&self.runner);
        let joined = tokio::spawn(async move { runner.run(ctx).await }).await;
        // Release the slot before any state bookkeeping so the next queued
        // task starts as early as possible.
        drop(permit);

        let result = match joined {
            Ok(result) => result,
            Err(join_err) => {
                let err = AppError::Internal {
                    message: format!("transfer runner panicked for task {task_id}"),
                };
                tracing::error!(task = %task_id, "{err} ({join_err})");
                self.forget_progress(&task_id);
                self.apply(
                    &task_id,
                    TransferCommand::Fail,
                    Some((err.code().to_string(), err.params())),
                )
                .await;
                return;
            }
        };

        match result {
            // A genuine completion wins over a stop requested while the runner
            // was committing: the bytes are on the server, and recording
            // "canceled" next to an object that exists would be a lie the user
            // cannot act on.
            Ok(RunOutcome::Completed) => {
                if let Some(record) = self.tasks.lock().await.get(&task_id) {
                    record
                        .transferred
                        .store(record.dto.total, Ordering::Relaxed);
                }
                self.forget_progress(&task_id);
                // `apply` only mutates -- and returns `true` -- when
                // `next_status` actually allows `Running -> Completed` (the
                // row exists and isn't already terminal from a race with
                // `cancel`/a second driver). Gating the counter on that
                // return value, rather than incrementing unconditionally, is
                // what keeps `completed_tasks` counting real transitions
                // one-for-one instead of every call to this arm.
                if self.apply(&task_id, TransferCommand::Complete, None).await {
                    self.completed_tasks.fetch_add(1, Ordering::Relaxed);
                }
                // Terminal: the object is on the server (or on disk), so the
                // checkpoint has nothing left to resume. Paused/Failed keep
                // theirs; only Completed and Canceled reap it.
                self.remove_checkpoint(&task_id);
            }
            Ok(RunOutcome::Stopped) => {
                if !self.apply_stop(&task_id, &control).await {
                    // Either nobody asked for a stop (a runner bug) or the
                    // transition was illegal. Failing the task is wrong-ish but
                    // recoverable; leaving it in `Running` is not recoverable
                    // at all.
                    let err = AppError::Internal {
                        message: format!(
                            "runner reported Stopped for task {task_id}, but no stop was \
                             requested"
                        ),
                    };
                    tracing::warn!(task = %task_id, stop = ?control.requested(), "{err}");
                    self.forget_progress(&task_id);
                    self.apply(
                        &task_id,
                        TransferCommand::Fail,
                        Some((err.code().to_string(), err.params())),
                    )
                    .await;
                }
            }
            Err(err) => {
                tracing::warn!(
                    task = %task_id,
                    code = err.code(),
                    "transfer failed: {err}"
                );
                self.forget_progress(&task_id);
                self.apply(
                    &task_id,
                    TransferCommand::Fail,
                    Some((err.code().to_string(), err.params())),
                )
                .await;
            }
        }
    }

    /// Tells the aggregator to drop this task's byte accounting. Deliberately
    /// **not** sent when pausing: the aggregator keeps the task's last known
    /// figure, so the panel goes on showing the progress the pause froze
    /// instead of blanking it.
    fn forget_progress(&self, task_id: &str) {
        let _ = self.progress.send(ProgressMsg::Forget {
            task_id: task_id.to_string(),
        });
    }

    /// Applies the requested stop, returning whether anything was actually
    /// written. `false` means either that no stop was requested or that the
    /// requested one was not a legal transition from the current status.
    async fn apply_stop(self: &Arc<Self>, task_id: &str, control: &TaskControl) -> bool {
        match control.requested() {
            Some(StopKind::Pause) => self.apply(task_id, TransferCommand::Pause, None).await,
            Some(StopKind::Cancel) => {
                let applied = self.apply(task_id, TransferCommand::Cancel, None).await;
                if applied {
                    self.forget_progress(task_id);
                    self.cleanup_orphaned_transfer(task_id).await;
                }
                applied
            }
            None => false,
        }
    }

    /// Removes a task's checkpoint file if checkpointing is configured. Called
    /// only from terminal transitions (Completed, Canceled); a no-op when no
    /// checkpoint dir was set, and best-effort inside [`checkpoint::remove`].
    fn remove_checkpoint(&self, task_id: &str) {
        if let Some(dir) = &self.checkpoint_dir {
            checkpoint::remove(dir, task_id);
        }
    }

    /// Best-effort cleanup of a transfer cancelled with no live runner to do
    /// it. The runner clears `resume` after *it* cleans up, so a still-`Some`
    /// resume here means nobody did -- an upload's server-side multipart is
    /// aborted, a download's local `.bcpart` is deleted. Failure is logged,
    /// never propagated; `take()` makes it safe against a racing runner.
    async fn cleanup_orphaned_transfer(self: &Arc<Self>, task_id: &str) {
        // Cancel is terminal, so reap the checkpoint first -- unconditionally,
        // before the resume-slot early return below, so a cancel with nothing
        // to abort still drops any checkpoint the task had written.
        self.remove_checkpoint(task_id);
        // Clone the coordinates + the resume handle out under the task lock,
        // then release it: the network abort must not run with any lock held
        // (the crate-wide rule -- no lock across a network `.await`).
        let (resume, connection_id, bucket, key) = {
            let tasks = self.tasks.lock().await;
            let Some(record) = tasks.get(task_id) else {
                return;
            };
            (
                Arc::clone(&record.resume),
                record.dto.connection_id.clone(),
                record.dto.bucket.clone(),
                record.dto.key.clone(),
            )
        };
        let Some(state) = resume.lock().await.take() else {
            return;
        };
        match state {
            ResumeState::Upload(ms) => {
                if ms.upload_id.is_empty() {
                    return;
                }
                let provider = match self.hub.provider(&connection_id).await {
                    Ok(p) => p,
                    Err(err) => {
                        tracing::warn!(task = %task_id, "cannot build provider to abort orphaned multipart: {err}");
                        return;
                    }
                };
                match provider.multipart_abort(&bucket, &key, &ms.upload_id).await {
                    Ok(()) => {
                        tracing::info!(task = %task_id, "aborted orphaned multipart upload on cancel")
                    }
                    Err(err) => {
                        tracing::warn!(task = %task_id, upload_id = %ms.upload_id, "abort of orphaned multipart failed: {err}")
                    }
                }
            }
            ResumeState::Download(ds) => {
                // No server-side state to reap -- just the local staging file.
                match std::fs::remove_file(&ds.bcpart) {
                    Ok(()) => tracing::info!(task = %task_id, "removed orphaned .bcpart on cancel"),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        tracing::warn!(task = %task_id, path = %ds.bcpart.display(), "removing orphaned .bcpart failed: {err}")
                    }
                }
            }
        }
    }

    /// A stop that applied nothing has left an active task with no live driver,
    /// which is unrecoverable. The single-lock `pause`/`cancel` should make
    /// this impossible; say so loudly if it ever happens anyway.
    fn warn_stranded(task_id: &str, control: &TaskControl) {
        tracing::warn!(
            task = %task_id,
            stop = ?control.requested(),
            "stop requested but no transition was legal; task may have no live driver"
        );
    }
}

/// A single-pass rollup of the task table -- the tray status line's data
/// source. The tray polls at 1Hz and only ever needs a handful of aggregate
/// numbers, so [`TransferEngine::summary`] walks `tasks` once under one lock
/// acquisition rather than cloning every [`TransferTaskDto`] the way
/// [`TransferEngine::snapshot`] does; that keeps a fast poll cheap and stops
/// it from holding the lock long enough to contend with the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineSummary {
    /// Tasks whose status is [`TransferStatus::is_active`] -- `Queued` or
    /// `Running`, i.e. the ones a live driver still owns.
    pub active_count: usize,
    /// Sum of `transferred` across the active tasks above, read straight off
    /// each task's atomic counter (not the possibly-stale `dto` field), so
    /// the tray sees live progress even between `state_changed` broadcasts.
    pub active_transferred: u64,
    /// Sum of `total` across the active tasks above.
    pub active_total: u64,
    /// Sum of `transferred` across *every* task, active or not. Kept apart
    /// from `active_transferred` so a caller can diff two successive
    /// summaries into a speed without the number jumping backwards the
    /// instant a task leaves the active set (e.g. completes) mid-interval.
    pub all_transferred: u64,
    /// Process-lifetime count of tasks that reached [`TransferStatus::Completed`],
    /// read straight off [`EngineInner::completed_tasks`]. This is a
    /// monotonic counter rather than a table scan for `Completed` rows
    /// because completed rows do not stay in the table: `clear_finished`
    /// reaps every terminal row, so by the time the tray polls, the task
    /// that just finished may already be gone. The counter is the only
    /// record left that it ever happened.
    pub completed_count: u64,
    /// Tasks whose status is [`TransferStatus::Paused`], counted in the same
    /// pass as `active_count` above.
    pub paused_count: usize,
    /// Sum of `transferred` across the paused tasks above, read off each
    /// task's atomic counter -- the same live-not-`dto` source
    /// `active_transferred` uses, so a paused task's frozen progress is
    /// exact even between `state_changed` broadcasts.
    pub paused_transferred: u64,
    /// Sum of `total` across the paused tasks above.
    pub paused_total: u64,
}

/// The transfer engine. Cheap to clone conceptually (everything is behind one
/// `Arc`), and reachable without Tauri's `State` so it can be handed to
/// non-IPC callers.
pub struct TransferEngine {
    inner: Arc<EngineInner>,
}

impl TransferEngine {
    pub fn new(
        hub: Arc<ProviderHub>,
        runner: Arc<dyn TransferRunner>,
        sink: Arc<dyn TransferSink>,
        progress: mpsc::UnboundedSender<ProgressMsg>,
        limits: Arc<SharedLimits>,
        checkpoint_dir: Option<PathBuf>,
        resume_enabled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            inner: Arc::new(EngineInner {
                hub,
                runner,
                sink,
                progress,
                tasks: Mutex::new(HashMap::new()),
                limits,
                seq: AtomicU64::new(0),
                checkpoint_dir,
                resume_enabled,
                completed_tasks: AtomicU64::new(0),
            }),
        }
    }

    /// The live concurrency + tuning state, shared with every task this
    /// engine spawns. The command layer (Task 8) adjusts it directly --
    /// `set_max_tasks`/`set_max_parts`/`set_tuning` apply to the next task
    /// admitted, with no engine rebuild and no restart.
    pub fn limits(&self) -> Arc<SharedLimits> {
        Arc::clone(&self.inner.limits)
    }

    /// Registers a task in `Queued` and starts its driver.
    pub async fn enqueue(&self, spec: EnqueueSpec) -> AppResult<TransferTaskDto> {
        let id = Uuid::new_v4().to_string();
        let dto = TransferTaskDto {
            id: id.clone(),
            seq: self.inner.seq.fetch_add(1, Ordering::SeqCst),
            direction: spec.direction,
            connection_id: spec.connection_id,
            bucket: spec.bucket,
            key: spec.key,
            local_path: spec.local_path,
            file_name: spec.file_name,
            total: spec.total,
            transferred: 0,
            status: TransferStatus::Queued,
            error_code: None,
            error_params: None,
            notice: None,
        };

        {
            let mut tasks = self.inner.tasks.lock().await;
            tasks.insert(
                id.clone(),
                TaskRecord {
                    dto: dto.clone(),
                    control: TaskControl::new(),
                    resume: Arc::new(Mutex::new(None)),
                    transferred: Arc::new(AtomicU64::new(0)),
                },
            );
        }

        tracing::info!(task = %id, key = %dto.key, total = dto.total, "transfer queued");
        // Announce the arrival: `enqueue` also has non-IPC callers (M4c's
        // checkpoint restore), so the panel cannot rely on this function's
        // return value to learn that the task exists.
        self.inner.sink.state_changed(&dto);
        self.inner.spawn_driver(id);
        Ok(dto)
    }

    /// Rebuilds a checkpointed transfer as a `Paused` task at startup, offline.
    ///
    /// This is the cross-restart counterpart to [`TransferEngine::pause`]: a
    /// checkpoint the previous session left behind is re-registered as a
    /// `Paused` row, with the [`ResumeState`] loaded straight from the file and
    /// `transferred` preset to the bytes that session already landed, so the
    /// panel shows real progress on a fresh launch rather than `0/total`.
    ///
    /// Crucially it does **not** spawn a driver -- the task stays `Paused`
    /// until the user resumes it, at which point the normal resume path
    /// re-`head`s / re-checks and picks up from the recorded parts. Nothing
    /// here touches the network or builds a provider; the caller has already
    /// discarded any orphan whose connection no longer exists.
    pub async fn restore_paused(&self, task_id: String, cp: Checkpoint) {
        // Preset the byte counter from the checkpoint's completed parts before
        // moving `cp` apart into the dto and the resume slot below.
        let preset = checkpoint_completed_bytes(&cp);
        let dto = TransferTaskDto {
            id: task_id.clone(),
            // A fresh `seq` from the same counter `enqueue` uses, so the
            // restored rows sort among any tasks enqueued later this session.
            seq: self.inner.seq.fetch_add(1, Ordering::SeqCst),
            direction: cp.direction,
            connection_id: cp.connection_id,
            bucket: cp.bucket,
            key: cp.key,
            local_path: cp.local_path,
            file_name: cp.file_name,
            total: cp.total,
            transferred: preset,
            status: TransferStatus::Paused,
            error_code: None,
            error_params: None,
            notice: None,
        };

        {
            let mut tasks = self.inner.tasks.lock().await;
            tasks.insert(
                task_id,
                TaskRecord {
                    dto: dto.clone(),
                    control: TaskControl::new(),
                    resume: Arc::new(Mutex::new(Some(cp.resume))),
                    transferred: Arc::new(AtomicU64::new(preset)),
                },
            );
        }

        tracing::info!(task = %dto.id, key = %dto.key, transferred = preset, "transfer restored as paused");
        // The startup path has no IPC return value, so the panel can only learn
        // of the row through the sink.
        self.inner.sink.state_changed(&dto);
    }

    /// Asks a running task to stop and keep its progress. The transition to
    /// `Paused` is applied by the driver once the runner has unwound.
    ///
    /// The status check and the token fire happen under **one** lock. Reading
    /// the status, releasing the lock and re-taking it to fire lets the task
    /// go back to `Queued` in between, where `Pause` is not a legal
    /// transition -- the driver would then apply nothing and exit, stranding
    /// the task with no live driver and no legal command left.
    pub async fn pause(&self, task_id: &str) -> AppResult<()> {
        let tasks = self.inner.tasks.lock().await;
        let record = tasks.get(task_id).ok_or_else(|| AppError::TaskNotFound {
            id: task_id.to_string(),
        })?;
        if record.dto.status == TransferStatus::Running {
            record.control.request(StopKind::Pause);
        }
        Ok(())
    }

    /// Cancels a task from any non-terminal state. When no driver is live
    /// (`Paused` / `Failed`) the transition is applied here, since nobody
    /// else will.
    ///
    /// Same single-lock rule as [`TransferEngine::pause`]: the status decides
    /// who applies the transition, so it must be read at the same instant the
    /// token fires.
    pub async fn cancel(&self, task_id: &str) -> AppResult<()> {
        let was_active = {
            let tasks = self.inner.tasks.lock().await;
            let record = tasks.get(task_id).ok_or_else(|| AppError::TaskNotFound {
                id: task_id.to_string(),
            })?;
            record.control.request(StopKind::Cancel);
            record.dto.status.is_active()
        };
        if !was_active
            && self
                .inner
                .apply(task_id, TransferCommand::Cancel, None)
                .await
        {
            self.inner.forget_progress(task_id);
            self.inner.cleanup_orphaned_transfer(task_id).await;
        }
        Ok(())
    }

    /// Re-queues a paused task with a fresh [`TaskControl`] -- a cancelled
    /// token can never be un-cancelled, so resuming on the old one would make
    /// the runner return immediately.
    pub async fn resume(&self, task_id: &str) -> AppResult<()> {
        self.restart(task_id, TransferCommand::Resume).await
    }

    /// Re-queues a failed task, keeping any multipart state so the retry
    /// continues from the parts that already landed.
    pub async fn retry(&self, task_id: &str) -> AppResult<()> {
        self.restart(task_id, TransferCommand::Retry).await
    }

    async fn restart(&self, task_id: &str, cmd: TransferCommand) -> AppResult<()> {
        {
            let mut tasks = self.inner.tasks.lock().await;
            let record = tasks
                .get_mut(task_id)
                .ok_or_else(|| AppError::TaskNotFound {
                    id: task_id.to_string(),
                })?;
            if next_status(record.dto.status, cmd).is_none() {
                return Ok(());
            }
            record.control = TaskControl::new();
            // Both byte counters must restart from zero at the *same* instant.
            // A resumed runner re-reports work it already reported in its
            // previous run (it replays whatever the checkpoint did not cover),
            // so leaving either side at its old value double-counts: the panel
            // would show a retried 1024-byte task at 2048/1024 = 200%.
            record.transferred.store(0, Ordering::Relaxed);
            self.inner.forget_progress(task_id);
        }
        if self.inner.apply(task_id, cmd, None).await {
            self.inner.spawn_driver(task_id.to_string());
        }
        Ok(())
    }

    /// All tasks, newest first.
    pub async fn snapshot(&self) -> Vec<TransferTaskDto> {
        let tasks = self.inner.tasks.lock().await;
        let mut out: Vec<TransferTaskDto> = tasks
            .values()
            .map(|r| {
                let mut dto = r.dto.clone();
                dto.transferred = r.transferred.load(Ordering::Relaxed);
                dto
            })
            .collect();
        out.sort_unstable_by_key(|t| std::cmp::Reverse(t.seq));
        out
    }

    /// The tray status line's data source: one lock, one pass over the task
    /// table, filling in every [`EngineSummary`] field. See that type for
    /// what each field means and why it exists.
    pub async fn summary(&self) -> EngineSummary {
        let tasks = self.inner.tasks.lock().await;
        let mut summary = EngineSummary {
            active_count: 0,
            active_transferred: 0,
            active_total: 0,
            all_transferred: 0,
            completed_count: self.inner.completed_tasks.load(Ordering::Relaxed),
            paused_count: 0,
            paused_transferred: 0,
            paused_total: 0,
        };
        for record in tasks.values() {
            let transferred = record.transferred.load(Ordering::Relaxed);
            summary.all_transferred += transferred;
            if record.dto.status.is_active() {
                summary.active_count += 1;
                summary.active_transferred += transferred;
                summary.active_total += record.dto.total;
            } else if record.dto.status == TransferStatus::Paused {
                summary.paused_count += 1;
                summary.paused_transferred += transferred;
                summary.paused_total += record.dto.total;
            }
        }
        summary
    }

    /// Drops finished tasks. `Paused` and `Failed` stay: the user can still
    /// resume or retry them, so removing them would destroy actionable work.
    pub async fn clear_finished(&self) {
        let mut tasks = self.inner.tasks.lock().await;
        tasks.retain(|_, r| !r.dto.status.is_terminal());
    }
}

/// One decision [`plan_restore`] reached for a single scanned checkpoint.
///
/// Split out from the side effects on purpose: the *decision* (restore vs.
/// discard, or -- when the connection store is unreadable -- do nothing at all)
/// is the safety-critical part and is unit-tested against the pure
/// [`plan_restore`], while [`restore_all`] is the thin layer that maps each
/// action to its I/O.
#[derive(Debug)]
pub enum RestoreAction {
    /// The checkpoint's connection still exists: rebuild it as a `Paused` row.
    Restore(String, Checkpoint),
    /// The checkpoint's connection is gone: drop the checkpoint (and, for a
    /// download, its staging `.bcpart`) -- nobody will ever resume it.
    DiscardOrphan(String, Checkpoint),
}

/// Pure restore policy: decides what to do with each scanned checkpoint, given
/// the set of connection ids currently known to the hub.
///
/// This is the safety-critical branch of startup restore, factored out so it
/// can be tested without a live Tauri app or a real hub:
///
/// - **`known == Err`** (the connection store could not be read) yields an
///   **empty** plan -- restore *nothing*, discard *nothing*, leave every
///   checkpoint on disk. A read error must never be mistaken for "no
///   connections exist", which would discard/delete every checkpoint: the
///   exact opposite of the intended safety property.
/// - **`known == Ok(set)`**: a checkpoint whose `connection_id` is in `set` is
///   a [`RestoreAction::Restore`]; one whose connection is absent is a
///   [`RestoreAction::DiscardOrphan`].
pub fn plan_restore(
    scanned: Vec<(String, Checkpoint)>,
    known: &AppResult<HashSet<String>>,
) -> Vec<RestoreAction> {
    let Ok(known) = known else {
        // Unreadable store: leave everything untouched. Never treat this as
        // "all orphans" -- that would delete every checkpoint on a transient
        // read failure.
        return Vec::new();
    };
    scanned
        .into_iter()
        .map(|(id, cp)| {
            if known.contains(&cp.connection_id) {
                RestoreAction::Restore(id, cp)
            } else {
                RestoreAction::DiscardOrphan(id, cp)
            }
        })
        .collect()
}

/// Startup restore (M4c Task 6): rebuild each unfinished transfer as a `Paused`
/// row, offline, and discard any whose connection is gone.
///
/// The single shared entry point for both `lib.rs`'s `setup` and the e2e's
/// cross-restart tests, so the two can never diverge on the safety-critical
/// error path. The decision is delegated to the pure [`plan_restore`]; this
/// function only performs the side effects the plan calls for:
///
/// - a read failure from [`ProviderHub::connection_ids`] is logged (never a
///   credential -- only the error) and yields an empty plan, so **every**
///   checkpoint is left in place rather than mistaken for an orphan;
/// - an orphan's checkpoint (and, for a download, its `.bcpart`) is removed;
/// - a known checkpoint is rebuilt via [`TransferEngine::restore_paused`].
///
/// Best-effort throughout, and holds no lock across an `.await`.
pub async fn restore_all(engine: &TransferEngine, hub: &ProviderHub, dir: &Path) {
    let known = hub
        .connection_ids()
        .await
        .map(|ids| ids.into_iter().collect::<HashSet<String>>());
    if let Err(err) = &known {
        tracing::warn!(
            "cannot read connections for checkpoint restore; leaving checkpoints in place: {err}"
        );
    }
    for action in plan_restore(checkpoint::scan(dir), &known) {
        match action {
            RestoreAction::Restore(id, cp) => engine.restore_paused(id, cp).await,
            RestoreAction::DiscardOrphan(id, cp) => {
                if cp.direction == Direction::Download {
                    // Drop the staging file too -- nobody will ever resume it.
                    let _ = std::fs::remove_file(bcpart_path(Path::new(&cp.local_path)));
                }
                checkpoint::remove(dir, &id);
                tracing::warn!(task = %id, conn = %cp.connection_id, "orphan checkpoint discarded");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    /// How the fake runner should misbehave. The engine has to survive runners
    /// it did not write, so the pathological branches need a way in.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FakeMode {
        /// Park until cancelled or told to finish, then report honestly.
        Normal,
        /// Panic instead of returning at all (Finding I-4).
        Panic,
        /// Ignore the token entirely, so a stop request that arrives while the
        /// runner is "committing" cannot be observed (Finding I-3).
        IgnoreToken,
        /// Report `Stopped` with nothing to stop -- a runner bug the engine
        /// must not turn into a stranded `Running` row.
        BogusStop,
    }

    /// A runner that parks until cancelled (or until `finish` is set), while
    /// tracking how many runs were in flight at once.
    ///
    /// `finish` is a polled `AtomicBool` rather than a `Notify`: `started` is
    /// bumped *before* the park, so a test that waits for `started == 1` can
    /// observe it before this runner is actually parked. `Notify::notify_waiters`
    /// only wakes tasks that are *already* waiting, so such a notification
    /// would be dropped and the test would hang. A flag checked at the top of
    /// the loop cannot miss a signal.
    struct FakeRunner {
        live: AtomicUsize,
        peak: AtomicUsize,
        started: AtomicUsize,
        finish: AtomicBool,
        fail_with: StdMutex<Option<AppError>>,
        mode: StdMutex<FakeMode>,
        /// Bytes to report through [`ProgressHandle::add`] at the start of each
        /// run, imitating a runner that replays already-transferred work when
        /// it resumes. Zero means "report nothing".
        report_bytes: AtomicU64,
        /// Runs that got as far as reporting `report_bytes`.
        reported: AtomicUsize,
        /// If set, each run mirrors this resume state to a checkpoint (`force`)
        /// while it is running -- standing in for a real multipart runner that
        /// has an upload id worth persisting. The write goes through the engine's
        /// real [`CheckpointWriter`], so a test observes genuine file state.
        checkpoint_state: StdMutex<Option<ResumeState>>,
        /// `ctx.part_limit` as seen by the most recently *started* run --
        /// proof that hot-adjusting [`SharedLimits`] via
        /// [`TransferEngine::limits`] is read fresh by the *next* task
        /// spawned rather than only at engine construction.
        last_part_limit: AtomicUsize,
        /// `ctx.tuning` as seen by the most recently *started* run -- the
        /// same proof as `last_part_limit`, but for the tuning snapshot
        /// [`SharedLimits::set_tuning`] hot-adjusts.
        last_tuning: StdMutex<Option<TransferTuning>>,
        /// If set, each run calls `(ctx.notice)(Some(..))` with this notice
        /// right after starting -- standing in for a retry site (Task 7)
        /// deciding a step is worth surfacing.
        emit_notice: StdMutex<Option<TaskNotice>>,
    }

    impl FakeRunner {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                live: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                started: AtomicUsize::new(0),
                finish: AtomicBool::new(false),
                fail_with: StdMutex::new(None),
                mode: StdMutex::new(FakeMode::Normal),
                report_bytes: AtomicU64::new(0),
                reported: AtomicUsize::new(0),
                checkpoint_state: StdMutex::new(None),
                last_part_limit: AtomicUsize::new(0),
                last_tuning: StdMutex::new(None),
                emit_notice: StdMutex::new(None),
            })
        }

        fn set_mode(&self, mode: FakeMode) {
            *self.mode.lock().unwrap() = mode;
        }

        fn set_emit_notice(&self, notice: TaskNotice) {
            *self.emit_notice.lock().unwrap() = Some(notice);
        }
    }

    #[async_trait::async_trait]
    impl TransferRunner for FakeRunner {
        async fn run(&self, ctx: TaskContext) -> AppResult<RunOutcome> {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.last_part_limit.store(ctx.part_limit, Ordering::SeqCst);
            *self.last_tuning.lock().unwrap() = Some(ctx.tuning);
            // Copy the mode out before anything can panic: holding the guard
            // across the `panic!` would poison the mutex for the next run.
            let mode = *self.mode.lock().unwrap();
            if mode == FakeMode::Panic {
                panic!("fake runner exploded on purpose");
            }

            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(live, Ordering::SeqCst);

            let bytes = self.report_bytes.load(Ordering::SeqCst);
            if bytes > 0 {
                ctx.progress.add(bytes);
            }
            if let Some(notice) = self.emit_notice.lock().unwrap().clone() {
                (ctx.notice)(Some(notice));
            }
            // Mirror the checkpoint *before* bumping `reported`, so a test that
            // waits on `reported == 1` is guaranteed the file is already on disk.
            if let Some(state) = self.checkpoint_state.lock().unwrap().clone() {
                ctx.persist_checkpoint(&state, true);
            }
            if bytes > 0 {
                self.reported.fetch_add(1, Ordering::SeqCst);
            }

            // Bound outside the loop: `token()` hands back an owned clone, and
            // `cancelled()` borrows it, so calling both inline inside
            // `select!` would borrow a temporary that dies too early.
            let token = ctx.control.token();
            let stopped = loop {
                // `finish` is checked first on purpose: a runner that has
                // already committed reports `Completed` even if its token
                // fired, which is exactly the race Finding I-3 is about.
                if self.finish.load(Ordering::SeqCst) {
                    break false;
                }
                if mode == FakeMode::IgnoreToken {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    continue;
                }
                tokio::select! {
                    _ = token.cancelled() => break true,
                    _ = tokio::time::sleep(Duration::from_millis(2)) => {}
                }
            };

            self.live.fetch_sub(1, Ordering::SeqCst);
            match self.fail_with.lock().unwrap().take() {
                Some(err) => Err(err),
                None if mode == FakeMode::BogusStop => Ok(RunOutcome::Stopped),
                None if stopped => Ok(RunOutcome::Stopped),
                None => Ok(RunOutcome::Completed),
            }
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        seen: StdMutex<Vec<(String, TransferStatus, Option<TaskNotice>)>>,
    }

    impl RecordingSink {
        fn statuses_of(&self, id: &str) -> Vec<TransferStatus> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(task, _, _)| task == id)
                .map(|(_, status, _)| *status)
                .collect()
        }

        /// Every `notice` a `state_changed` broadcast carried for `id`, in
        /// arrival order -- including `None`s, so a test can assert both that
        /// a notice arrived and that a later broadcast cleared it again.
        fn notices_of(&self, id: &str) -> Vec<Option<TaskNotice>> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(task, _, _)| task == id)
                .map(|(_, _, notice)| notice.clone())
                .collect()
        }
    }

    impl TransferSink for RecordingSink {
        fn state_changed(&self, task: &TransferTaskDto) {
            self.seen
                .lock()
                .unwrap()
                .push((task.id.clone(), task.status, task.notice.clone()));
        }
    }

    struct Harness {
        engine: TransferEngine,
        runner: Arc<FakeRunner>,
        sink: Arc<RecordingSink>,
        /// The aggregator end of the progress channel. It has to be *kept*:
        /// binding it to `_progress_rx` inside `harness()` dropped it the
        /// moment the function returned, which closed the channel and made
        /// every `ProgressMsg` the engine sent fail silently -- so no test
        /// could observe `Forget` at all.
        progress_rx: StdMutex<mpsc::UnboundedReceiver<ProgressMsg>>,
        progress_seen: StdMutex<Vec<ProgressMsg>>,
        _dir: tempfile::TempDir,
    }

    impl Harness {
        /// Moves whatever the engine has sent so far out of the channel into
        /// `progress_seen`. Draining is destructive, so tests never touch the
        /// receiver directly -- they poll the accessors below instead.
        fn pump_progress(&self) {
            let mut rx = self.progress_rx.lock().unwrap();
            let mut seen = self.progress_seen.lock().unwrap();
            while let Ok(msg) = rx.try_recv() {
                seen.push(msg);
            }
        }

        fn forgets_of(&self, task_id: &str) -> usize {
            self.pump_progress();
            self.progress_seen
                .lock()
                .unwrap()
                .iter()
                .filter(|msg| matches!(msg, ProgressMsg::Forget { task_id: id } if id == task_id))
                .count()
        }

        async fn task(&self, task_id: &str) -> TransferTaskDto {
            self.engine
                .snapshot()
                .await
                .into_iter()
                .find(|t| t.id == task_id)
                .expect("task missing from snapshot")
        }
    }

    fn harness(max_tasks: usize) -> Harness {
        harness_cfg(max_tasks, None, true)
    }

    /// The full harness constructor: `checkpoint_dir` + the resume flag let the
    /// checkpoint tests below observe the writer/remove paths, while `harness`
    /// keeps its two-`None`-equivalent defaults so every existing test is
    /// unchanged.
    fn harness_cfg(
        max_tasks: usize,
        checkpoint_dir: Option<PathBuf>,
        resume_enabled: bool,
    ) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let hub = Arc::new(ProviderHub::new(crate::store::SecureStore {
            path: dir.path().join("connections.enc"),
        }));
        let runner = FakeRunner::new();
        let sink = Arc::new(RecordingSink::default());
        let (progress_tx, progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = TransferEngine::new(
            hub,
            runner.clone(),
            sink.clone(),
            progress_tx,
            SharedLimits::new(max_tasks, 4, TransferTuning::balanced()),
            checkpoint_dir,
            Arc::new(AtomicBool::new(resume_enabled)),
        );
        Harness {
            engine,
            runner,
            sink,
            progress_rx: StdMutex::new(progress_rx),
            progress_seen: StdMutex::new(Vec::new()),
            _dir: dir,
        }
    }

    fn spec(key: &str) -> EnqueueSpec {
        EnqueueSpec {
            direction: Direction::Upload,
            connection_id: "c1".to_string(),
            bucket: "b".to_string(),
            key: key.to_string(),
            local_path: format!("/tmp/{key}"),
            file_name: key.to_string(),
            total: 1024,
        }
    }

    fn download_spec(key: &str, local_path: &str) -> EnqueueSpec {
        EnqueueSpec {
            direction: Direction::Download,
            connection_id: "c1".to_string(),
            bucket: "b".to_string(),
            key: key.to_string(),
            local_path: local_path.to_string(),
            file_name: key.to_string(),
            total: 1024,
        }
    }

    /// Polls until `check` holds or the (virtual-clock-free) budget expires.
    /// The engine hands work to spawned tasks, so assertions have to wait for
    /// a scheduling round rather than assuming synchronous effects.
    ///
    /// `label` is not decoration: a timeout here is the most common way one of
    /// these tests fails, and "condition never became true" on its own says
    /// nothing about *which* of a test's five waits gave up.
    async fn eventually(mut check: impl FnMut() -> bool, label: &str) {
        for _ in 0..200 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for: {label}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn global_concurrency_is_capped_at_max_tasks() {
        let h = harness(3);
        for i in 0..8 {
            h.engine.enqueue(spec(&format!("k{i}"))).await.unwrap();
        }
        eventually(
            || h.runner.live.load(Ordering::SeqCst) == 3,
            "three runners in flight",
        )
        .await;

        // Give the extra five tasks every chance to sneak past the semaphore.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            h.runner.peak.load(Ordering::SeqCst),
            3,
            "more tasks ran concurrently than the task-level semaphore allows"
        );
        assert_eq!(h.runner.started.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn finishing_a_task_admits_another_queued_one() {
        // "another", not "the next": drivers race each other for the permit, so
        // which queued task wins is the scheduler's choice rather than enqueue
        // order. See the module header's "Admission order" section.
        let h = harness(1);
        h.engine.enqueue(spec("a")).await.unwrap();
        h.engine.enqueue(spec("b")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "one of the two tasks is admitted",
        )
        .await;

        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 2,
            "the freed permit admits the other task",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn changing_max_parts_applies_to_the_next_spawned_task_only() {
        // Task 5 (spec §4.4): `TransferEngine::limits()` is the command
        // layer's hot-adjustment handle. A change made through it must reach
        // the *next* task the engine spawns, without requiring a new engine
        // (no restart) and without touching a task that is already running.
        // Also covers `ctx.tuning` (Finding 3 of the whole-branch review):
        // a hot-changed `TransferTuning` must thread into the next spawned
        // task the same way `part_limit` does, not just at construction.
        let h = harness(1);
        let first = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "task a starts",
        )
        .await;
        assert_eq!(
            h.runner.last_part_limit.load(Ordering::SeqCst),
            4,
            "harness_cfg's engine was built with max_parts = 4"
        );
        assert_eq!(
            *h.runner.last_tuning.lock().unwrap(),
            Some(TransferTuning::balanced()),
            "harness_cfg's engine was built with TransferTuning::balanced()"
        );

        h.engine.limits().set_max_parts(1);
        h.engine.limits().set_tuning(TransferTuning::aggressive());
        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(
            || h.sink.statuses_of(&first.id).last() == Some(&TransferStatus::Completed),
            "task a completes",
        )
        .await;

        h.runner.finish.store(false, Ordering::SeqCst);
        h.engine.enqueue(spec("b")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 2,
            "task b starts",
        )
        .await;
        assert_eq!(
            h.runner.last_part_limit.load(Ordering::SeqCst),
            1,
            "the next spawned task must read the hot-adjusted part_limit, not the value the \
             engine was constructed with"
        );
        assert_eq!(
            *h.runner.last_tuning.lock().unwrap(),
            Some(TransferTuning::aggressive()),
            "the next spawned task must read the hot-adjusted tuning, not the value the engine \
             was constructed with"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_running_task_completes() {
        let h = harness(1);
        let task = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "the task starts",
        )
        .await;
        h.runner.finish.store(true, Ordering::SeqCst);

        eventually(
            || h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Completed),
            "the task reaches Completed",
        )
        .await;
        assert_eq!(
            h.sink.statuses_of(&task.id),
            vec![
                TransferStatus::Queued,
                TransferStatus::Running,
                TransferStatus::Completed
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_a_queued_task_never_runs_it() {
        let h = harness(1);
        // Take the only slot *before* the second task exists. Enqueueing both
        // up front and then waiting for `started == 1` would not do: the two
        // drivers race to the semaphore and enqueue order does not decide who
        // wins it, so which task ends up queued would be the scheduler's
        // choice. With `a` already parked on the sole permit, `b` provably
        // cannot be admitted.
        let running = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "task a takes the only permit",
        )
        .await;

        let queued = h.engine.enqueue(spec("b")).await.unwrap();
        h.engine.cancel(&queued.id).await.unwrap();
        eventually(
            || h.sink.statuses_of(&queued.id).last() == Some(&TransferStatus::Canceled),
            "the queued task reaches Canceled",
        )
        .await;

        // It must have gone straight to Canceled without ever being started --
        // the announcement of its arrival in `Queued` is the only thing
        // allowed between enqueue and cancellation.
        assert_eq!(
            h.sink.statuses_of(&queued.id),
            vec![TransferStatus::Queued, TransferStatus::Canceled]
        );
        assert_eq!(h.runner.started.load(Ordering::SeqCst), 1);
        let _ = running;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pausing_a_running_task_cancels_its_token_and_parks_it() {
        let h = harness(1);
        let task = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "the task starts",
        )
        .await;

        h.engine.pause(&task.id).await.unwrap();
        // The fake runner only returns when its token fires, so reaching
        // Paused proves cancellation actually propagated into the runner.
        eventually(
            || h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Paused),
            "the task reaches Paused",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pausing_a_queued_task_leaves_it_runnable() {
        // `Pause` is not a legal transition from `Queued`, so `pause` must not
        // fire the token of a task that is merely waiting for a permit: a stop
        // intent nothing can apply would make the driver exit without writing
        // a status, leaving a `Queued` row with no live driver and no legal
        // command left (`resume`, `retry` and `pause` are all illegal there).
        let h = harness(1);
        let running = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "task a takes the only permit",
        )
        .await;

        let queued = h.engine.enqueue(spec("b")).await.unwrap();
        h.engine.pause(&queued.id).await.unwrap();
        assert_eq!(
            h.sink.statuses_of(&queued.id),
            vec![TransferStatus::Queued],
            "pausing a queued task must be a no-op"
        );

        // Free the permit: the queued task must still be admitted.
        h.engine.pause(&running.id).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 2,
            "the task that was pause-poked still gets admitted",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resuming_a_paused_task_runs_it_again_with_a_fresh_token() {
        let h = harness(1);
        let task = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "the task starts",
        )
        .await;
        h.engine.pause(&task.id).await.unwrap();
        eventually(
            || h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Paused),
            "the task reaches Paused",
        )
        .await;

        h.engine.resume(&task.id).await.unwrap();
        // A stale (already-cancelled) token would make the runner return
        // instantly; reaching a second start proves the token was replaced.
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 2,
            "the resumed task starts a second run",
        )
        .await;
        eventually(
            || h.runner.live.load(Ordering::SeqCst) == 1,
            "the resumed run stays in flight instead of returning instantly",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_task_records_its_error_code_and_can_be_retried() {
        let h = harness(1);
        *h.runner.fail_with.lock().unwrap() = Some(AppError::AccessDenied);
        let task = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "the task starts",
        )
        .await;
        h.runner.finish.store(true, Ordering::SeqCst);

        eventually(
            || h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Failed),
            "the task reaches Failed",
        )
        .await;
        assert_eq!(
            h.task(&task.id).await.error_code.as_deref(),
            Some("auth/access-denied")
        );

        h.engine.retry(&task.id).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 2,
            "the retried task starts a second run",
        )
        .await;
        assert_eq!(
            h.task(&task.id).await.error_code,
            None,
            "a retried task must not keep showing the previous failure"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_task_carries_error_params() {
        // Task 7 (spec §7.4): `error_params` mirrors `AppError::params()`
        // alongside `error_code`, so the frontend's i18n dictionary can
        // interpolate `{bucket}` and friends without a second round trip.
        let h = harness(1);
        *h.runner.fail_with.lock().unwrap() = Some(AppError::BucketNotFound {
            bucket: "b1".to_string(),
        });
        let task = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "the task starts",
        )
        .await;
        h.runner.finish.store(true, Ordering::SeqCst);

        eventually(
            || h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Failed),
            "the task reaches Failed",
        )
        .await;

        let dto = h.task(&task.id).await;
        assert_eq!(dto.error_code.as_deref(), Some("storage/bucket-not-found"));
        assert_eq!(
            dto.error_params
                .as_ref()
                .and_then(|p| p.get("bucket"))
                .map(String::as_str),
            Some("b1")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn notice_reaches_the_dto_and_clears_on_completion() {
        // Task 7 (spec §7.4): a runner surfaces an in-flight retry through
        // `ctx.notice`, which must reach a `state_changed` broadcast without
        // the runner blocking on the task table, and must not survive the
        // task reaching a terminal state.
        let h = harness(1);
        let notice = TaskNotice {
            code: "network/throttled".to_string(),
            attempt: 1,
            max: 3,
        };
        h.runner.set_emit_notice(notice.clone());
        let task = h.engine.enqueue(spec("a")).await.unwrap();

        eventually(
            || h.sink.notices_of(&task.id).contains(&Some(notice.clone())),
            "the notice reaches a state_changed broadcast",
        )
        .await;

        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(
            || h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Completed),
            "the task completes",
        )
        .await;

        assert_eq!(
            h.task(&task.id).await.notice,
            None,
            "a finished task must not keep showing a stale retry notice"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_late_notice_arriving_after_the_task_left_running_is_dropped() {
        // Review fix: the `notice` closure built in `EngineInner::drive`
        // dispatches each call via its own `tokio::spawn`, with no ordering
        // guarantee against the driver's own `apply()` calls. A `Some(..)`
        // emitted right before a permanent failure has no compensating
        // `None` on that path (the retry loops just return `Err`), so if
        // its `set_notice` call is still queued when `apply(Fail)` runs and
        // clears `notice`, the late arrival must not resurrect it on a row
        // that has already left `Running` -- there is no driver left to
        // ever clear it again.
        //
        // Called directly against `EngineInner` (not through a real retry)
        // so the race is deterministic instead of depending on the ~1s+
        // backoff window actually racing a spawn.
        let h = harness(1);
        let task = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "the task starts",
        )
        .await;
        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(
            || h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Completed),
            "the task completes",
        )
        .await;

        // Simulate the late arrival: a `set_notice` call dispatched before
        // the task finished, but only actually scheduled afterwards.
        h.engine
            .inner
            .set_notice(
                &task.id,
                Some(TaskNotice {
                    code: "network/throttled".to_string(),
                    attempt: 1,
                    max: 3,
                }),
            )
            .await;

        assert_eq!(
            h.task(&task.id).await.notice,
            None,
            "a notice landing after the task left Running must be dropped, not resurrected"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_completion_beats_a_late_stop_request() {
        let h = harness(1);
        // A runner that cannot see its token stands in for a real one already
        // inside `CompleteMultipartUpload`: the bytes are committed and no
        // abort will be issued, so recording "Canceled" would put that status
        // next to an object that exists in the bucket.
        h.runner.set_mode(FakeMode::IgnoreToken);
        let task = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "the task starts",
        )
        .await;

        h.engine.cancel(&task.id).await.unwrap();
        h.runner.finish.store(true, Ordering::SeqCst);

        eventually(
            || h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Completed),
            "a committed transfer is recorded as Completed",
        )
        .await;
        assert_eq!(
            h.sink.statuses_of(&task.id),
            vec![
                TransferStatus::Queued,
                TransferStatus::Running,
                TransferStatus::Completed
            ],
            "a stop requested while the runner was committing must not overwrite the completion"
        );
        assert_eq!(h.task(&task.id).await.transferred, 1024);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_runner_reporting_stopped_without_a_stop_request_fails_the_task() {
        let h = harness(1);
        h.runner.set_mode(FakeMode::BogusStop);
        let task = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "the task starts",
        )
        .await;
        h.runner.finish.store(true, Ordering::SeqCst);

        eventually(
            || h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Failed),
            "a bogus Stopped fails the task instead of stranding it in Running",
        )
        .await;
        assert_eq!(
            h.task(&task.id).await.error_code.as_deref(),
            Some("internal"),
            "the runner bug must be visible as an internal error, not a plausible stop"
        );
        assert_eq!(h.forgets_of(&task.id), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_panicking_runner_fails_its_task_and_keeps_the_engine_scheduling() {
        let h = harness(1);
        h.runner.set_mode(FakeMode::Panic);
        let boom = h.engine.enqueue(spec("a")).await.unwrap();

        // Without catching the panic the driver dies with the runner: no
        // transition is ever applied, the row sits in `Running` forever, and
        // neither `cancel` (which only requests a stop nobody observes) nor
        // `clear_finished` (which skips non-terminal rows) can rescue it.
        eventually(
            || h.sink.statuses_of(&boom.id).last() == Some(&TransferStatus::Failed),
            "a panicking runner fails its task instead of stranding it in Running",
        )
        .await;
        assert_eq!(
            h.task(&boom.id).await.error_code.as_deref(),
            Some("internal")
        );
        assert_eq!(h.forgets_of(&boom.id), 1);

        // ... and the engine must go on scheduling afterwards.
        h.runner.set_mode(FakeMode::Normal);
        let next = h.engine.enqueue(spec("b")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 2,
            "the next task is still admitted after a panic",
        )
        .await;
        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(
            || h.sink.statuses_of(&next.id).last() == Some(&TransferStatus::Completed),
            "the task admitted after the panic runs to completion",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn completing_a_task_stops_the_aggregator_tracking_it() {
        let h = harness(1);
        let task = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "the task starts",
        )
        .await;
        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(
            || h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Completed),
            "the task reaches Completed",
        )
        .await;

        eventually(
            || h.forgets_of(&task.id) == 1,
            "a completed task sends exactly one Forget so the aggregator drops its entry",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pausing_a_task_keeps_its_progress_in_the_aggregator() {
        let h = harness(1);
        h.runner.report_bytes.store(256, Ordering::SeqCst);
        let task = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.reported.load(Ordering::SeqCst) == 1,
            "the runner reports its first bytes",
        )
        .await;

        h.engine.pause(&task.id).await.unwrap();
        eventually(
            || h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Paused),
            "the task reaches Paused",
        )
        .await;

        // Give a stray Forget every chance to arrive before concluding it did
        // not.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            h.forgets_of(&task.id),
            0,
            "pausing must not Forget: the aggregator's last figure is what keeps the paused row \
             showing the progress it froze instead of blanking it"
        );
        assert_eq!(h.task(&task.id).await.transferred, 256);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resuming_restarts_byte_accounting_from_zero() {
        let h = harness(1);
        // A resumed runner re-reports work it already reported, so 256 bytes
        // reported twice must still read 256/1024 rather than 512/1024 -- and
        // both counters have to be zeroed at the same instant, or whichever one
        // was missed keeps double-counting.
        h.runner.report_bytes.store(256, Ordering::SeqCst);
        let task = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.reported.load(Ordering::SeqCst) == 1,
            "the first run reports its bytes",
        )
        .await;
        h.engine.pause(&task.id).await.unwrap();
        eventually(
            || h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Paused),
            "the task reaches Paused",
        )
        .await;
        assert_eq!(h.task(&task.id).await.transferred, 256);

        h.engine.resume(&task.id).await.unwrap();
        eventually(
            || h.runner.reported.load(Ordering::SeqCst) == 2,
            "the resumed run reports its bytes",
        )
        .await;

        assert_eq!(
            h.task(&task.id).await.transferred,
            256,
            "restart must zero the engine's counter, or the replayed 256 bytes read as 512/1024"
        );
        assert_eq!(
            h.forgets_of(&task.id),
            1,
            "restart must Forget, or the aggregator adds the replayed bytes to the old total"
        );
    }

    #[tokio::test]
    async fn retract_sends_a_compensating_message_to_the_aggregator() {
        // Direct construction rather than the full Harness: this is a
        // narrow contract on ProgressHandle itself -- retract must both
        // roll back its own atomic (already covered by the doc comment's
        // fetch_update) and tell the aggregator, so a task that keeps
        // running after a retract does not permanently over-report over
        // IPC (Task 3's retry-while-running case).
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = ProgressHandle {
            tx,
            task_id: "t1".to_string(),
            total: 100,
            transferred: Arc::new(AtomicU64::new(20)),
        };

        handle.retract(7);

        assert_eq!(
            handle.transferred.load(Ordering::Relaxed),
            13,
            "the atomic must still roll back exactly as before"
        );
        match rx.try_recv().expect("retract must send a message") {
            ProgressMsg::Retract { task_id, bytes } => {
                assert_eq!(task_id, "t1");
                assert_eq!(bytes, 7);
            }
            other => panic!("expected ProgressMsg::Retract, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn snapshot_is_newest_first() {
        let h = harness(1);
        let a = h.engine.enqueue(spec("a")).await.unwrap();
        let b = h.engine.enqueue(spec("b")).await.unwrap();
        let c = h.engine.enqueue(spec("c")).await.unwrap();
        let ids: Vec<String> = h
            .engine
            .snapshot()
            .await
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![c.id, b.id, a.id]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn summary_aggregates_only_active_tasks_but_all_transferred_bytes() {
        // One of each status the state machine can produce, all live in the
        // table at once, so `summary()` has to actually filter by
        // `is_active()` rather than just summing everything. `max_tasks(1)`
        // is what makes "queued" deterministically Queued: once "running" is
        // admitted and left parked, the one slot never frees, so nothing else
        // can start.
        let h = harness(1);

        h.runner.report_bytes.store(1000, Ordering::SeqCst);
        let done = h
            .engine
            .enqueue(EnqueueSpec {
                total: 1000,
                ..spec("done")
            })
            .await
            .unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "task done starts",
        )
        .await;
        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(
            || h.sink.statuses_of(&done.id).last() == Some(&TransferStatus::Completed),
            "task done reaches Completed",
        )
        .await;
        // Both flags are sticky (checked at the top of every run), so the next
        // task must not inherit them.
        h.runner.finish.store(false, Ordering::SeqCst);
        h.runner.report_bytes.store(200, Ordering::SeqCst);
        *h.runner.fail_with.lock().unwrap() = Some(AppError::AccessDenied);
        let failed = h
            .engine
            .enqueue(EnqueueSpec {
                total: 2000,
                ..spec("failed")
            })
            .await
            .unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 2,
            "task failed starts",
        )
        .await;
        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(
            || h.sink.statuses_of(&failed.id).last() == Some(&TransferStatus::Failed),
            "task failed reaches Failed",
        )
        .await;
        h.runner.finish.store(false, Ordering::SeqCst);
        h.runner.report_bytes.store(300, Ordering::SeqCst);
        let paused = h
            .engine
            .enqueue(EnqueueSpec {
                total: 900,
                ..spec("paused")
            })
            .await
            .unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 3,
            "task paused starts",
        )
        .await;
        h.engine.pause(&paused.id).await.unwrap();
        eventually(
            || h.sink.statuses_of(&paused.id).last() == Some(&TransferStatus::Paused),
            "task paused reaches Paused",
        )
        .await;

        h.runner.report_bytes.store(500, Ordering::SeqCst);
        let running = h
            .engine
            .enqueue(EnqueueSpec {
                total: 4000,
                ..spec("running")
            })
            .await
            .unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 4,
            "task running starts",
        )
        .await;
        // Left parked: `finish` stays false and nobody cancels it, so it
        // never yields the one slot back.
        h.runner.report_bytes.store(0, Ordering::SeqCst);
        let queued = h
            .engine
            .enqueue(EnqueueSpec {
                total: 3000,
                ..spec("queued")
            })
            .await
            .unwrap();

        let summary = h.engine.summary().await;
        assert_eq!(
            summary.active_count, 2,
            "only running + queued are active; done, failed, paused are not"
        );
        assert_eq!(
            summary.active_transferred, 500,
            "running has reported 500 bytes; queued has never run and reports 0"
        );
        assert_eq!(
            summary.active_total,
            running.total + queued.total,
            "active_total sums only the active tasks' totals"
        );
        assert_eq!(
            summary.all_transferred, 2000,
            "all_transferred = done(1000) + failed(200) + paused(300) + running(500) + \
             queued(0), regardless of status"
        );
        assert_eq!(summary.completed_count, 1, "only \"done\" ever completed");
        assert_eq!(
            summary.paused_count, 1,
            "only \"paused\" sits in the Paused bucket"
        );
        assert_eq!(
            summary.paused_transferred, 300,
            "paused_transferred reads \"paused\"'s live atomic, not a stale dto"
        );
        assert_eq!(
            summary.paused_total, paused.total,
            "paused_total sums only the paused tasks' totals"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn clear_finished_keeps_everything_still_actionable() {
        let h = harness(1);
        let done = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "task a starts",
        )
        .await;
        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(
            || h.sink.statuses_of(&done.id).last() == Some(&TransferStatus::Completed),
            "task a reaches Completed",
        )
        .await;
        // The finish flag is sticky, unlike `Notify`'s wake-only-current-waiters
        // semantics: clear it so "b" actually parks instead of completing
        // instantly and being swept away by `clear_finished`.
        h.runner.finish.store(false, Ordering::SeqCst);

        let queued = h.engine.enqueue(spec("b")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 2,
            "task b starts",
        )
        .await;

        h.engine.clear_finished().await;
        let ids: Vec<String> = h
            .engine
            .snapshot()
            .await
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(
            ids,
            vec![queued.id],
            "only Completed/Canceled tasks may be dropped"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn clear_finished_keeps_paused_and_failed() {
        // The sibling test above only ever exercises Completed-dropped and
        // Running-kept, so `retain(!is_terminal)` could become
        // `retain(is_active)` -- which destroys `Paused` and `Failed` too --
        // without a single test noticing. Those are exactly the rows whose loss
        // costs the user work: they are the ones that can still be resumed or
        // retried.
        let h = harness(1);

        let paused = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "task a starts",
        )
        .await;
        h.engine.pause(&paused.id).await.unwrap();
        eventually(
            || h.sink.statuses_of(&paused.id).last() == Some(&TransferStatus::Paused),
            "task a reaches Paused",
        )
        .await;

        // Only arm the failure now: the fake takes `fail_with` at the end of
        // *every* run, and "a" has just finished one.
        *h.runner.fail_with.lock().unwrap() = Some(AppError::AccessDenied);
        let failed = h.engine.enqueue(spec("b")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 2,
            "task b starts",
        )
        .await;
        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(
            || h.sink.statuses_of(&failed.id).last() == Some(&TransferStatus::Failed),
            "task b reaches Failed",
        )
        .await;

        let done = h.engine.enqueue(spec("c")).await.unwrap();
        eventually(
            || h.sink.statuses_of(&done.id).last() == Some(&TransferStatus::Completed),
            "task c reaches Completed",
        )
        .await;

        h.engine.clear_finished().await;
        let mut kept: Vec<String> = h
            .engine
            .snapshot()
            .await
            .into_iter()
            .map(|t| t.id)
            .collect();
        kept.sort();
        let mut expected = vec![paused.id, failed.id];
        expected.sort();
        assert_eq!(
            kept, expected,
            "clear_finished may drop only terminal tasks; Paused and Failed are still resumable \
             or retryable, so sweeping them would destroy actionable work"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn completed_count_survives_clear_finished() {
        // `completed_count` exists precisely because `Completed` rows do not:
        // `clear_finished` reaps them. So the counter has to be read *before*
        // the row is gone (it isn't -- it's a separate atomic, not derived
        // from the table) and it must not reset or drop when the row does.
        // A Paused task rides along to prove the sweep -- which does touch
        // the table -- leaves the other summary buckets alone too.
        let h = harness(2);

        let done = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "task a starts",
        )
        .await;
        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(
            || h.sink.statuses_of(&done.id).last() == Some(&TransferStatus::Completed),
            "task a reaches Completed",
        )
        .await;
        h.runner.finish.store(false, Ordering::SeqCst);

        let paused = h.engine.enqueue(spec("b")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 2,
            "task b starts",
        )
        .await;
        h.engine.pause(&paused.id).await.unwrap();
        eventually(
            || h.sink.statuses_of(&paused.id).last() == Some(&TransferStatus::Paused),
            "task b reaches Paused",
        )
        .await;

        let before = h.engine.summary().await;
        assert_eq!(
            before.completed_count, 1,
            "the one task that ran to completion is counted exactly once"
        );
        assert_eq!(before.paused_count, 1, "the paused task is in the bucket");

        h.engine.clear_finished().await;

        let after = h.engine.summary().await;
        assert_eq!(
            after.completed_count, 1,
            "clear_finished reaps the Completed row, but the monotonic counter \
             that already recorded it must not move"
        );
        assert_eq!(
            after.paused_count, 1,
            "clear_finished must not touch the still-actionable Paused row either"
        );
        assert_eq!(
            after.active_count, 0,
            "no active tasks were ever enqueued in this test"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_on_a_task_with_no_driver_applies_the_transition_itself() {
        // The third bullet of the module's headline invariant: `Paused` and
        // `Failed` tasks have no live driver, so nobody but `cancel` itself can
        // ever write their `Canceled`. Both assertions below are synchronous on
        // purpose -- there is no driver left to do it later.
        let h = harness(1);

        let paused = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "task a starts",
        )
        .await;
        h.engine.pause(&paused.id).await.unwrap();
        eventually(
            || h.sink.statuses_of(&paused.id).last() == Some(&TransferStatus::Paused),
            "task a reaches Paused",
        )
        .await;

        h.engine.cancel(&paused.id).await.unwrap();
        assert_eq!(
            h.sink.statuses_of(&paused.id).last(),
            Some(&TransferStatus::Canceled),
            "cancelling a Paused task must apply the transition inside `cancel`"
        );

        *h.runner.fail_with.lock().unwrap() = Some(AppError::AccessDenied);
        let failed = h.engine.enqueue(spec("b")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 2,
            "task b starts",
        )
        .await;
        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(
            || h.sink.statuses_of(&failed.id).last() == Some(&TransferStatus::Failed),
            "task b reaches Failed",
        )
        .await;

        h.engine.cancel(&failed.id).await.unwrap();
        assert_eq!(
            h.sink.statuses_of(&failed.id).last(),
            Some(&TransferStatus::Canceled),
            "cancelling a Failed task must apply the transition inside `cancel`"
        );
    }

    /// Reads a task's resume handle straight out of the table. The tests below
    /// stand in for a runner that paused mid-multipart by seeding the slot
    /// themselves -- the fake runner never touches it.
    async fn resume_handle(h: &Harness, task_id: &str) -> Arc<Mutex<Option<ResumeState>>> {
        let tasks = h.engine.inner.tasks.lock().await;
        Arc::clone(&tasks.get(task_id).expect("task present").resume)
    }

    fn multipart_state(upload_id: &str) -> MultipartState {
        MultipartState {
            upload_id: upload_id.to_string(),
            completed: vec![UploadedPart {
                number: 1,
                etag: "\"e1\"".to_string(),
                size: 8,
            }],
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_a_paused_task_drains_its_orphaned_multipart_state() {
        // I-2 path 1: a `Paused` task holding multipart state is cancelled with
        // no live runner, so `cancel` applies `Canceled` itself and must abort
        // the orphaned upload -- nobody else will. The unit harness has no live
        // endpoint, so the network abort cannot fire; but the engine *reaching*
        // the abort path is observable: `cleanup_orphaned_transfer` `take()`s the
        // resume slot before it ever touches the network, so a drained slot
        // proves the path ran and the discriminator (`resume` still `Some`) fired
        // before the warn-and-return on the unreachable endpoint.
        let h = harness(1);
        let paused = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "task a starts",
        )
        .await;
        h.engine.pause(&paused.id).await.unwrap();
        eventually(
            || h.sink.statuses_of(&paused.id).last() == Some(&TransferStatus::Paused),
            "task a reaches Paused",
        )
        .await;

        let resume = resume_handle(&h, &paused.id).await;
        *resume.lock().await = Some(ResumeState::Upload(multipart_state("u-1")));

        h.engine.cancel(&paused.id).await.unwrap();
        assert_eq!(
            h.sink.statuses_of(&paused.id).last(),
            Some(&TransferStatus::Canceled),
            "cancelling a Paused task must still apply Canceled"
        );
        assert!(
            resume.lock().await.is_none(),
            "the engine must drain the resume slot on its own cancel-apply; a slot left Some means \
             the abort path never ran and the fragments would leak"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_a_resumed_but_queued_task_drains_its_orphaned_multipart_state() {
        // I-2 path 3: a resumed task can sit `Queued` on the semaphore with
        // multipart state in its resume slot. `Queued` is `is_active`, so
        // `cancel` only fires the token; the parked driver wakes on it via the
        // biased `select!` and applies `Cancel` through `apply_stop` -- without
        // ever acquiring a permit or invoking the runner. That engine-side apply
        // must also abort the orphaned upload. Same observation as path 1: the
        // drained slot proves `cleanup_orphaned_transfer` ran.
        let h = harness(1);
        // Task a takes the only permit and parks, so b is provably stuck Queued.
        let running = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "task a takes the only permit",
        )
        .await;

        let queued = h.engine.enqueue(spec("b")).await.unwrap();
        // Stand in for a resumed task whose earlier run left multipart state.
        let resume = resume_handle(&h, &queued.id).await;
        *resume.lock().await = Some(ResumeState::Upload(multipart_state("u-2")));

        h.engine.cancel(&queued.id).await.unwrap();
        eventually(
            || h.sink.statuses_of(&queued.id).last() == Some(&TransferStatus::Canceled),
            "the queued task reaches Canceled via its driver",
        )
        .await;
        assert_eq!(
            h.runner.started.load(Ordering::SeqCst),
            1,
            "the queued task must be cancelled without the runner ever being invoked"
        );
        assert!(
            resume.lock().await.is_none(),
            "the driver's apply_stop must drain the resume slot; a slot left Some means the abort \
             path never ran and the fragments would leak"
        );
        let _ = running;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_a_paused_task_without_multipart_state_is_a_plain_cancel() {
        // The discriminator must not fire on a single-stream task: with an empty
        // resume slot there is no upload to abort, and `cleanup_orphaned_transfer`
        // must return without incident. (A `None` slot never reaches the network
        // build at all.)
        let h = harness(1);
        let paused = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "task a starts",
        )
        .await;
        h.engine.pause(&paused.id).await.unwrap();
        eventually(
            || h.sink.statuses_of(&paused.id).last() == Some(&TransferStatus::Paused),
            "task a reaches Paused",
        )
        .await;

        let resume = resume_handle(&h, &paused.id).await;
        assert!(
            resume.lock().await.is_none(),
            "no multipart state was seeded"
        );

        h.engine.cancel(&paused.id).await.unwrap();
        assert_eq!(
            h.sink.statuses_of(&paused.id).last(),
            Some(&TransferStatus::Canceled),
            "a Paused single-stream task still cancels cleanly"
        );
        assert!(resume.lock().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_a_paused_download_removes_its_orphaned_bcpart() {
        // The download-direction mirror of the upload paths above. A `Paused`
        // download holds a `.bcpart` staging file in its resume slot; cancelled
        // with no live runner, `cancel` applies `Canceled` itself and
        // `cleanup_orphaned_transfer` must delete that staging file -- nobody
        // else will, and a leaked `.bcpart` is the download-side analogue of a
        // leaked server-side multipart. The drained slot plus the vanished file
        // prove the `Download` arm ran (the `take()` discriminator fired before
        // the removal).
        let dir = tempfile::tempdir().unwrap();
        let bcpart = dir.path().join("photo.jpg.bcpart");
        std::fs::write(&bcpart, b"partial download").unwrap();
        assert!(bcpart.exists(), "the staging file must exist before cancel");

        let h = harness(1);
        let local = dir.path().join("photo.jpg");
        let paused = h
            .engine
            .enqueue(download_spec("photo.jpg", &local.to_string_lossy()))
            .await
            .unwrap();
        eventually(
            || h.runner.started.load(Ordering::SeqCst) == 1,
            "the download starts",
        )
        .await;
        h.engine.pause(&paused.id).await.unwrap();
        eventually(
            || h.sink.statuses_of(&paused.id).last() == Some(&TransferStatus::Paused),
            "the download reaches Paused",
        )
        .await;

        // Stand in for a runner that paused mid-download by seeding the slot.
        let resume = resume_handle(&h, &paused.id).await;
        *resume.lock().await = Some(ResumeState::Download(DownloadState {
            etag: Some("\"e1\"".to_string()),
            completed_parts: vec![1],
            bcpart: bcpart.clone(),
            part_size: 8 * 1024 * 1024,
        }));

        h.engine.cancel(&paused.id).await.unwrap();
        assert_eq!(
            h.sink.statuses_of(&paused.id).last(),
            Some(&TransferStatus::Canceled),
            "cancelling a Paused download must still apply Canceled"
        );
        assert!(
            resume.lock().await.is_none(),
            "the engine must drain the resume slot on its own cancel-apply; a slot left Some means \
             the cleanup path never ran"
        );
        assert!(
            !bcpart.exists(),
            "the orphaned .bcpart must be deleted on cancel; a surviving staging file means the \
             Download cleanup arm never ran and stale .bcpart files would accumulate"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn commands_against_an_unknown_task_are_reported_not_ignored() {
        let h = harness(1);
        for result in [
            h.engine.pause("nope").await,
            h.engine.resume("nope").await,
            h.engine.cancel("nope").await,
            h.engine.retry("nope").await,
        ] {
            assert_eq!(result.unwrap_err().code(), "storage/task-not-found");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn enqueue_rejects_a_missing_local_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone.bin");
        let err =
            EnqueueSpec::for_upload("c1".to_string(), "b".to_string(), "k".to_string(), missing)
                .unwrap_err();
        assert_eq!(err.code(), "local/file-io");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn for_upload_reads_size_and_basename_from_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, vec![7u8; 4096]).unwrap();

        let spec = EnqueueSpec::for_upload(
            "c1".to_string(),
            "b".to_string(),
            "album/photo.jpg".to_string(),
            path.clone(),
        )
        .unwrap();
        assert_eq!(spec.total, 4096);
        assert_eq!(spec.file_name, "photo.jpg");
        assert_eq!(spec.local_path, path.to_string_lossy());
        assert_eq!(spec.direction, Direction::Upload);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_running_upload_writes_a_checkpoint_then_removes_it_on_completion() {
        let cp_dir = tempfile::tempdir().unwrap();
        let h = harness_cfg(1, Some(cp_dir.path().to_path_buf()), true);
        // A real multipart runner has an upload id worth persisting; the fake
        // stands in by mirroring one resume state through the engine's *real*
        // CheckpointWriter once it is running with progress. The assertions
        // below observe genuine file state via `checkpoint::scan`, not a mock.
        h.runner.report_bytes.store(256, Ordering::SeqCst);
        *h.runner.checkpoint_state.lock().unwrap() =
            Some(ResumeState::Upload(multipart_state("u-live")));

        let task = h.engine.enqueue(spec("big")).await.unwrap();
        eventually(
            || h.runner.reported.load(Ordering::SeqCst) == 1,
            "the running upload reports progress",
        )
        .await;
        assert!(
            !checkpoint::scan(cp_dir.path()).is_empty(),
            "a checkpoint must be written mid-flight so a crash can resume the transfer"
        );

        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(
            || h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Completed),
            "the upload reaches Completed",
        )
        .await;
        eventually(
            || checkpoint::scan(cp_dir.path()).is_empty(),
            "the checkpoint must be removed on completion; a survivor would resume a finished task",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn disabling_resume_writes_no_checkpoint() {
        let cp_dir = tempfile::tempdir().unwrap();
        let h = harness_cfg(1, Some(cp_dir.path().to_path_buf()), false);
        // Same running-with-progress scenario as the sibling test, but with the
        // resume flag off: the gate must suppress every write.
        h.runner.report_bytes.store(256, Ordering::SeqCst);
        *h.runner.checkpoint_state.lock().unwrap() =
            Some(ResumeState::Upload(multipart_state("u-live")));

        let task = h.engine.enqueue(spec("big")).await.unwrap();
        eventually(
            || h.runner.reported.load(Ordering::SeqCst) == 1,
            "the running upload reports progress",
        )
        .await;
        // Give any stray write every chance to land before concluding none did.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            checkpoint::scan(cp_dir.path()).is_empty(),
            "with resume disabled the gate must suppress every checkpoint write, even mid-flight"
        );
        let _ = task;
    }

    #[tokio::test(start_paused = true)]
    async fn persist_coalesces_non_forced_writes_but_force_always_lands() {
        // Unit cover for the throttle, on the virtual clock so it needs no real
        // sleeps. Writes are distinguished by upload id, so "did this write
        // land" is observable straight from the file's content.
        let cp_dir = tempfile::tempdir().unwrap();
        let writer = CheckpointWriter {
            dir: cp_dir.path().to_path_buf(),
            task_id: "t".to_string(),
            statics: CheckpointStatics {
                direction: Direction::Upload,
                connection_id: "c1".to_string(),
                bucket: "b".to_string(),
                key: "k".to_string(),
                local_path: "/tmp/k".to_string(),
                file_name: "k".to_string(),
                total: 1024,
            },
            enabled: Arc::new(AtomicBool::new(true)),
            last: Arc::new(std::sync::Mutex::new(None)),
        };
        let state = |id: &str| ResumeState::Upload(multipart_state(id));
        let persisted_upload_id = || {
            let found = checkpoint::scan(cp_dir.path());
            assert_eq!(found.len(), 1, "exactly one checkpoint file");
            match &found[0].1.resume {
                ResumeState::Upload(ms) => ms.upload_id.clone(),
                _ => panic!("expected an upload checkpoint"),
            }
        };

        writer.persist(&state("first"), false); // last == None -> writes
        assert_eq!(persisted_upload_id(), "first");
        writer.persist(&state("coalesced"), false); // within 2s -> skipped
        assert_eq!(
            persisted_upload_id(),
            "first",
            "a non-force write inside the 2s window must be coalesced away"
        );
        writer.persist(&state("forced"), true); // force -> ignores the throttle
        assert_eq!(
            persisted_upload_id(),
            "forced",
            "a force write must land regardless of the throttle"
        );

        tokio::time::advance(Duration::from_secs(3)).await;
        writer.persist(&state("after-window"), false); // >2s since last -> writes
        assert_eq!(
            persisted_upload_id(),
            "after-window",
            "a non-force write after the window elapses must land"
        );

        // And the gate wins over everything: a disabled writer never writes.
        writer.enabled.store(false, Ordering::Relaxed);
        writer.persist(&state("gated"), true);
        assert_eq!(
            persisted_upload_id(),
            "after-window",
            "a disabled writer must not write even with force"
        );
    }

    /// A download checkpoint marking two of a 100 MiB object's chunks done.
    /// Under the default (balanced) tuning, `plan_download(100 MiB, ...)`
    /// splits it into four 32 MiB chunks (the last short), so "chunks 1 and 2
    /// done" means the restored task's `transferred` must be exactly
    /// `2 * 32 MiB`.
    fn download_checkpoint_with_two_completed_chunks() -> Checkpoint {
        Checkpoint {
            direction: Direction::Download,
            connection_id: "c1".to_string(),
            bucket: "b".to_string(),
            key: "big.bin".to_string(),
            local_path: "/tmp/big.bin".to_string(),
            file_name: "big.bin".to_string(),
            total: 100 * 1024 * 1024,
            resume: ResumeState::Download(DownloadState {
                etag: Some("\"e1\"".to_string()),
                completed_parts: vec![1, 2],
                bcpart: std::path::PathBuf::from("/tmp/big.bin.bcpart"),
                part_size: 32 * 1024 * 1024,
            }),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restore_paused_rebuilds_a_paused_task_without_a_driver() {
        // The cross-restart counterpart to a pause: a checkpoint the previous
        // session left behind is rebuilt as a `Paused` row, offline, with its
        // already-done bytes preset -- but no driver is spawned, so nothing
        // touches the network and the task waits for the user to resume it.
        let h = harness(1);
        h.engine
            .restore_paused(
                "t-restored".to_string(),
                download_checkpoint_with_two_completed_chunks(),
            )
            .await;

        let dto = h.task("t-restored").await;
        assert_eq!(
            dto.status,
            TransferStatus::Paused,
            "a restored checkpoint must come back as Paused"
        );
        assert_eq!(
            dto.transferred,
            64 * 1024 * 1024,
            "the two completed 32 MiB chunks must be preset as transferred, not left at 0"
        );
        // The panel learns of the restored row only through the sink -- there
        // is no IPC return value on the startup path.
        assert_eq!(
            h.sink.statuses_of("t-restored"),
            vec![TransferStatus::Paused],
            "restore must emit exactly one Paused state so the panel shows the row"
        );

        // Give any (erroneously spawned) driver every chance to run, then prove
        // none did: the fake runner never started, and the status never left
        // Paused.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            h.runner.started.load(Ordering::SeqCst),
            0,
            "restore_paused must not spawn a driver; the runner must never be invoked"
        );
        assert_eq!(
            h.task("t-restored").await.status,
            TransferStatus::Paused,
            "with no driver the task can only stay Paused"
        );
    }

    // --- plan_restore: the pure startup-restore decision -------------------
    //
    // The safety-critical branch of startup restore, tested without a live
    // Tauri app or hub. `restore_all` maps this plan to its side effects
    // (restore_paused / remove files), but the *decision* is here.

    /// A minimal upload checkpoint tagged with `conn` as its connection id --
    /// enough for `plan_restore`, which only ever reads `connection_id`.
    fn cp_for(conn: &str) -> Checkpoint {
        Checkpoint {
            direction: Direction::Upload,
            connection_id: conn.to_string(),
            bucket: "b".to_string(),
            key: "k".to_string(),
            local_path: "/tmp/k".to_string(),
            file_name: "k".to_string(),
            total: 100,
            resume: ResumeState::Upload(MultipartState {
                upload_id: "u1".to_string(),
                completed: vec![],
                source_size: 100,
                source_mtime: 0,
                part_size: 0,
            }),
        }
    }

    /// THE regression guard for the safety property: when the connection store
    /// cannot be read (`known == Err`), the plan is EMPTY -- nothing is
    /// restored and, crucially, nothing is discarded. Treating an unreadable
    /// store as "no connections exist" would discard/delete every checkpoint
    /// on a transient read failure, the exact catastrophic inversion this
    /// factoring exists to prevent.
    #[test]
    fn plan_restore_on_read_error_leaves_everything() {
        let scanned = vec![
            ("t1".to_string(), cp_for("c1")),
            ("t2".to_string(), cp_for("cX")),
        ];
        let known: AppResult<HashSet<String>> = Err(AppError::Internal {
            message: "store unreadable".to_string(),
        });
        let plan = plan_restore(scanned, &known);
        assert!(
            plan.is_empty(),
            "a read error must produce an empty plan: restore nothing, discard nothing, got {plan:?}"
        );
    }

    /// With a readable store, a checkpoint whose connection still exists is
    /// restored, and one whose connection is gone is discarded as an orphan.
    #[test]
    fn plan_restore_restores_known_and_discards_orphan() {
        let scanned = vec![
            ("t-known".to_string(), cp_for("c1")),
            ("t-orphan".to_string(), cp_for("cX")),
        ];
        let known: AppResult<HashSet<String>> = Ok(HashSet::from(["c1".to_string()]));
        let plan = plan_restore(scanned, &known);
        assert_eq!(plan.len(), 2, "both scanned checkpoints must be planned");

        match &plan[0] {
            RestoreAction::Restore(id, cp) => {
                assert_eq!(id, "t-known");
                assert_eq!(cp.connection_id, "c1");
            }
            other => panic!("a checkpoint for a known connection must be Restore, got {other:?}"),
        }
        match &plan[1] {
            RestoreAction::DiscardOrphan(id, cp) => {
                assert_eq!(id, "t-orphan");
                assert_eq!(cp.connection_id, "cX");
            }
            other => {
                panic!(
                    "a checkpoint for an unknown connection must be DiscardOrphan, got {other:?}"
                )
            }
        }
    }

    /// An empty known-set (every connection was deleted, but the store read
    /// SUCCEEDED) is distinct from a read error: here every checkpoint is a
    /// genuine orphan and must be discarded -- proving `plan_restore` keys the
    /// "leave everything" behaviour on `Err`, not on emptiness.
    #[test]
    fn plan_restore_with_empty_known_set_discards_all_as_orphans() {
        let scanned = vec![
            ("t1".to_string(), cp_for("c1")),
            ("t2".to_string(), cp_for("c2")),
        ];
        let known: AppResult<HashSet<String>> = Ok(HashSet::new());
        let plan = plan_restore(scanned, &known);
        assert_eq!(plan.len(), 2);
        assert!(
            plan.iter()
                .all(|a| matches!(a, RestoreAction::DiscardOrphan(_, _))),
            "a successful read of an empty store makes every checkpoint an orphan: {plan:?}"
        );
    }

    // --- part_size: checkpoint round-trip + restored-bytes derivation ------
    //
    // Task 3 (spec §4.6): chunking is user-configurable (M6+), so a
    // checkpoint cut under one tuning must never be trusted under another --
    // its `completed_parts` chunk numbers would map to different byte
    // ranges. These cover the serde-compat side (a legacy file with no
    // `part_size` reads as 0) and the `checkpoint_completed_bytes` side
    // (restored progress is derived from the *recorded* part size, not
    // whatever tuning happens to be active now).

    /// A minimal download checkpoint carrying `resume`, for the
    /// `checkpoint_completed_bytes` tests below -- the rest of the fields are
    /// irrelevant to that pure function.
    fn checkpoint_with(resume: ResumeState, total: u64) -> Checkpoint {
        Checkpoint {
            direction: Direction::Download,
            connection_id: "c1".to_string(),
            bucket: "b".to_string(),
            key: "k".to_string(),
            local_path: "/tmp/k".to_string(),
            file_name: "k".to_string(),
            total,
            resume,
        }
    }

    #[test]
    fn legacy_checkpoint_without_part_size_reads_as_zero() {
        let json = r#"{"Download":{"etag":null,"completed_parts":[1,2],"bcpart":"/tmp/x.bcpart"}}"#;
        let ResumeState::Download(ds) = serde_json::from_str::<ResumeState>(json).unwrap() else {
            panic!("expected a Download variant")
        };
        assert_eq!(ds.part_size, 0);
    }

    #[test]
    fn restored_bytes_derive_from_the_recorded_part_size_not_current_settings() {
        // 8MB recorded part size x completed chunks [1,2] = 16MB, no matter
        // what the current tuning preset is.
        let ds = DownloadState {
            etag: None,
            completed_parts: vec![1, 2],
            bcpart: "/tmp/x.bcpart".into(),
            part_size: 8 * 1024 * 1024,
        };
        let cp = checkpoint_with(ResumeState::Download(ds), 100 * 1024 * 1024);
        assert_eq!(checkpoint_completed_bytes(&cp), 16 * 1024 * 1024);
    }

    #[test]
    fn zero_part_size_restores_zero_bytes() {
        // A legacy checkpoint is discarded wholesale on resume, so the
        // displayed progress must agree and show zero rather than a
        // misleading partial figure.
        let ds = DownloadState {
            etag: None,
            completed_parts: vec![1],
            bcpart: "/tmp/x.bcpart".into(),
            part_size: 0,
        };
        let cp = checkpoint_with(ResumeState::Download(ds), 100 * 1024 * 1024);
        assert_eq!(checkpoint_completed_bytes(&cp), 0);
    }
}
