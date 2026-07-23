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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::provider::{ProviderHub, UploadedPart};
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

/// Performs one transfer. `Ok(())` means "the runner is done"; whether that
/// is a completion, a pause or a cancellation is decided by the engine from
/// [`TaskControl::requested`], so runners never have to invent a
/// "stopped early" error.
#[async_trait]
pub trait TransferRunner: Send + Sync + 'static {
    async fn run(&self, ctx: TaskContext) -> AppResult<()>;
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
            biased;
            _ = token.cancelled() => {
                self.apply_stop(&task_id, &control).await;
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
            self.apply_stop(&task_id, &control).await;
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

        let result = self.runner.run(ctx).await;
        // Release the slot before any state bookkeeping so the next queued
        // task starts as early as possible.
        drop(permit);

        let _ = self.progress.send(ProgressMsg::Forget {
            task_id: task_id.clone(),
        });

        if control.requested().is_some() {
            self.apply_stop(&task_id, &control).await;
            return;
        }

        match result {
            Ok(()) => {
                if let Some(record) = self.tasks.lock().await.get(&task_id) {
                    record
                        .transferred
                        .store(record.dto.total, Ordering::Relaxed);
                }
                self.apply(&task_id, TransferCommand::Complete, None).await;
            }
            Err(err) => {
                tracing::warn!(
                    task = %task_id,
                    code = err.code(),
                    "transfer failed: {err}"
                );
                self.apply(
                    &task_id,
                    TransferCommand::Fail,
                    Some(err.code().to_string()),
                )
                .await;
            }
        }
    }

    async fn apply_stop(self: &Arc<Self>, task_id: &str, control: &TaskControl) {
        match control.requested() {
            Some(StopKind::Pause) => {
                self.apply(task_id, TransferCommand::Pause, None).await;
            }
            Some(StopKind::Cancel) => {
                self.apply(task_id, TransferCommand::Cancel, None).await;
            }
            None => {}
        };
    }

    async fn status_of(&self, task_id: &str) -> AppResult<TransferStatus> {
        self.tasks
            .lock()
            .await
            .get(task_id)
            .map(|r| r.dto.status)
            .ok_or_else(|| AppError::TaskNotFound {
                id: task_id.to_string(),
            })
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
    pub async fn pause(&self, task_id: &str) -> AppResult<()> {
        let status = self.inner.status_of(task_id).await?;
        if status != TransferStatus::Running {
            return Ok(());
        }
        if let Some(record) = self.inner.tasks.lock().await.get(task_id) {
            record.control.request(StopKind::Pause);
        }
        Ok(())
    }

    /// Cancels a task from any non-terminal state. When no driver is live
    /// (`Paused` / `Failed`) the transition is applied here, since nobody
    /// else will.
    pub async fn cancel(&self, task_id: &str) -> AppResult<()> {
        let status = self.inner.status_of(task_id).await?;
        if let Some(record) = self.inner.tasks.lock().await.get(task_id) {
            record.control.request(StopKind::Cancel);
        }
        if !status.is_active() {
            self.inner
                .apply(task_id, TransferCommand::Cancel, None)
                .await;
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
    }

    impl FakeRunner {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                live: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                started: AtomicUsize::new(0),
                finish: AtomicBool::new(false),
                fail_with: StdMutex::new(None),
            })
        }
    }

    #[async_trait::async_trait]
    impl TransferRunner for FakeRunner {
        async fn run(&self, ctx: TaskContext) -> AppResult<()> {
            self.started.fetch_add(1, Ordering::SeqCst);
            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(live, Ordering::SeqCst);

            // Bound outside the loop: `token()` hands back an owned clone, and
            // `cancelled()` borrows it, so calling both inline inside
            // `select!` would borrow a temporary that dies too early.
            let token = ctx.control.token();
            loop {
                if self.finish.load(Ordering::SeqCst) {
                    break;
                }
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(2)) => {}
                }
            }

            self.live.fetch_sub(1, Ordering::SeqCst);
            match self.fail_with.lock().unwrap().take() {
                Some(err) => Err(err),
                None => Ok(()),
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
        _dir: tempfile::TempDir,
    }

    fn harness(max_tasks: usize) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let hub = Arc::new(ProviderHub::new(crate::store::SecureStore {
            path: dir.path().join("connections.enc"),
        }));
        let runner = FakeRunner::new();
        let sink = Arc::new(RecordingSink::default());
        let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
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
    async fn eventually(mut check: impl FnMut() -> bool) {
        for _ in 0..200 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition never became true");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn global_concurrency_is_capped_at_max_tasks() {
        let h = harness(3);
        for i in 0..8 {
            h.engine.enqueue(spec(&format!("k{i}"))).await.unwrap();
        }
        eventually(|| h.runner.live.load(Ordering::SeqCst) == 3).await;

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
    async fn finishing_a_task_admits_the_next_queued_one() {
        let h = harness(1);
        h.engine.enqueue(spec("a")).await.unwrap();
        h.engine.enqueue(spec("b")).await.unwrap();
        eventually(|| h.runner.started.load(Ordering::SeqCst) == 1).await;

        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(|| h.runner.started.load(Ordering::SeqCst) == 2).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_running_task_completes() {
        let h = harness(1);
        let task = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(|| h.runner.started.load(Ordering::SeqCst) == 1).await;
        h.runner.finish.store(true, Ordering::SeqCst);

        eventually(|| h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Completed))
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
        eventually(|| h.runner.started.load(Ordering::SeqCst) == 1).await;

        let queued = h.engine.enqueue(spec("b")).await.unwrap();
        h.engine.cancel(&queued.id).await.unwrap();
        eventually(|| h.sink.statuses_of(&queued.id).last() == Some(&TransferStatus::Canceled))
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
        eventually(|| h.runner.started.load(Ordering::SeqCst) == 1).await;

        h.engine.pause(&task.id).await.unwrap();
        // The fake runner only returns when its token fires, so reaching
        // Paused proves cancellation actually propagated into the runner.
        eventually(|| h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Paused)).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resuming_a_paused_task_runs_it_again_with_a_fresh_token() {
        let h = harness(1);
        let task = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(|| h.runner.started.load(Ordering::SeqCst) == 1).await;
        h.engine.pause(&task.id).await.unwrap();
        eventually(|| h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Paused)).await;

        h.engine.resume(&task.id).await.unwrap();
        // A stale (already-cancelled) token would make the runner return
        // instantly; reaching a second start proves the token was replaced.
        eventually(|| h.runner.started.load(Ordering::SeqCst) == 2).await;
        eventually(|| h.runner.live.load(Ordering::SeqCst) == 1).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_task_records_its_error_code_and_can_be_retried() {
        let h = harness(1);
        *h.runner.fail_with.lock().unwrap() = Some(AppError::AccessDenied);
        let task = h.engine.enqueue(spec("a")).await.unwrap();
        eventually(|| h.runner.started.load(Ordering::SeqCst) == 1).await;
        h.runner.finish.store(true, Ordering::SeqCst);

        eventually(|| h.sink.statuses_of(&task.id).last() == Some(&TransferStatus::Failed)).await;
        let failed = h
            .engine
            .snapshot()
            .await
            .into_iter()
            .find(|t| t.id == task.id)
            .unwrap();
        assert_eq!(failed.error_code.as_deref(), Some("auth/access-denied"));

        h.engine.retry(&task.id).await.unwrap();
        eventually(|| h.runner.started.load(Ordering::SeqCst) == 2).await;
        let retried = h
            .engine
            .snapshot()
            .await
            .into_iter()
            .find(|t| t.id == task.id)
            .unwrap();
        assert_eq!(
            retried.error_code, None,
            "a retried task must not keep showing the previous failure"
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
        eventually(|| h.runner.started.load(Ordering::SeqCst) == 1).await;
        h.runner.finish.store(true, Ordering::SeqCst);
        eventually(|| h.sink.statuses_of(&done.id).last() == Some(&TransferStatus::Completed))
            .await;
        // The finish flag is sticky, unlike `Notify`'s wake-only-current-waiters
        // semantics: clear it so "b" actually parks instead of completing
        // instantly and being swept away by `clear_finished`.
        h.runner.finish.store(false, Ordering::SeqCst);

        let queued = h.engine.enqueue(spec("b")).await.unwrap();
        eventually(|| h.runner.started.load(Ordering::SeqCst) == 2).await;

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
