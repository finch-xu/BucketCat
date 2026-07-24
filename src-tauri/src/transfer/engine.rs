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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::provider::{Provider, ProviderHub, UploadedPart};
use crate::transfer::model::{
    next_status, Direction, TransferCommand, TransferStatus, TransferTaskDto,
};
use crate::transfer::progress::ProgressMsg;

/// Concurrency limits (design §5). Runtime hot-update belongs to the settings
/// page (M6); M4a reads these once at construction.
#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    /// Tasks allowed to be `Running` at once.
    pub max_tasks: usize,
    /// Parts a single task may have in flight.
    pub max_parts: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_tasks: 3,
            max_parts: 4,
        }
    }
}

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
/// where it left off. **Memory only** -- surviving an app restart is M4c's
/// job (`store/checkpoint.rs`).
#[derive(Debug, Clone, Default)]
pub struct MultipartState {
    pub upload_id: String,
    pub completed: Vec<UploadedPart>,
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
}

/// Everything a runner needs. Deliberately concrete: the runner is chosen at
/// engine construction, so there is no reason to abstract this further.
pub struct TaskContext {
    pub task: TransferTaskDto,
    pub hub: Arc<ProviderHub>,
    pub control: TaskControl,
    pub part_limit: usize,
    pub progress: ProgressHandle,
    pub resume: Arc<Mutex<Option<MultipartState>>>,
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
}

struct TaskRecord {
    dto: TransferTaskDto,
    control: TaskControl,
    resume: Arc<Mutex<Option<MultipartState>>>,
    transferred: Arc<AtomicU64>,
}

struct EngineInner {
    hub: Arc<ProviderHub>,
    runner: Arc<dyn TransferRunner>,
    sink: Arc<dyn TransferSink>,
    progress: mpsc::UnboundedSender<ProgressMsg>,
    tasks: Mutex<HashMap<String, TaskRecord>>,
    task_sem: Arc<Semaphore>,
    seq: AtomicU64,
    config: EngineConfig,
}

impl EngineInner {
    /// Applies a state-machine command and, if it was legal, emits the new
    /// state. The emit happens **after** the lock is released so a slow sink
    /// can never stall the task table.
    async fn apply(
        self: &Arc<Self>,
        task_id: &str,
        cmd: TransferCommand,
        error_code: Option<String>,
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
            record.dto.error_code = if next == TransferStatus::Failed {
                error_code
            } else {
                None
            };
            record.dto.transferred = record.transferred.load(Ordering::Relaxed);
            record.dto.clone()
        };
        tracing::debug!(task = %updated.id, status = ?updated.status, "transfer state");
        self.sink.state_changed(&updated);
        true
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
            permit = Arc::clone(&self.task_sem).acquire_owned() => match permit {
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
        // handle needs out of it first.
        let total = dto.total;
        let ctx = TaskContext {
            task: dto,
            hub: Arc::clone(&self.hub),
            control: control.clone(),
            part_limit: self.config.max_parts,
            progress: ProgressHandle {
                tx: self.progress.clone(),
                task_id: task_id.clone(),
                total,
                transferred,
            },
            resume,
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
                    Some(err.code().to_string()),
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
                self.apply(&task_id, TransferCommand::Complete, None).await;
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
                        Some(err.code().to_string()),
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
                    Some(err.code().to_string()),
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
                    self.abort_orphaned_multipart(task_id).await;
                }
                applied
            }
            None => false,
        }
    }

    /// Best-effort abort of a multipart upload orphaned by a cancel that no
    /// runner handled (a `Paused`/`Failed` task, or a resumed task still
    /// `Queued` waiting for a permit). The runner clears `resume` to `None`
    /// after *it* aborts, so a `resume` still holding an `upload_id` here means
    /// nobody cleaned up and the fragments would bill the user until the
    /// bucket's lifecycle rules reap them.
    ///
    /// Failure is logged, never propagated: a cleanup failure must not turn a
    /// completed cancellation into an error (the same discipline the runner's
    /// own cancel branch follows). `take()` on the `resume` mutex makes this
    /// safe against a racing runner -- whoever takes the `Some` does the abort,
    /// the other sees `None`.
    async fn abort_orphaned_multipart(self: &Arc<Self>, task_id: &str) {
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
        if state.upload_id.is_empty() {
            return;
        }

        let provider = match self.hub.provider(&connection_id).await {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(task = %task_id, "cannot build provider to abort orphaned multipart: {err}");
                return;
            }
        };
        match provider
            .multipart_abort(&bucket, &key, &state.upload_id)
            .await
        {
            Ok(()) => {
                tracing::info!(task = %task_id, "aborted orphaned multipart upload on cancel")
            }
            Err(err) => {
                tracing::warn!(task = %task_id, upload_id = %state.upload_id, "abort of orphaned multipart failed: {err}")
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
        config: EngineConfig,
    ) -> Self {
        Self {
            inner: Arc::new(EngineInner {
                hub,
                runner,
                sink,
                progress,
                tasks: Mutex::new(HashMap::new()),
                task_sem: Arc::new(Semaphore::new(config.max_tasks)),
                seq: AtomicU64::new(0),
                config,
            }),
        }
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
            self.inner.abort_orphaned_multipart(task_id).await;
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

    /// Drops finished tasks. `Paused` and `Failed` stay: the user can still
    /// resume or retry them, so removing them would destroy actionable work.
    pub async fn clear_finished(&self) {
        let mut tasks = self.inner.tasks.lock().await;
        tasks.retain(|_, r| !r.dto.status.is_terminal());
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
            })
        }

        fn set_mode(&self, mode: FakeMode) {
            *self.mode.lock().unwrap() = mode;
        }
    }

    #[async_trait::async_trait]
    impl TransferRunner for FakeRunner {
        async fn run(&self, ctx: TaskContext) -> AppResult<RunOutcome> {
            self.started.fetch_add(1, Ordering::SeqCst);
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
        seen: StdMutex<Vec<(String, TransferStatus)>>,
    }

    impl RecordingSink {
        fn statuses_of(&self, id: &str) -> Vec<TransferStatus> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(task, _)| task == id)
                .map(|(_, status)| *status)
                .collect()
        }
    }

    impl TransferSink for RecordingSink {
        fn state_changed(&self, task: &TransferTaskDto) {
            self.seen
                .lock()
                .unwrap()
                .push((task.id.clone(), task.status));
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
            EngineConfig {
                max_tasks,
                max_parts: 4,
            },
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
    async fn resume_handle(h: &Harness, task_id: &str) -> Arc<Mutex<Option<MultipartState>>> {
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
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_a_paused_task_drains_its_orphaned_multipart_state() {
        // I-2 path 1: a `Paused` task holding multipart state is cancelled with
        // no live runner, so `cancel` applies `Canceled` itself and must abort
        // the orphaned upload -- nobody else will. The unit harness has no live
        // endpoint, so the network abort cannot fire; but the engine *reaching*
        // the abort path is observable: `abort_orphaned_multipart` `take()`s the
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
        *resume.lock().await = Some(multipart_state("u-1"));

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
        // drained slot proves `abort_orphaned_multipart` ran.
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
        *resume.lock().await = Some(multipart_state("u-2"));

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
        // resume slot there is no upload to abort, and `abort_orphaned_multipart`
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
}
