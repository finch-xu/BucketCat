//! The upload runner: single-stream below the multipart threshold, otherwise
//! `create → N concurrent parts → complete`, with per-part retry.
//!
//! ## Who decides that an upload happened
//!
//! [`TransferRunner::run`] returns a [`RunOutcome`] and the engine believes
//! it: a `Completed` recorded next to an object the server never assembled is
//! a lie nothing downstream can detect. So [`RunOutcome::Completed`] is
//! returned from exactly two places in this module -- straight after
//! `PutObject` returned `Ok`, and straight after `CompleteMultipartUpload`
//! returned `Ok`. Every other way out is an error or
//! [`RunOutcome::Stopped`]. The reverse mistake is cheap by comparison: a
//! bogus `Stopped` is caught by the engine, which fails the task with an
//! `internal` code instead of stranding it.
//!
//! ## Cancel and pause differ in what they leave on the server
//!
//! Both unwind the same way and both report `Stopped`; the difference is the
//! residue. A **cancel** aborts the multipart upload so the accumulated
//! fragments stop costing the user storage, and clears the resume slot --
//! there is nothing left to continue. A **pause** deliberately does neither:
//! the `upload_id` and the accepted parts stay in the resume slot so the next
//! run picks up where this one stopped. A single-stream upload needs no
//! cleanup either way -- `PutObject` writes the whole object or nothing.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::provider::{BodyProgress, BodyProgressFn, Provider, UploadedPart};
use crate::transfer::engine::{
    CheckpointWriter, MultipartState, ResumeState, RunOutcome, StopKind, TaskContext,
    TransferRunner,
};
use crate::transfer::model::TaskNotice;
use crate::transfer::part::{plan_upload_with, PartSpec, TransferTuning, UploadPlan};
use crate::transfer::retry::{backoff_delay_for, is_retryable, MAX_RETRIES};

/// Reports transferred bytes.
///
/// A closure rather than a [`crate::transfer::ProgressHandle`] because that
/// type is only constructible inside `transfer::engine`; going through a
/// closure is what lets the code below -- where a wrong answer means claiming
/// an upload that did not happen -- be tested at all.
type ProgressFn = Arc<dyn Fn(u64) + Send + Sync>;

/// Reads the task's stop intent. A closure for the same reason as
/// [`ProgressFn`]: [`crate::transfer::TaskControl`] has no public constructor.
type StopFn = Arc<dyn Fn() -> Option<StopKind> + Send + Sync>;

/// Surfaces a transient in-flight retry notice (Task 7). Same rationale as
/// [`ProgressFn`]: [`TaskContext::notice`] is the only constructor.
type NoticeFn = Arc<dyn Fn(Option<TaskNotice>) + Send + Sync>;

/// Parts of `plan` the server has not accepted yet.
///
/// Pure so resume arithmetic is testable without a network: given the plan
/// and whatever the last run managed to finish, this is exactly the work
/// left. Completions naming a part outside the plan are ignored rather than
/// trusted -- a stale resume state must never shrink the real work.
pub fn pending_parts(plan: &[PartSpec], done: &[UploadedPart]) -> Vec<PartSpec> {
    let finished: HashSet<i32> = done.iter().map(|p| p.number).collect();
    plan.iter()
        .filter(|spec| !finished.contains(&spec.number))
        .copied()
        .collect()
}

/// The source file's fingerprint -- `(size, mtime as unix milliseconds)` --
/// captured when a multipart upload is created. A cross-restart resume
/// (M4c) will compare this against the file's current fingerprint before
/// trusting a saved resume state, so a `stat` that fails must not fail the
/// upload itself: it degrades to `(0, 0)`, the same as a fingerprint that
/// was simply never captured.
fn source_fingerprint(path: &std::path::Path) -> (u64, i64) {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            (meta.len(), mtime)
        }
        Err(_) => (0, 0),
    }
}

/// The slice of a [`TaskContext`] an upload actually uses.
struct UploadJob {
    task_id: String,
    bucket: String,
    key: String,
    path: PathBuf,
    part_limit: usize,
    /// The tuning snapshot the task was admitted under ([`TaskContext::tuning`],
    /// Task 5). Read once, at `from_context`, not re-read mid-transfer -- a
    /// settings change must not reshape a plan this run has already
    /// committed to.
    tuning: TransferTuning,
    token: CancellationToken,
    stop: StopFn,
    progress: ProgressFn,
    /// Rolls back bytes a failed or abandoned provider-call attempt already
    /// reported, before the retry loop decides to retry or fail (Task 3).
    /// Wired to [`crate::transfer::ProgressHandle::retract`] exactly as
    /// `progress` is wired to `add` -- mirrors `DownloadJob.regress`.
    regress: ProgressFn,
    /// Reports a transient in-flight retry notice (Task 7). Cloned straight
    /// from [`TaskContext::notice`]; every `with_retry` call site gets its own
    /// clone (or a further clone into a spawned part task).
    notice: NoticeFn,
    resume: Arc<Mutex<Option<ResumeState>>>,
    /// Mirrors the in-memory resume slot to a checkpoint file (M4c). `None`
    /// when checkpointing is disabled, in which case `persist_checkpoint` is a
    /// no-op.
    checkpoint: Option<CheckpointWriter>,
}

impl UploadJob {
    fn from_context(ctx: &TaskContext) -> Self {
        let control = ctx.control.clone();
        let progress = ctx.progress.clone();
        let regress_handle = ctx.progress.clone();
        Self {
            task_id: ctx.task.id.clone(),
            bucket: ctx.task.bucket.clone(),
            key: ctx.task.key.clone(),
            path: PathBuf::from(&ctx.task.local_path),
            part_limit: ctx.part_limit,
            tuning: ctx.tuning,
            token: ctx.control.token(),
            // `ctx.task` is a snapshot taken before the task went `Running`,
            // so its `status` is stale; the control is the only live source of
            // stop intent.
            stop: Arc::new(move || control.requested()),
            progress: Arc::new(move |bytes| progress.add(bytes)),
            regress: Arc::new(move |bytes| regress_handle.retract(bytes)),
            notice: Arc::clone(&ctx.notice),
            resume: Arc::clone(&ctx.resume),
            checkpoint: ctx.checkpoint.clone(),
        }
    }

    fn stopped(&self) -> Option<StopKind> {
        (*self.stop)()
    }

    fn report(&self, bytes: u64) {
        (*self.progress)(bytes);
    }

    fn regress(&self, bytes: u64) {
        (*self.regress)(bytes);
    }

    /// Mirrors `state` to the checkpoint file (coalesced/gated/best-effort);
    /// a no-op when checkpointing is disabled. See [`CheckpointWriter`].
    fn persist_checkpoint(&self, state: &ResumeState, force: bool) {
        if let Some(writer) = &self.checkpoint {
            writer.persist(state, force);
        }
    }
}

/// The immutable half of a part upload -- everything all parts of one task
/// share. Behind an `Arc` so spawning a part costs one refcount bump instead
/// of four `String`/`PathBuf` clones.
struct PartTarget {
    bucket: String,
    key: String,
    upload_id: String,
    path: PathBuf,
}

pub struct UploadRunner;

#[async_trait]
impl TransferRunner for UploadRunner {
    async fn run(&self, ctx: TaskContext) -> AppResult<RunOutcome> {
        let provider = ctx.hub.provider(&ctx.task.connection_id).await?;
        run_upload(&UploadJob::from_context(&ctx), provider, ctx.task.total).await
    }
}

/// Uploads `total` bytes of `job.path` to `job.bucket`/`job.key`.
///
/// Generic over the provider so the decision points below can be exercised
/// against a fake; production always passes the hub's `S3Provider`.
async fn run_upload<P>(job: &UploadJob, provider: Arc<P>, total: u64) -> AppResult<RunOutcome>
where
    P: Provider + Send + Sync + 'static,
{
    // Checked before any round trip: a stop that is already pending on a fresh
    // upload would otherwise make the multipart path create an upload purely to
    // abort it again a moment later. This is never a bogus `Stopped` -- the
    // intent is present, so the engine has a legal transition to apply.
    //
    // The resume slot must be empty to take this exit. A task paused
    // mid-multipart holds `{upload_id, completed}` here; on resume the engine
    // spawns a fresh driver and a cancel can land in that window, so the stop
    // is already pending when the runner starts. Short-circuiting then would
    // return `Stopped` *without* aborting the recorded upload id -- the engine
    // files `Canceled`, `clear_finished` drops the record, and the server-side
    // fragments bill the user forever. With resume state present we fall
    // through into the multipart path instead: it reuses the recorded
    // `upload_id`, the spawn loop breaks on its first iteration because the stop
    // is already requested, and the cancel branch issues the abort.
    if job.stopped().is_some() && !matches!(&*job.resume.lock().await, Some(ResumeState::Upload(_)))
    {
        return Ok(RunOutcome::Stopped);
    }

    match plan_upload_with(total, &job.tuning) {
        UploadPlan::Single { length } => upload_single(job, provider, length).await,
        UploadPlan::Multipart { parts, part_size } => {
            tracing::info!(
                task = %job.task_id,
                parts = parts.len(),
                part_size,
                "uploading as multipart"
            );
            upload_multipart(job, provider, parts, part_size).await
        }
    }
}

/// Builds the [`BodyProgressFn`] for one provider-call attempt: `Sent(n)`
/// (the transport pulled `n` more bytes from the request body) adds to both
/// `attempt_reported` and the live progress bar; `Rewound(n)` (the SDK
/// rebuilding the body for its own internal retry) undoes both. See
/// `upload_single`'s doc comment on `attempt_reported` for the accounting
/// scheme this is one piece of.
fn attempt_body_progress(
    attempt_reported: &Arc<AtomicU64>,
    progress: &ProgressFn,
    regress: &ProgressFn,
) -> BodyProgressFn {
    let attempt_reported = Arc::clone(attempt_reported);
    let progress = Arc::clone(progress);
    let regress = Arc::clone(regress);
    Arc::new(move |event| match event {
        BodyProgress::Sent(n) => {
            attempt_reported.fetch_add(n, Ordering::SeqCst);
            progress(n);
        }
        BodyProgress::Rewound(n) => {
            let _ = attempt_reported.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                Some(cur.saturating_sub(n))
            });
            regress(n);
        }
    })
}

async fn upload_single<P>(job: &UploadJob, provider: Arc<P>, length: u64) -> AppResult<RunOutcome>
where
    P: Provider + Send + Sync + 'static,
{
    tracing::info!(task = %job.task_id, bytes = length, "uploading as a single stream");

    // Bytes this call's request body has reported `Sent` to the progress bar
    // but the server has not (yet, or ever) confirmed by returning `Ok`.
    // Declared here, outside the `with_retry` closure, because `with_retry`'s
    // signature stays generic over `T` and gives the caller no hook into a
    // cancelled attempt -- this is the one channel by which a mid-flight
    // `Ok(None)` (the token fired while `PutObject` was still in flight,
    // dropping the future before it resolved) can still be retracted after
    // the fact.
    //
    // Accounting scheme (mirrored in `upload_one_part`):
    // - Reset to 0 at the top of every attempt: a defensive no-op in the
    //   normal case (the previous attempt's `Err` path below already cleaned
    //   up before returning), but it means a bug in that cleanup shows up as
    //   under-reporting, never as a permanent phantom balance.
    // - `Sent(n)` adds to both this counter and the live progress bar;
    //   `Rewound(n)` (an SDK-internal retry rebuilding the body) undoes both.
    // - `Err` retracts and zeroes the counter before returning, so a retried
    //   attempt starts from a clean slate.
    // - `Ok` leaves the counter holding this attempt's net total so the code
    //   after `with_retry` can true it up against `length`.
    // - `with_retry` returning `Ok(None)` (cancelled) retracts whatever the
    //   aborted attempt left behind.
    //
    // Invariant: every byte ever added here is either (a) still held by a
    // live counter that will be trued-up or retracted, or (b) has already
    // been retracted -- never both, never neither.
    let attempt_reported = Arc::new(AtomicU64::new(0));

    let put = with_retry(&job.token, &job.notice, || {
        let provider = Arc::clone(&provider);
        let bucket = job.bucket.clone();
        let key = job.key.clone();
        let path = job.path.clone();
        let attempt_reported = Arc::clone(&attempt_reported);
        let progress = Arc::clone(&job.progress);
        let regress = Arc::clone(&job.regress);
        async move {
            let leftover = attempt_reported.swap(0, Ordering::SeqCst);
            if leftover > 0 {
                regress(leftover);
            }
            let body_progress = attempt_body_progress(&attempt_reported, &progress, &regress);
            let result = provider
                .put_object_from_file(&bucket, &key, &path, length, body_progress)
                .await;
            if result.is_err() {
                let counted = attempt_reported.swap(0, Ordering::SeqCst);
                if counted > 0 {
                    regress(counted);
                }
            }
            result
        }
    })
    .await?;

    if put.is_none() {
        // The token fired, so `PutObject` was abandoned rather than observed
        // succeeding: neither the bytes nor the completion may be claimed. No
        // cleanup is owed -- a single `PutObject` writes the whole object or
        // nothing, and there is no multipart state to abort. Whatever this
        // aborted attempt had already reported must still come back off the
        // bar, though.
        let counted = attempt_reported.swap(0, Ordering::SeqCst);
        if counted > 0 {
            job.regress(counted);
        }
        return Ok(RunOutcome::Stopped);
    }

    // Truthful only here: the request returned `Ok`, so the object exists even
    // if a stop was requested while it was in flight. The incremental `Sent`
    // notches already moved the bar close to `length`; true up whatever
    // coalescing (`PROGRESS_QUANTUM`) left short.
    let counted = attempt_reported.load(Ordering::SeqCst);
    debug_assert!(
        counted <= length,
        "a successful upload attempt must never report more than it sent"
    );
    job.report(length - counted);
    Ok(RunOutcome::Completed)
}

async fn upload_multipart<P>(
    job: &UploadJob,
    provider: Arc<P>,
    plan: Vec<PartSpec>,
    part_size: u64,
) -> AppResult<RunOutcome>
where
    P: Provider + Send + Sync + 'static,
{
    // A `Download` variant here is impossible for an upload runner; treat it
    // (like an empty slot) as "no upload resume state" rather than panicking.
    let existing = match job.resume.lock().await.clone() {
        Some(ResumeState::Upload(ms)) => ms,
        _ => MultipartState::default(),
    };

    let planned: HashSet<i32> = plan.iter().map(|spec| spec.number).collect();

    // Decide whether this run continues an existing upload or starts a fresh
    // one, and -- crucially -- what the *server* considers already done. A
    // saved resume state is only as trustworthy as two things it cannot vouch
    // for on its own: that the source file is still the one the upload was
    // created for, and that the parts its checkpoint claims were accepted
    // really reached the server. Both are verified here, before any part is
    // (re-)sent, so a cross-restart resume assembles the right object instead
    // of a plausible-looking corrupt one.
    //
    // `resume_id` is `Some` only when the upload can genuinely be continued;
    // otherwise it is `None` and a brand-new `multipart_init` follows below.
    // `carried` is the *server-authoritative* set of already-accepted parts
    // (empty for a fresh upload), never the checkpoint's own `completed` list.
    let (resume_id, carried, source_size, source_mtime) = if existing.upload_id.is_empty() {
        // A fresh upload: capture the fingerprint of the file as it is now, so
        // a later resume can tell whether it changed underneath us.
        let (size, mtime) = source_fingerprint(&job.path);
        (None, Vec::new(), size, mtime)
    } else {
        let (cur_size, cur_mtime) = source_fingerprint(&job.path);
        if (cur_size, cur_mtime) != (existing.source_size, existing.source_mtime)
            || existing.part_size != part_size
        {
            // Either the bytes on disk are not the bytes this upload was
            // created for, or the recorded part size no longer matches the
            // current plan (chunking is user-configurable -- M6+ -- so a
            // checkpoint cut under a different tuning cannot be trusted: its
            // `completed` part numbers would map to different byte ranges
            // under this plan). Continuing either way would splice mismatched
            // parts onto old ones and assemble a corrupt object, so abort the
            // stale upload (best-effort -- its fragments would otherwise keep
            // billing the user), forget it, and fall through to a fresh
            // upload of the current file.
            if let Err(err) = provider
                .multipart_abort(&job.bucket, &job.key, &existing.upload_id)
                .await
            {
                tracing::warn!(task = %job.task_id, "abort of stale upload failed: {err}");
            }
            *job.resume.lock().await = None;
            (None, Vec::new(), cur_size, cur_mtime)
        } else {
            // The source matches. The checkpoint's `completed` can still be
            // wrong (a write interrupted mid-flush, a truncated disk), so ask
            // the server what it actually holds and treat *that* as the set of
            // finished parts. A part it reports outside this plan is dropped:
            // `CompleteMultipartUpload` rejects a number the upload never
            // issued, and a stale entry must never shrink the real work.
            match provider
                .multipart_list(&job.bucket, &job.key, &existing.upload_id)
                .await
            {
                Ok(server_parts) => {
                    let carried: Vec<UploadedPart> = server_parts
                        .into_iter()
                        .filter(|part| planned.contains(&part.number))
                        .collect();
                    (
                        Some(existing.upload_id.clone()),
                        carried,
                        existing.source_size,
                        existing.source_mtime,
                    )
                }
                Err(err) => {
                    // The id is gone or unreachable, so it cannot be continued:
                    // sending parts under it would only fail at complete time.
                    // Forget it and start fresh against the current file.
                    tracing::warn!(task = %job.task_id, "listing parts to resume failed: {err}");
                    *job.resume.lock().await = None;
                    (None, Vec::new(), cur_size, cur_mtime)
                }
            }
        }
    };

    let upload_id = match resume_id {
        Some(id) => id,
        None => {
            let id = provider.multipart_init(&job.bucket, &job.key).await?;
            // Recorded immediately: if everything below fails, a cancel still
            // has an upload id to abort with, so no server-side fragments leak.
            let init_state = ResumeState::Upload(MultipartState {
                upload_id: id.clone(),
                completed: carried.clone(),
                source_size,
                source_mtime,
                part_size,
            });
            *job.resume.lock().await = Some(init_state.clone());
            // The first set, before any part lands: `force` so a cancel-before-
            // any-part can read the upload id it must abort even after a restart.
            job.persist_checkpoint(&init_state, true);
            id
        }
    };

    // The engine zeroes `transferred` and sends `Forget` whenever a task is
    // resumed or retried, so a continued run has to re-report the work earlier
    // runs already put on the server -- otherwise the bar under-reports for
    // the rest of the transfer.
    for part in &carried {
        job.report(part.size);
    }

    let pending = pending_parts(&plan, &carried);
    let completed = Mutex::new(carried);
    let target = Arc::new(PartTarget {
        bucket: job.bucket.clone(),
        key: job.key.clone(),
        upload_id: upload_id.clone(),
        path: job.path.clone(),
    });

    let permits = Arc::new(Semaphore::new(job.part_limit.max(1)));
    // A child token, so that stopping the remaining parts after one of them
    // failed for good is distinguishable from a user-requested stop: a part
    // that gave up because a sibling died must never be read as a pause.
    let part_token = job.token.child_token();
    let mut set: JoinSet<AppResult<Option<UploadedPart>>> = JoinSet::new();
    let mut first_error: Option<AppError> = None;

    for spec in pending {
        // Acquiring before spawning is what bounds in-flight parts: the loop
        // blocks here until an earlier part releases its permit, instead of
        // spawning the whole plan at once.
        let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
            break;
        };
        // Fold in whatever finished while we waited. Draining here, and not
        // only after the loop, is what makes one part's permanent failure
        // stop the others: with 1000 parts and 4 permits, a drain placed after
        // the spawn loop would not look at a single result until all 1000 had
        // already been sent to the server.
        while let Some(joined) = set.try_join_next() {
            absorb_part(joined, &completed, &mut first_error, &part_token).await;
        }
        if part_token.is_cancelled() {
            break;
        }

        let provider = Arc::clone(&provider);
        let target = Arc::clone(&target);
        let token = part_token.clone();
        let progress = Arc::clone(&job.progress);
        let regress = Arc::clone(&job.regress);
        let notice = Arc::clone(&job.notice);

        set.spawn(async move {
            let _permit = permit;
            upload_one_part(provider, target, spec, token, progress, regress, notice).await
        });
    }

    while let Some(joined) = set.join_next().await {
        absorb_part(joined, &completed, &mut first_error, &part_token).await;
    }

    // Persist what landed before deciding anything, so a pause, a permanent
    // failure and a later retry all resume from the same place.
    let done = completed.lock().await.clone();
    let landed_state = ResumeState::Upload(MultipartState {
        upload_id: upload_id.clone(),
        completed: done.clone(),
        source_size,
        source_mtime,
        part_size,
    });
    *job.resume.lock().await = Some(landed_state.clone());
    // A per-part checkpoint refresh: coalesced, so a task that finishes many
    // parts in a burst writes a bounded number of files rather than one each.
    job.persist_checkpoint(&landed_state, false);

    // Read the stop intent once. Anything arriving after this line arrived
    // during the commit, and the contract is explicit that a real completion
    // wins that race.
    let stop = job.stopped();

    if stop == Some(StopKind::Cancel) {
        // Deliberately ahead of `first_error`. An error makes the engine file
        // `Failed`, and cancelling a `Failed` task never reaches a runner
        // again -- so if the error won here, this abort would be the cleanup
        // nobody ever performs and the fragments would bill the user forever.
        // The error is worth less than the cleanup: the user asked for this
        // upload to go away.
        //
        // Best effort: a failed abort must not turn the user's cancellation
        // into a failed task, but it does belong in the log -- the fragments
        // keep costing storage until the bucket's lifecycle rules reap them.
        if let Err(err) = provider
            .multipart_abort(&job.bucket, &job.key, &upload_id)
            .await
        {
            tracing::warn!(task = %job.task_id, "abort of cancelled upload failed: {err}");
        }
        *job.resume.lock().await = None;
        return Ok(RunOutcome::Stopped);
    }

    if let Some(err) = first_error {
        return Err(err);
    }

    if stop.is_some() {
        // A pause. Nothing is aborted and the resume slot keeps the upload id
        // plus every accepted part, so the next run continues instead of
        // re-uploading from zero. `force` the checkpoint so the pause point is
        // current on disk even if the per-part write above was coalesced away.
        job.persist_checkpoint(&landed_state, true);
        return Ok(RunOutcome::Stopped);
    }

    if done.len() != plan.len() {
        // Nothing above explains a short list, and `CompleteMultipartUpload`
        // would happily assemble one into a truncated object the user cannot
        // tell from a good one. Fail loudly rather than silently corrupt.
        return Err(AppError::Internal {
            message: format!(
                "upload of {} finished with {} of {} parts and no error to explain the gap",
                job.key,
                done.len(),
                plan.len()
            ),
        });
    }

    provider
        .multipart_complete(&job.bucket, &job.key, &upload_id, &done)
        .await?;
    // The only truthful place to claim success on this path: the server has
    // assembled the object, so the bytes are there even if a stop landed while
    // the request was in flight.
    *job.resume.lock().await = None;
    Ok(RunOutcome::Completed)
}

/// Folds one finished part into the accepted list or into `first_error`.
///
/// The first error also cancels `part_token`, which is what stops the parts
/// that have not started yet.
async fn absorb_part(
    joined: Result<AppResult<Option<UploadedPart>>, tokio::task::JoinError>,
    completed: &Mutex<Vec<UploadedPart>>,
    first_error: &mut Option<AppError>,
    part_token: &CancellationToken,
) {
    match joined {
        Ok(Ok(Some(part))) => completed.lock().await.push(part),
        // `None` = the part observed cancellation and bailed cleanly.
        Ok(Ok(None)) => {}
        Ok(Err(err)) => {
            if first_error.is_none() {
                *first_error = Some(err);
                part_token.cancel();
            }
        }
        Err(join_err) => {
            if first_error.is_none() {
                *first_error = Some(AppError::Internal {
                    message: format!("upload part task panicked: {join_err}"),
                });
                part_token.cancel();
            }
        }
    }
}

async fn upload_one_part<P>(
    provider: Arc<P>,
    target: Arc<PartTarget>,
    spec: PartSpec,
    token: CancellationToken,
    progress: ProgressFn,
    regress: ProgressFn,
    notice: NoticeFn,
) -> AppResult<Option<UploadedPart>>
where
    P: Provider + Send + Sync + 'static,
{
    // See `upload_single`'s doc comment on `attempt_reported` for the
    // accounting scheme; this is the per-part mirror of it.
    let attempt_reported = Arc::new(AtomicU64::new(0));

    let etag = with_retry(&token, &notice, || {
        let provider = Arc::clone(&provider);
        let target = Arc::clone(&target);
        let attempt_reported = Arc::clone(&attempt_reported);
        let progress = Arc::clone(&progress);
        let regress = Arc::clone(&regress);
        async move {
            let leftover = attempt_reported.swap(0, Ordering::SeqCst);
            if leftover > 0 {
                regress(leftover);
            }
            let body_progress = attempt_body_progress(&attempt_reported, &progress, &regress);
            let result = provider
                .upload_part_from_file(
                    &target.bucket,
                    &target.key,
                    &target.upload_id,
                    spec.number,
                    &target.path,
                    spec.offset,
                    spec.length,
                    body_progress,
                )
                .await;
            if result.is_err() {
                let counted = attempt_reported.swap(0, Ordering::SeqCst);
                if counted > 0 {
                    regress(counted);
                }
            }
            result
        }
    })
    .await?;

    let Some(etag) = etag else {
        // Cancelled mid-flight: whatever this aborted attempt already
        // reported must come back off the bar. Only this part's in-flight
        // count is affected -- sibling parts that already finished keep
        // their reports.
        let counted = attempt_reported.swap(0, Ordering::SeqCst);
        if counted > 0 {
            regress(counted);
        }
        return Ok(None);
    };

    // Incremental `Sent` notches already moved the bar close to
    // `spec.length`; true up whatever coalescing left short.
    let counted = attempt_reported.load(Ordering::SeqCst);
    debug_assert!(
        counted <= spec.length,
        "a successful part upload must never report more than it sent"
    );
    progress(spec.length - counted);
    Ok(Some(UploadedPart {
        number: spec.number,
        etag,
        size: spec.length,
    }))
}

/// Runs `op` with the retry policy from `transfer::retry`, giving up early if
/// `token` fires. `Ok(None)` means "stopped, not failed" -- the caller must
/// not treat it as success *or* as an error.
///
/// `notice` surfaces each retry (Task 7, design §7.4): a retryable failure
/// emits `Some(TaskNotice { .. })` before the backoff sleep, and -- only if a
/// notice was actually emitted earlier in this call -- a subsequent success
/// emits `None` to clear it. The `notified` guard keeps a step that never
/// failed from ever touching `set_notice`, which would otherwise fire on
/// every single successful part/put with nothing to say.
async fn with_retry<T, F, Fut>(
    token: &CancellationToken,
    notice: &NoticeFn,
    mut op: F,
) -> AppResult<Option<T>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = AppResult<T>>,
{
    let mut retries = 0u32;
    let mut notified = false;
    loop {
        if token.is_cancelled() {
            return Ok(None);
        }

        let outcome = tokio::select! {
            biased;
            _ = token.cancelled() => return Ok(None),
            result = op() => result,
        };

        match outcome {
            Ok(value) => {
                if notified {
                    (*notice)(None);
                }
                return Ok(Some(value));
            }
            Err(err) => {
                retries += 1;
                if !is_retryable(&err) || retries > MAX_RETRIES {
                    return Err(err);
                }
                (*notice)(Some(TaskNotice {
                    code: err.code().to_string(),
                    attempt: retries,
                    max: MAX_RETRIES,
                }));
                notified = true;
                let delay = backoff_delay_for(&err, retries);
                tracing::warn!(retry = retries, ?delay, "retrying transfer step: {err}");
                tokio::select! {
                    _ = token.cancelled() => return Ok(None),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use tokio::sync::Notify;

    use crate::provider::{BatchResult, BodyProgressFn, Bucket, ListPage, ObjectHead};

    const UPLOAD_ID: &str = "u-1";
    const MB: u64 = 1024 * 1024;

    /// The default (balanced) preset's upload threshold -- 32MB. Multipart-
    /// trigger sizes below are expressed relative to this and `part_floor()`
    /// rather than hardcoded, so a future preset change cannot leave a
    /// silently-wrong test size behind.
    fn threshold() -> u64 {
        TransferTuning::default().upload_threshold
    }

    /// The balanced preset's upload part floor -- 16MB, and (not
    /// coincidentally) exactly `threshold() / 2`: a total of `threshold()`
    /// resolves to two parts of this size.
    fn part_floor() -> u64 {
        TransferTuning::default().upload_part_floor
    }

    fn spec(number: i32) -> PartSpec {
        PartSpec {
            number,
            offset: (number as u64 - 1) * 8,
            length: 8,
        }
    }

    fn done(number: i32) -> UploadedPart {
        UploadedPart {
            number,
            etag: format!("\"e{number}\""),
            size: 8,
        }
    }

    #[test]
    fn nothing_done_means_everything_is_pending() {
        let plan = [spec(1), spec(2), spec(3)];
        assert_eq!(pending_parts(&plan, &[]), plan.to_vec());
    }

    #[test]
    fn completed_parts_are_skipped_on_resume() {
        let plan = [spec(1), spec(2), spec(3), spec(4)];
        let pending = pending_parts(&plan, &[done(1), done(3)]);
        assert_eq!(
            pending.iter().map(|p| p.number).collect::<Vec<_>>(),
            vec![2, 4]
        );
    }

    #[test]
    fn out_of_order_completions_are_handled() {
        // Parts finish in whatever order the network delivers them.
        let plan = [spec(1), spec(2), spec(3)];
        let pending = pending_parts(&plan, &[done(3), done(1)]);
        assert_eq!(
            pending.iter().map(|p| p.number).collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn a_fully_completed_plan_leaves_nothing_pending() {
        let plan = [spec(1), spec(2)];
        assert!(pending_parts(&plan, &[done(1), done(2)]).is_empty());
    }

    #[test]
    fn stale_completions_outside_the_plan_are_ignored() {
        // Defensive: a resume state naming a part the current plan does not
        // contain must not remove a real part or panic.
        let plan = [spec(1), spec(2)];
        let pending = pending_parts(&plan, &[done(99)]);
        assert_eq!(
            pending.iter().map(|p| p.number).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    // ---- The runner itself -------------------------------------------------
    //
    // A wrong `RunOutcome` is the one failure mode nothing downstream can
    // detect (the engine files `Completed` for an object that may not exist),
    // so the outcome of every path below is asserted directly rather than
    // inferred from the engine's behaviour.

    /// Which failure a fake part upload hands back. `AppError` is not `Clone`,
    /// so tests describe the failure and the fake builds a fresh one per
    /// attempt.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Fail {
        /// `network/*` -- `is_retryable` says yes.
        Transient,
        /// `auth/*` -- retrying cannot help, so the task must fail.
        Permanent,
    }

    impl Fail {
        fn error(self) -> AppError {
            match self {
                Fail::Transient => AppError::Timeout,
                Fail::Permanent => AppError::AccessDenied,
            }
        }
    }

    /// A one-shot rendezvous a scripted upload call parks on after applying a
    /// triggering `Sent` notch, so a test can observe progress strictly
    /// between two notches -- or fire a cancel while the call is genuinely
    /// still in flight -- instead of racing a real concurrent write against
    /// the runner.
    ///
    /// `pass` is called from inside the fake: it signals `reached` (so a test
    /// already waiting on it wakes up) and then blocks on `release`. A test
    /// either calls `open()` to let the call continue, or -- for a cancel
    /// test -- never does, and instead fires the stop switch; the runner's
    /// own `with_retry` select then drops the parked call's future outright,
    /// which is exactly the "abandoned mid-flight" case under test.
    #[derive(Default)]
    struct Gate {
        reached: Notify,
        release: Notify,
    }

    impl Gate {
        async fn pass(&self) {
            self.reached.notify_one();
            self.release.notified().await;
        }

        /// Test-side: block until a scripted call reaches its pause point.
        async fn wait(&self) {
            self.reached.notified().await;
        }

        /// Test-side: let a parked call continue.
        fn open(&self) {
            self.release.notify_one();
        }
    }

    /// Identifies exactly which scripted part call should park on
    /// `FakeProvider::part_gate`: the `attempt`'th call (1-based, retries
    /// included) to part `part`, right after it has applied the notch at
    /// index `after` (0-based).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct PartPause {
        part: i32,
        attempt: u32,
        after: usize,
    }

    /// A stand-in for [`crate::transfer::TaskControl`], whose constructor is
    /// private to the engine. Writes the intent *before* firing the token,
    /// exactly as the real one does, so a reader woken by the token is always
    /// able to see why.
    struct Switch {
        kind: StdMutex<Option<StopKind>>,
        token: CancellationToken,
    }

    impl Switch {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                kind: StdMutex::new(None),
                token: CancellationToken::new(),
            })
        }

        fn request(&self, kind: StopKind) {
            *self.kind.lock().unwrap() = Some(kind);
            self.token.cancel();
        }

        fn requested(&self) -> Option<StopKind> {
            *self.kind.lock().unwrap()
        }
    }

    /// Records what the runner asked of the server, and can be told to fail
    /// specific parts or to fire the stop switch mid-run.
    ///
    /// Only the five transfer primitives are implemented: the admin-plane
    /// methods are unreachable from an upload, and a stub that panics says so
    /// more clearly than one that quietly succeeds.
    struct FakeProvider {
        switch: Arc<Switch>,
        puts: AtomicUsize,
        inits: AtomicUsize,
        aborts: AtomicUsize,
        /// Every `upload_part_from_file` call in arrival order, retries
        /// included -- which is what makes the retry budget observable.
        attempts: StdMutex<Vec<i32>>,
        /// The part list `multipart_complete` was called with, if at all.
        assembled: StdMutex<Option<Vec<UploadedPart>>>,
        live: AtomicUsize,
        peak: AtomicUsize,
        /// part number -> (attempts still to fail, how).
        failures: StdMutex<HashMap<i32, (u32, Fail)>>,
        /// Fire the stop switch when this part is reached -- this is how a
        /// user's pause/cancel is made to land in the middle of a real run.
        stop_at_part: StdMutex<Option<(i32, StopKind)>>,
        /// Fire the stop switch from inside `multipart_complete`, i.e. after
        /// the runner has already decided to commit.
        stop_on_complete: StdMutex<Option<StopKind>>,
        /// How long one request takes. Non-zero makes concurrency observable,
        /// and makes a request abandonable while it is still in flight.
        op_delay: Duration,
        abort_fails: bool,
        /// Make `multipart_complete` return an error -- the server refused to
        /// assemble the object. The runner must surface this as `Err`, never
        /// report `Completed` for a commit that did not happen.
        complete_fails: bool,
        /// What `multipart_list` reports the server has accepted. Configurable
        /// so a resume test can make the *server* disagree with the
        /// checkpoint's `completed` -- e.g. return an empty list even though
        /// the checkpoint claims part 1 is done -- which is exactly the case
        /// the server-authoritative reconciliation must get right.
        listed: StdMutex<Vec<UploadedPart>>,
        /// `Sent`-notch sizes `put_object_from_file` emits, in order, before
        /// it resolves -- simulates the transport pulling the single-stream
        /// body incrementally. Empty (the default) emits nothing, matching
        /// every pre-existing single-stream test's expectations.
        put_notches: Vec<u64>,
        /// If set, `put_object_from_file` parks on `put_gate` right after
        /// applying the notch at this index (0-based).
        put_pause_after: Option<usize>,
        put_gate: Arc<Gate>,
        /// part_number -> queue of `Sent`-notch sequences, one popped per
        /// call attempt -- so a retried part can script a different sequence
        /// for its second attempt than its first. A part with no entry (or
        /// an exhausted queue) gets no notches, matching every pre-existing
        /// multipart test's expectations.
        part_notches: StdMutex<HashMap<i32, VecDeque<Vec<u64>>>>,
        /// If set, the matching call to `upload_part_from_file` parks on
        /// `part_gate` right after applying the triggering notch.
        part_pause: Option<PartPause>,
        part_gate: Arc<Gate>,
    }

    impl FakeProvider {
        fn new(switch: Arc<Switch>) -> Self {
            Self {
                switch,
                puts: AtomicUsize::new(0),
                inits: AtomicUsize::new(0),
                aborts: AtomicUsize::new(0),
                attempts: StdMutex::new(Vec::new()),
                assembled: StdMutex::new(None),
                live: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                failures: StdMutex::new(HashMap::new()),
                stop_at_part: StdMutex::new(None),
                stop_on_complete: StdMutex::new(None),
                op_delay: Duration::ZERO,
                abort_fails: false,
                complete_fails: false,
                listed: StdMutex::new(Vec::new()),
                put_notches: Vec::new(),
                put_pause_after: None,
                put_gate: Arc::new(Gate::default()),
                part_notches: StdMutex::new(HashMap::new()),
                part_pause: None,
                part_gate: Arc::new(Gate::default()),
            }
        }

        fn puts(&self) -> usize {
            self.puts.load(Ordering::SeqCst)
        }

        fn inits(&self) -> usize {
            self.inits.load(Ordering::SeqCst)
        }

        fn aborts(&self) -> usize {
            self.aborts.load(Ordering::SeqCst)
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }

        fn attempts_of(&self, part: i32) -> usize {
            self.attempts
                .lock()
                .unwrap()
                .iter()
                .filter(|number| **number == part)
                .count()
        }

        /// Distinct parts the runner sent, ascending.
        fn parts_seen(&self) -> Vec<i32> {
            let mut seen = self.attempts.lock().unwrap().clone();
            seen.sort_unstable();
            seen.dedup();
            seen
        }

        /// Part numbers handed to `multipart_complete`, ascending. The real
        /// provider sorts before sending, so the runner is free not to.
        fn assembled_numbers(&self) -> Vec<i32> {
            let mut numbers: Vec<i32> = self
                .assembled
                .lock()
                .unwrap()
                .as_ref()
                .expect("multipart_complete was never called")
                .iter()
                .map(|part| part.number)
                .collect();
            numbers.sort_unstable();
            numbers
        }

        fn never_assembled(&self) -> bool {
            self.assembled.lock().unwrap().is_none()
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        async fn test_connection(&self) -> AppResult<()> {
            unimplemented!("an upload never tests the connection")
        }

        async fn list_buckets(&self) -> AppResult<Vec<Bucket>> {
            unimplemented!("an upload never lists buckets")
        }

        async fn create_bucket(&self, _name: &str) -> AppResult<()> {
            unimplemented!("an upload never creates buckets")
        }

        async fn delete_bucket(&self, _name: &str) -> AppResult<()> {
            unimplemented!("an upload never deletes buckets")
        }

        async fn list_objects(
            &self,
            _bucket: &str,
            _prefix: &str,
            _token: Option<&str>,
            _max_keys: i32,
        ) -> AppResult<ListPage> {
            unimplemented!("an upload never lists objects")
        }

        async fn delete_objects(&self, _bucket: &str, _keys: &[String]) -> AppResult<BatchResult> {
            unimplemented!("an upload never deletes objects")
        }

        async fn delete_prefix(&self, _bucket: &str, _prefix: &str) -> AppResult<BatchResult> {
            unimplemented!("an upload never deletes prefixes")
        }

        async fn rename_object(
            &self,
            _bucket: &str,
            _from_key: &str,
            _to_key: &str,
        ) -> AppResult<()> {
            unimplemented!("an upload never renames objects")
        }

        async fn create_folder(&self, _bucket: &str, _prefix: &str) -> AppResult<()> {
            unimplemented!("an upload never creates folders")
        }

        async fn put_object_from_file(
            &self,
            _bucket: &str,
            _key: &str,
            _path: &Path,
            _length: u64,
            progress: BodyProgressFn,
        ) -> AppResult<()> {
            if !self.op_delay.is_zero() {
                tokio::time::sleep(self.op_delay).await;
            }
            for (i, n) in self.put_notches.iter().enumerate() {
                progress(BodyProgress::Sent(*n));
                if self.put_pause_after == Some(i) {
                    self.put_gate.pass().await;
                }
            }
            // Counted *after* the delay, so `puts()` means "puts the server
            // accepted" rather than "puts that were started and possibly
            // dropped half way".
            self.puts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn multipart_init(&self, _bucket: &str, _key: &str) -> AppResult<String> {
            self.inits.fetch_add(1, Ordering::SeqCst);
            Ok(UPLOAD_ID.to_string())
        }

        #[allow(clippy::too_many_arguments)]
        async fn upload_part_from_file(
            &self,
            _bucket: &str,
            _key: &str,
            upload_id: &str,
            part_number: i32,
            _path: &Path,
            _offset: u64,
            _length: u64,
            progress: BodyProgressFn,
        ) -> AppResult<String> {
            assert_eq!(
                upload_id, UPLOAD_ID,
                "parts must be sent under the id multipart_init returned"
            );
            self.attempts.lock().unwrap().push(part_number);
            // 1-based: the count above just recorded this call.
            let attempt_no = self.attempts_of(part_number) as u32;

            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(live, Ordering::SeqCst);
            if !self.op_delay.is_zero() {
                tokio::time::sleep(self.op_delay).await;
            }
            self.live.fetch_sub(1, Ordering::SeqCst);

            let notches = self
                .part_notches
                .lock()
                .unwrap()
                .get_mut(&part_number)
                .and_then(|queue| queue.pop_front())
                .unwrap_or_default();
            for (i, n) in notches.into_iter().enumerate() {
                progress(BodyProgress::Sent(n));
                let pause = self.part_pause;
                if pause
                    == Some(PartPause {
                        part: part_number,
                        attempt: attempt_no,
                        after: i,
                    })
                {
                    self.part_gate.pass().await;
                }
            }

            let stop_at = *self.stop_at_part.lock().unwrap();
            if let Some((number, kind)) = stop_at {
                if number == part_number {
                    self.switch.request(kind);
                }
            }

            let failure = {
                let mut failures = self.failures.lock().unwrap();
                match failures.get_mut(&part_number) {
                    Some((remaining, how)) if *remaining > 0 => {
                        *remaining -= 1;
                        Some(*how)
                    }
                    _ => None,
                }
            };
            if let Some(how) = failure {
                return Err(how.error());
            }

            Ok(format!("\"etag-{part_number}\""))
        }

        async fn multipart_complete(
            &self,
            _bucket: &str,
            _key: &str,
            upload_id: &str,
            parts: &[UploadedPart],
        ) -> AppResult<()> {
            assert_eq!(upload_id, UPLOAD_ID);
            *self.assembled.lock().unwrap() = Some(parts.to_vec());
            if self.complete_fails {
                return Err(AppError::Unreachable);
            }
            let late = *self.stop_on_complete.lock().unwrap();
            if let Some(kind) = late {
                self.switch.request(kind);
            }
            Ok(())
        }

        async fn multipart_abort(
            &self,
            _bucket: &str,
            _key: &str,
            upload_id: &str,
        ) -> AppResult<()> {
            assert_eq!(upload_id, UPLOAD_ID);
            self.aborts.fetch_add(1, Ordering::SeqCst);
            if self.abort_fails {
                return Err(AppError::Unreachable);
            }
            Ok(())
        }

        // The server's own record of accepted parts -- authoritative on a
        // cross-restart resume. Defaults to empty (the server accepted
        // nothing), which a test overrides via the `listed` field to model a
        // server that has some parts already.
        async fn multipart_list(
            &self,
            _bucket: &str,
            _key: &str,
            _upload_id: &str,
        ) -> AppResult<Vec<UploadedPart>> {
            Ok(self.listed.lock().unwrap().clone())
        }

        async fn head_object(&self, _bucket: &str, _key: &str) -> AppResult<ObjectHead> {
            unimplemented!("an upload never heads objects")
        }

        async fn open_range(
            &self,
            _bucket: &str,
            _key: &str,
            _offset: u64,
            _length: u64,
        ) -> AppResult<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
            unimplemented!("an upload never reads object ranges")
        }

        async fn list_objects_flat(
            &self,
            _bucket: &str,
            _prefix: &str,
            _token: Option<&str>,
            _max_keys: i32,
        ) -> AppResult<ListPage> {
            unimplemented!("an upload never lists objects")
        }

        async fn presign_get(
            &self,
            _bucket: &str,
            _key: &str,
            _expires_secs: u64,
        ) -> AppResult<String> {
            unimplemented!("an upload never presigns a share url")
        }
    }

    struct Rig {
        switch: Arc<Switch>,
        provider: Arc<FakeProvider>,
        resume: Arc<Mutex<Option<ResumeState>>>,
        reported: Arc<AtomicU64>,
        /// Cumulative bytes `regress` has retracted -- gross, like `reported`,
        /// so `reported() - retracted()` is the net figure the real progress
        /// bar would show (Task 3, mirrors download.rs's `regressed`).
        retracted: Arc<AtomicU64>,
        part_limit: usize,
        /// Every notice `with_retry` emitted, in arrival order -- `Some` for a
        /// retryable failure, `None` for the clear that follows an eventual
        /// success (Task 7).
        notices: Arc<StdMutex<Vec<Option<TaskNotice>>>>,
    }

    fn rig(part_limit: usize, configure: impl FnOnce(&mut FakeProvider)) -> Rig {
        let switch = Switch::new();
        let mut provider = FakeProvider::new(Arc::clone(&switch));
        configure(&mut provider);
        Rig {
            switch,
            provider: Arc::new(provider),
            resume: Arc::new(Mutex::new(None)),
            reported: Arc::new(AtomicU64::new(0)),
            retracted: Arc::new(AtomicU64::new(0)),
            part_limit,
            notices: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    impl Rig {
        fn job(&self) -> UploadJob {
            let switch = Arc::clone(&self.switch);
            let reported = Arc::clone(&self.reported);
            let retracted = Arc::clone(&self.retracted);
            let notices = Arc::clone(&self.notices);
            UploadJob {
                task_id: "task-1".to_string(),
                bucket: "b".to_string(),
                key: "k".to_string(),
                // Deliberately absent: the runner must never open the file
                // itself -- reading a byte range is the provider's job.
                path: PathBuf::from("/nonexistent/source.bin"),
                part_limit: self.part_limit,
                tuning: TransferTuning::default(),
                token: self.switch.token.clone(),
                stop: Arc::new(move || switch.requested()),
                progress: Arc::new(move |bytes| {
                    reported.fetch_add(bytes, Ordering::SeqCst);
                }),
                regress: Arc::new(move |bytes| {
                    retracted.fetch_add(bytes, Ordering::SeqCst);
                }),
                notice: Arc::new(move |n| notices.lock().unwrap().push(n)),
                resume: Arc::clone(&self.resume),
                // The runner's decision logic is under test here, not the
                // checkpoint mirror; disabling it keeps these tests filesystem-
                // free (the engine tests cover the writer end).
                checkpoint: None,
            }
        }

        /// Every notice recorded so far, in arrival order.
        fn notices(&self) -> Vec<Option<TaskNotice>> {
            self.notices.lock().unwrap().clone()
        }

        async fn run(&self, total: u64) -> AppResult<RunOutcome> {
            run_upload(&self.job(), Arc::clone(&self.provider), total).await
        }

        async fn seed_resume(&self, state: MultipartState) {
            *self.resume.lock().await = Some(ResumeState::Upload(state));
        }

        async fn resume_state(&self) -> Option<MultipartState> {
            match self.resume.lock().await.clone() {
                Some(ResumeState::Upload(ms)) => Some(ms),
                _ => None,
            }
        }

        fn reported(&self) -> u64 {
            self.reported.load(Ordering::SeqCst)
        }

        fn retracted(&self) -> u64 {
            self.retracted.load(Ordering::SeqCst)
        }

        /// Like `job`, but pointed at a real file instead of `job()`'s
        /// deliberately-nonexistent stand-in. Only the fingerprint test below
        /// needs a real file on disk -- everything else in this module is
        /// exercised entirely through the fake provider, which never touches
        /// the filesystem itself.
        fn job_at(&self, path: PathBuf) -> UploadJob {
            UploadJob { path, ..self.job() }
        }

        async fn run_at(&self, path: PathBuf, total: u64) -> AppResult<RunOutcome> {
            run_upload(&self.job_at(path), Arc::clone(&self.provider), total).await
        }
    }

    #[tokio::test]
    async fn a_small_file_goes_up_as_one_put_and_reports_completed() {
        let rig = rig(4, |_| {});
        let outcome = rig.run(threshold() - 1).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(rig.provider.puts(), 1);
        assert_eq!(
            rig.provider.inits(),
            0,
            "below the threshold there is no multipart upload to create"
        );
        assert_eq!(rig.reported(), threshold() - 1);
        assert!(rig.resume_state().await.is_none());
    }

    #[tokio::test]
    async fn a_stop_that_is_already_pending_short_circuits_before_any_request() {
        // A multipart-sized upload, so that a missing pre-flight guard would
        // reach `multipart_init` and create an upload purely to abort it a
        // moment later -- a single-stream size can never call init, so it would
        // leave the guard's removal undetectable. The rig seeds no resume
        // state, so the guard still short-circuits after the I-1 fix, which
        // only falls through when there is a recorded upload id to clean up.
        let rig = rig(4, |_| {});
        rig.switch.request(StopKind::Cancel);

        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Stopped);
        assert_eq!(rig.provider.puts(), 0);
        assert_eq!(
            rig.provider.inits(),
            0,
            "creating a multipart upload only to abort it a moment later is pure waste"
        );
        assert_eq!(
            rig.provider.aborts(),
            0,
            "nothing was created, so there is nothing to abort"
        );
        assert_eq!(
            rig.reported(),
            0,
            "bytes that were never sent must not move the bar"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_during_a_single_stream_put_reports_stopped_without_claiming_progress() {
        // The stop lands while `PutObject` is in flight, so the request is
        // abandoned rather than observed succeeding. Claiming `Completed` here
        // would be the exact lie the outcome contract exists to prevent.
        let rig = rig(4, |fake| {
            fake.op_delay = Duration::from_millis(50);
        });
        let switch = Arc::clone(&rig.switch);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            switch.request(StopKind::Cancel);
        });

        let outcome = rig.run(1024).await.unwrap();

        assert_eq!(outcome, RunOutcome::Stopped);
        assert_eq!(
            rig.provider.puts(),
            0,
            "the request never reached the server"
        );
        assert_eq!(
            rig.reported(),
            0,
            "an abandoned put must not move the bar by a single byte"
        );
        assert_eq!(
            rig.provider.aborts(),
            0,
            "PutObject writes the whole object or nothing; there is no multipart state to abort"
        );
    }

    #[tokio::test]
    async fn a_multipart_upload_reports_completed_only_after_the_server_assembles_it() {
        let rig = rig(4, |_| {});
        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(rig.provider.inits(), 1);
        assert_eq!(rig.provider.parts_seen(), vec![1, 2]);
        assert_eq!(rig.provider.assembled_numbers(), vec![1, 2]);
        assert_eq!(rig.reported(), threshold());
        assert!(
            rig.resume_state().await.is_none(),
            "a finished upload leaves nothing to resume"
        );
    }

    #[tokio::test]
    async fn a_stop_landing_during_the_commit_does_not_undo_the_completion() {
        // The fake fires the cancel from inside `multipart_complete` -- the
        // exact moment a real runner is already committing. The bytes land, so
        // the only truthful answer is `Completed`; the engine then drops the
        // pending stop rather than filing "Canceled" beside a real object.
        let rig = rig(4, |fake| {
            *fake.stop_on_complete.lock().unwrap() = Some(StopKind::Cancel);
        });

        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(rig.switch.requested(), Some(StopKind::Cancel));
        assert_eq!(
            rig.provider.aborts(),
            0,
            "the server has already assembled the object; there is no upload left to abort"
        );
        assert!(rig.resume_state().await.is_none());
    }

    #[tokio::test]
    async fn cancelling_a_multipart_upload_aborts_it_and_drops_the_resume_state() {
        let rig = rig(1, |fake| {
            *fake.stop_at_part.lock().unwrap() = Some((1, StopKind::Cancel));
        });

        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Stopped);
        assert_eq!(
            rig.provider.aborts(),
            1,
            "server-side fragments keep costing the user storage until they are aborted"
        );
        assert!(
            rig.provider.never_assembled(),
            "a cancelled upload must not be committed"
        );
        assert!(
            rig.resume_state().await.is_none(),
            "the upload was aborted, so its id and parts are worthless"
        );
    }

    #[tokio::test]
    async fn pausing_a_multipart_upload_keeps_its_upload_id_and_accepted_parts() {
        let rig = rig(1, |fake| {
            *fake.stop_at_part.lock().unwrap() = Some((1, StopKind::Pause));
        });

        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Stopped);
        assert_eq!(
            rig.provider.aborts(),
            0,
            "aborting on a pause would throw away exactly the work the pause is meant to keep"
        );
        assert!(rig.provider.never_assembled());

        let state = rig
            .resume_state()
            .await
            .expect("a pause must leave resume state behind");
        assert_eq!(state.upload_id, UPLOAD_ID);
        assert_eq!(
            state
                .completed
                .iter()
                .map(|part| part.number)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[tokio::test]
    async fn a_resumed_upload_replays_finished_parts_into_the_progress_bar() {
        // The server confirms it holds part 1, matching the checkpoint. With
        // both in agreement the runner reuses the id and skips part 1.
        let rig = rig(4, |fake| {
            fake.listed = StdMutex::new(vec![UploadedPart {
                number: 1,
                etag: "\"etag-1\"".to_string(),
                size: part_floor(),
            }]);
        });
        rig.seed_resume(MultipartState {
            upload_id: UPLOAD_ID.to_string(),
            completed: vec![UploadedPart {
                number: 1,
                etag: "\"etag-1\"".to_string(),
                size: part_floor(),
            }],
            part_size: part_floor(),
            ..Default::default()
        })
        .await;

        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.inits(),
            0,
            "a resume must reuse the recorded upload id, not open a second one and orphan the first"
        );
        assert_eq!(
            rig.provider.parts_seen(),
            vec![2],
            "part 1 already landed on the server"
        );
        assert_eq!(rig.provider.assembled_numbers(), vec![1, 2]);
        assert_eq!(
            rig.reported(),
            threshold(),
            "the engine zeroes `transferred` on resume, so a run that does not re-report the \
             finished parts leaves the bar stuck at half"
        );
    }

    #[tokio::test]
    async fn a_permanent_part_failure_fails_the_task_and_keeps_the_resume_state() {
        let rig = rig(1, |fake| {
            fake.failures
                .lock()
                .unwrap()
                .insert(1, (u32::MAX, Fail::Permanent));
        });

        let err = rig.run(threshold()).await.unwrap_err();

        assert_eq!(err.code(), "auth/access-denied");
        assert_eq!(
            rig.provider.attempts_of(1),
            1,
            "a non-network error fails identically next time; retrying only delays the feedback"
        );
        assert!(
            rig.provider.never_assembled(),
            "a short part list must never reach CompleteMultipartUpload"
        );
        assert_eq!(
            rig.provider.aborts(),
            0,
            "the user did not ask for this to go away; a retry should reuse the upload"
        );
        let state = rig
            .resume_state()
            .await
            .expect("a failure must leave the upload id behind for a retry");
        assert_eq!(state.upload_id, UPLOAD_ID);
    }

    #[tokio::test]
    async fn a_permanent_part_failure_stops_the_parts_that_had_not_started() {
        // 8 * part_floor() is eight parts, and part 1 fails for good. The
        // spawn loop has to look at finished parts *as it goes*: a drain
        // placed only after the loop would not see part 1's failure until
        // parts 2..8 had all been uploaded, for an object that is never
        // going to be assembled.
        let rig = rig(1, |fake| {
            fake.failures
                .lock()
                .unwrap()
                .insert(1, (u32::MAX, Fail::Permanent));
        });

        let err = rig.run(8 * part_floor()).await.unwrap_err();

        assert_eq!(err.code(), "auth/access-denied");
        let sent = rig.provider.parts_seen();
        assert!(
            sent.len() <= 2,
            "one part failing for good must stop the other seven, not merely be reported \
             after them; parts actually sent: {sent:?}"
        );
    }

    #[tokio::test]
    async fn a_cancel_racing_a_part_failure_still_aborts_the_upload() {
        // Both land at once: the part fails permanently *and* the user
        // cancels. If the error won, the engine would file `Failed` -- and
        // cancelling a `Failed` task never reaches a runner again, so this
        // abort would be the cleanup nobody ever performs.
        let rig = rig(1, |fake| {
            *fake.stop_at_part.lock().unwrap() = Some((1, StopKind::Cancel));
            fake.failures
                .lock()
                .unwrap()
                .insert(1, (u32::MAX, Fail::Permanent));
        });

        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Stopped);
        assert_eq!(rig.provider.aborts(), 1);
        assert!(rig.provider.never_assembled());
        assert!(rig.resume_state().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_transient_part_failure_is_retried_within_the_budget() {
        let rig = rig(4, |fake| {
            fake.failures
                .lock()
                .unwrap()
                .insert(1, (2, Fail::Transient));
        });

        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.attempts_of(1),
            3,
            "two timeouts must cost two retries, not the task"
        );
        assert_eq!(rig.provider.assembled_numbers(), vec![1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_transient_part_failure_emits_a_retry_notice_and_clears_it_on_success() {
        // Task 7 (spec §7.4): each retryable failure surfaces a `TaskNotice`
        // before the backoff sleep, and the eventual success clears it --
        // exactly once, since a step that never failed must never touch
        // `notice` at all (see the other multipart tests' silence on this).
        let rig = rig(4, |fake| {
            fake.failures
                .lock()
                .unwrap()
                .insert(1, (2, Fail::Transient));
        });

        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.notices(),
            vec![
                Some(TaskNotice {
                    code: "network/timeout".to_string(),
                    attempt: 1,
                    max: MAX_RETRIES,
                }),
                Some(TaskNotice {
                    code: "network/timeout".to_string(),
                    attempt: 2,
                    max: MAX_RETRIES,
                }),
                None,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_part_that_exhausts_the_retry_budget_fails_the_task() {
        let rig = rig(4, |fake| {
            fake.failures
                .lock()
                .unwrap()
                .insert(1, (u32::MAX, Fail::Transient));
        });

        let err = rig.run(threshold()).await.unwrap_err();

        assert_eq!(err.code(), "network/timeout");
        assert_eq!(
            rig.provider.attempts_of(1),
            MAX_RETRIES as usize + 1,
            "MAX_RETRIES is retries *after* the first attempt"
        );
        assert!(rig.provider.never_assembled());
    }

    #[tokio::test(start_paused = true)]
    async fn parts_in_flight_never_exceed_the_part_limit() {
        // 8 * part_floor() is eight equal parts, so the limit has to be
        // enforced by the spawn loop rather than by there being nothing left
        // to spawn.
        let rig = rig(2, |fake| {
            fake.op_delay = Duration::from_millis(10);
        });

        let outcome = rig.run(8 * part_floor()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(rig.provider.parts_seen().len(), 8);
        assert_eq!(
            rig.provider.peak(),
            2,
            "the semaphore must be acquired before spawning, or all eight parts go out at once"
        );
    }

    #[tokio::test]
    async fn a_resume_state_naming_a_part_outside_the_plan_never_reaches_complete() {
        // Even the *server* can report a part this plan does not contain (an
        // upload created against a different plan). It must be dropped, not
        // carried into the commit that `CompleteMultipartUpload` would reject.
        let rig = rig(4, |fake| {
            fake.listed = StdMutex::new(vec![UploadedPart {
                number: 99,
                etag: "\"stale\"".to_string(),
                size: part_floor(),
            }]);
        });
        rig.seed_resume(MultipartState {
            upload_id: UPLOAD_ID.to_string(),
            completed: vec![UploadedPart {
                number: 99,
                etag: "\"stale\"".to_string(),
                size: part_floor(),
            }],
            part_size: part_floor(),
            ..Default::default()
        })
        .await;

        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.assembled_numbers(),
            vec![1, 2],
            "CompleteMultipartUpload rejects a part number the upload never issued, so a stale \
             entry has to be dropped rather than carried into the commit"
        );
        assert_eq!(
            rig.reported(),
            threshold(),
            "a dropped entry must not be reported as transferred either"
        );
    }

    #[tokio::test]
    async fn a_failed_abort_still_reports_the_cancellation() {
        let rig = rig(1, |fake| {
            *fake.stop_at_part.lock().unwrap() = Some((1, StopKind::Cancel));
            fake.abort_fails = true;
        });

        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(
            outcome,
            RunOutcome::Stopped,
            "a cleanup that failed must not turn the user's cancellation into a failed task"
        );
        assert_eq!(rig.provider.aborts(), 1);
    }

    #[tokio::test]
    async fn cancelling_a_resumed_upload_at_the_preflight_still_aborts_the_fragments() {
        // A task paused mid-multipart holds `{upload_id, completed}` in its
        // resume slot. On resume the engine spawns a fresh driver and applies
        // `Start`; if the user cancels in that window the stop is already
        // pending when the runner reaches its pre-flight. Short-circuiting there
        // would return `Stopped` without aborting -- the engine files
        // `Canceled`, `clear_finished` drops the record, and the recorded
        // upload id's fragments bill the user forever. The pre-flight must fall
        // through into the multipart path so the cancel branch aborts.
        let rig = rig(4, |_| {});
        rig.seed_resume(MultipartState {
            upload_id: UPLOAD_ID.to_string(),
            completed: vec![UploadedPart {
                number: 1,
                etag: "\"etag-1\"".to_string(),
                size: part_floor(),
            }],
            part_size: part_floor(),
            ..Default::default()
        })
        .await;
        rig.switch.request(StopKind::Cancel);

        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Stopped);
        assert_eq!(
            rig.provider.aborts(),
            1,
            "the resume slot held an upload id; cancelling without aborting strands the \
             fragments forever"
        );
        assert_eq!(
            rig.provider.inits(),
            0,
            "a resume reuses the recorded upload id rather than opening a second one"
        );
        assert!(
            rig.provider.never_assembled(),
            "a cancelled upload must not be committed"
        );
        assert!(
            rig.resume_state().await.is_none(),
            "the upload was aborted, so its id and parts are worthless"
        );
    }

    #[tokio::test]
    async fn a_paused_multipart_upload_records_the_source_files_fingerprint() {
        // A completed upload clears its resume slot entirely (see
        // `a_multipart_upload_reports_completed_only_after_the_server_assembles_it`),
        // so the fingerprint has to be observed on a *paused* upload instead --
        // exactly like `pausing_a_multipart_upload_keeps_its_upload_id_and_accepted_parts`,
        // just against a real file so `run_upload` has something to stat.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("src.bin");
        std::fs::write(&path, vec![7u8; threshold() as usize]).unwrap(); // at the threshold -> multipart
        let meta = std::fs::metadata(&path).unwrap();

        let rig = rig(1, |fake| {
            *fake.stop_at_part.lock().unwrap() = Some((1, StopKind::Pause));
        });

        let outcome = rig.run_at(path.clone(), threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Stopped);
        let state = rig
            .resume_state()
            .await
            .expect("a pause must leave resume state, fingerprint included");
        assert_eq!(state.source_size, meta.len());
        assert!(state.source_mtime > 0, "mtime must be captured");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn resume_restarts_when_the_source_changed() {
        // The resume state was captured against a file whose fingerprint no
        // longer matches what is on disk. Continuing on the old upload id would
        // splice new parts onto old ones and assemble a corrupt object, so the
        // runner must abort the stale upload and start a brand-new one against
        // the current file -- re-uploading every part rather than trusting the
        // checkpoint's `completed`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("src.bin");
        std::fs::write(&path, vec![9u8; threshold() as usize]).unwrap();

        let rig = rig(4, |_| {});
        rig.seed_resume(MultipartState {
            upload_id: UPLOAD_ID.to_string(),
            // Claims part 1 is already done -- a claim the source change voids.
            completed: vec![UploadedPart {
                number: 1,
                etag: "\"old\"".to_string(),
                size: part_floor(),
            }],
            // A size that cannot be the file we just wrote: the fingerprint
            // mismatch is what forces the restart.
            source_size: 999,
            source_mtime: 1,
            part_size: part_floor(),
        })
        .await;

        let outcome = rig.run_at(path.clone(), threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.aborts(),
            1,
            "the stale upload id must be aborted so its fragments stop billing"
        );
        assert_eq!(
            rig.provider.inits(),
            1,
            "a changed source forces a brand-new multipart upload rather than continuing the old id"
        );
        assert_eq!(
            rig.provider.parts_seen(),
            vec![1, 2],
            "every part is re-uploaded against the new file; nothing is carried from the stale \
             checkpoint"
        );
        assert_eq!(rig.provider.assembled_numbers(), vec![1, 2]);
        assert_eq!(
            rig.reported(),
            threshold(),
            "only the bytes actually re-sent count; the voided part 1 is not re-reported"
        );
        assert!(rig.resume_state().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn resume_trusts_multipart_list_not_the_checkpoint_completed() {
        // The checkpoint claims part 1 is already on the server, but the
        // server's own ListParts reports nothing. The server wins: part 1 must
        // be (re-)uploaded rather than skipped on the strength of a checkpoint
        // that could have been written before the part actually landed.
        let rig = rig(4, |_| {}); // fake.multipart_list returns empty by default
        rig.seed_resume(MultipartState {
            upload_id: UPLOAD_ID.to_string(),
            completed: vec![UploadedPart {
                number: 1,
                etag: "\"claimed-but-never-landed\"".to_string(),
                size: part_floor(),
            }],
            // (0, 0) matches the fingerprint of the deliberately-nonexistent
            // stand-in path, so the source-change branch is not taken and the
            // reconciliation against the server's part list is what decides.
            source_size: 0,
            source_mtime: 0,
            part_size: part_floor(),
        })
        .await;

        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.inits(),
            0,
            "the upload id is still valid, so the run continues it rather than opening a new one"
        );
        assert_eq!(
            rig.provider.aborts(),
            0,
            "nothing is stale -- the fingerprint matched, so there is nothing to abort"
        );
        assert_eq!(
            rig.provider.attempts_of(1),
            1,
            "the server never accepted part 1, so it must actually be uploaded despite the \
             checkpoint claiming otherwise"
        );
        assert_eq!(rig.provider.parts_seen(), vec![1, 2]);
        assert_eq!(rig.provider.assembled_numbers(), vec![1, 2]);
    }

    // Test (Task 3, spec §4.6): a resume whose recorded `part_size` no longer
    // matches the current plan must be treated exactly like a changed source
    // -- the stale upload is aborted and a brand-new one started, rather than
    // splicing parts planned under a different part size onto the old
    // upload id. The fingerprint is left matching (both default to (0, 0))
    // so only the part_size mismatch can be forcing the restart. Modelled on
    // `resume_restarts_when_the_source_changed`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn resume_restarts_when_the_recorded_part_size_does_not_match_the_plan() {
        let rig = rig(4, |_| {});
        rig.seed_resume(MultipartState {
            upload_id: UPLOAD_ID.to_string(),
            // Claims part 1 is already done -- a claim the part_size mismatch
            // voids.
            completed: vec![UploadedPart {
                number: 1,
                etag: "\"old\"".to_string(),
                size: part_floor(),
            }],
            // (0, 0) matches the fingerprint of the deliberately-nonexistent
            // stand-in path, so the source-change branch is not taken.
            source_size: 0,
            source_mtime: 0,
            // Deliberately wrong: the current plan's part_size is
            // `part_floor()` for a `threshold()`-sized upload.
            part_size: part_floor() + 1,
        })
        .await;

        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.aborts(),
            1,
            "a mismatched recorded part_size must abort the stale upload -- its fragments would \
             otherwise keep billing the user"
        );
        assert_eq!(
            rig.provider.inits(),
            1,
            "a mismatched part_size forces a brand-new multipart upload rather than continuing \
             the old id"
        );
        assert_eq!(
            rig.provider.parts_seen(),
            vec![1, 2],
            "every part is re-uploaded against the fresh upload; nothing is carried from the \
             stale checkpoint"
        );
        assert_eq!(rig.provider.assembled_numbers(), vec![1, 2]);
        assert!(rig.resume_state().await.is_none());
    }

    #[tokio::test]
    async fn a_rejected_commit_fails_the_task_rather_than_reporting_completed() {
        // `multipart_complete` returning an error means the server refused to
        // assemble the object. Reporting `Completed` here is the one lie the
        // engine cannot catch -- it would record success next to an object that
        // does not exist. The only truthful answer is `Err`, so the engine
        // files `Failed`; the resume slot must survive so a retry can re-issue
        // the commit rather than orphan the upload.
        let rig = rig(4, |fake| {
            fake.complete_fails = true;
        });

        let err = rig.run(threshold()).await.unwrap_err();

        assert_eq!(
            err.code(),
            "network/unreachable",
            "a rejected commit must surface as the server's error, not a silent success"
        );
        let state = rig
            .resume_state()
            .await
            .expect("a rejected commit must leave the upload id behind for a retry");
        assert_eq!(state.upload_id, UPLOAD_ID);
    }

    // ---- Task 3: incremental progress + retry-safe rollback ---------------
    //
    // Below `upload_single`/`upload_one_part` used to report a whole
    // file/part's length in one shot, only once the provider call returned
    // `Ok`. Now the body reports `Sent` notches as the transport pulls it, so
    // the bar can move while a large upload is still in flight -- which
    // means a failed, cancelled or rewound attempt can leave bytes on the
    // bar that the server never actually kept. These four tests are the
    // conservation invariant from the task brief: `reported - retracted`
    // never overstates what the server holds, and lands exactly on the true
    // length once a part/file lands.

    #[tokio::test]
    async fn sent_notches_move_the_bar_before_the_put_resolves_and_true_up_covers_the_rest() {
        let total = 20 * MB;
        assert!(
            total < threshold(),
            "this exercises the single-stream path, not multipart"
        );

        let rig = Arc::new(rig(4, |fake| {
            fake.put_notches = vec![8 * MB, 8 * MB, 4 * MB];
            fake.put_pause_after = Some(0);
        }));

        let runner = {
            let rig = Arc::clone(&rig);
            tokio::spawn(async move { rig.run(total).await })
        };

        // Parked right after the first notch: the put has not resolved yet,
        // so this is strictly "before completion".
        rig.provider.put_gate.wait().await;
        let mid_flight = rig.reported();
        assert!(
            mid_flight > 0 && mid_flight < total,
            "the first Sent notch must move the bar before PutObject resolves, but not claim \
             the whole file yet: reported={mid_flight}"
        );
        rig.provider.put_gate.open();

        let outcome = runner.await.unwrap().unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.reported(),
            total,
            "true-up must land the bar exactly on the full length once PutObject succeeds"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_parts_failed_attempt_retracts_exactly_what_it_sent_and_the_retry_true_ups_the_rest()
    {
        let rig = rig(4, |fake| {
            fake.failures
                .lock()
                .unwrap()
                .insert(1, (1, Fail::Transient));
            fake.part_notches
                .lock()
                .unwrap()
                .insert(1, VecDeque::from(vec![vec![5 * MB]]));
        });

        let outcome = rig.run(threshold()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.attempts_of(1),
            2,
            "one timeout must cost one retry, not the task"
        );
        assert_eq!(
            rig.retracted(),
            5 * MB,
            "the failed attempt's 5MB must be retracted before the retry, not left dangling"
        );
        assert_eq!(
            rig.reported() - rig.retracted(),
            threshold(),
            "no permanent over-count despite the retry: net reported (gross reported minus \
             what was retracted) must land exactly on the total"
        );
    }

    #[tokio::test]
    async fn a_cancel_mid_part_retracts_only_that_parts_in_flight_bytes() {
        // part_limit=1 so the two parts run strictly in sequence: part 1
        // finishes normally (nothing to retract), then part 2 is caught
        // mid-flight and cancelled. "已完成片保留" -- part 1's report must
        // survive; only part 2's dangling bytes come back off the bar.
        let rig = Arc::new(rig(1, |fake| {
            fake.part_notches
                .lock()
                .unwrap()
                .insert(2, VecDeque::from(vec![vec![5 * MB]]));
            fake.part_pause = Some(PartPause {
                part: 2,
                attempt: 1,
                after: 0,
            });
        }));

        let runner = {
            let rig = Arc::clone(&rig);
            tokio::spawn(async move { rig.run(threshold()).await })
        };

        // Part 2 has sent 5MB and is parked mid-attempt, still "in flight".
        rig.provider.part_gate.wait().await;
        assert_eq!(
            rig.reported(),
            part_floor() + 5 * MB,
            "part 1 landed in full and part 2's first notch already moved the bar"
        );
        rig.switch.request(StopKind::Cancel);
        // Never opened: the runner's own `with_retry` select drops the
        // parked call once the token fires, which is the abandonment this
        // test exercises.

        let outcome = runner.await.unwrap().unwrap();

        assert_eq!(outcome, RunOutcome::Stopped);
        assert_eq!(
            rig.provider.aborts(),
            1,
            "a cancel must abort the multipart upload"
        );
        assert!(rig.provider.never_assembled());
        assert_eq!(
            rig.retracted(),
            5 * MB,
            "exactly the aborted attempt's in-flight bytes must be retracted -- no more, no less"
        );
        assert_eq!(
            rig.reported() - rig.retracted(),
            part_floor(),
            "net reported must fall back to only the completed part; the cancelled part's \
             in-flight bytes must net to zero"
        );
    }

    #[tokio::test]
    async fn every_parts_completion_true_ups_reported_to_exactly_the_completed_length_sum() {
        // Four parts, each scripted to stream a different, partial amount of
        // its own length before the fake resolves the call. True-up must
        // make each one up to exactly its own length -- never more, never
        // less -- however the parts interleave under real concurrency.
        let rig = rig(4, |fake| {
            let mut notches = fake.part_notches.lock().unwrap();
            notches.insert(1, VecDeque::from(vec![vec![3 * MB]]));
            notches.insert(2, VecDeque::from(vec![vec![7 * MB]]));
            notches.insert(3, VecDeque::from(vec![vec![MB, 2 * MB]]));
            notches.insert(4, VecDeque::from(vec![vec![]]));
        });

        let outcome = rig.run(4 * part_floor()).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(rig.provider.assembled_numbers(), vec![1, 2, 3, 4]);
        assert_eq!(
            rig.reported(),
            4 * part_floor(),
            "every part's Sent notches plus its true-up must sum to exactly its own length -- \
             summed across all four parts, reported must equal the total exactly"
        );
        assert_eq!(
            rig.retracted(),
            0,
            "nothing failed or was cancelled, so nothing should ever have been retracted"
        );
    }
}
