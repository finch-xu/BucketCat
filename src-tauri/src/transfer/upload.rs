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
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::provider::{Provider, UploadedPart};
use crate::transfer::engine::{
    MultipartState, ResumeState, RunOutcome, StopKind, TaskContext, TransferRunner,
};
use crate::transfer::part::{plan_upload, PartSpec, UploadPlan};
use crate::transfer::retry::{backoff_delay, is_retryable, MAX_RETRIES};

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

/// The slice of a [`TaskContext`] an upload actually uses.
struct UploadJob {
    task_id: String,
    bucket: String,
    key: String,
    path: PathBuf,
    part_limit: usize,
    token: CancellationToken,
    stop: StopFn,
    progress: ProgressFn,
    resume: Arc<Mutex<Option<ResumeState>>>,
}

impl UploadJob {
    fn from_context(ctx: &TaskContext) -> Self {
        let control = ctx.control.clone();
        let progress = ctx.progress.clone();
        Self {
            task_id: ctx.task.id.clone(),
            bucket: ctx.task.bucket.clone(),
            key: ctx.task.key.clone(),
            path: PathBuf::from(&ctx.task.local_path),
            part_limit: ctx.part_limit,
            token: ctx.control.token(),
            // `ctx.task` is a snapshot taken before the task went `Running`,
            // so its `status` is stale; the control is the only live source of
            // stop intent.
            stop: Arc::new(move || control.requested()),
            progress: Arc::new(move |bytes| progress.add(bytes)),
            resume: Arc::clone(&ctx.resume),
        }
    }

    fn stopped(&self) -> Option<StopKind> {
        (*self.stop)()
    }

    fn report(&self, bytes: u64) {
        (*self.progress)(bytes);
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

    match plan_upload(total) {
        UploadPlan::Single { length } => upload_single(job, provider, length).await,
        UploadPlan::Multipart { parts, part_size } => {
            tracing::info!(
                task = %job.task_id,
                parts = parts.len(),
                part_size,
                "uploading as multipart"
            );
            upload_multipart(job, provider, parts).await
        }
    }
}

async fn upload_single<P>(job: &UploadJob, provider: Arc<P>, length: u64) -> AppResult<RunOutcome>
where
    P: Provider + Send + Sync + 'static,
{
    tracing::info!(task = %job.task_id, bytes = length, "uploading as a single stream");

    let put = with_retry(&job.token, || {
        let provider = Arc::clone(&provider);
        let bucket = job.bucket.clone();
        let key = job.key.clone();
        let path = job.path.clone();
        async move {
            provider
                .put_object_from_file(&bucket, &key, &path, length)
                .await
        }
    })
    .await?;

    if put.is_none() {
        // The token fired, so `PutObject` was abandoned rather than observed
        // succeeding: neither the bytes nor the completion may be claimed. No
        // cleanup is owed -- a single `PutObject` writes the whole object or
        // nothing, and there is no multipart state to abort.
        return Ok(RunOutcome::Stopped);
    }

    // Truthful only here: the request returned `Ok`, so the object exists even
    // if a stop was requested while it was in flight.
    job.report(length);
    Ok(RunOutcome::Completed)
}

async fn upload_multipart<P>(
    job: &UploadJob,
    provider: Arc<P>,
    plan: Vec<PartSpec>,
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

    // A recorded completion naming a part this plan does not contain is
    // unusable: `CompleteMultipartUpload` rejects a part number the upload
    // never issued, so carrying one forward would fail the whole transfer at
    // its very last step. `pending_parts` already ignores such entries;
    // dropping them here as well keeps the accepted list and the plan in step.
    let planned: HashSet<i32> = plan.iter().map(|spec| spec.number).collect();
    let carried: Vec<UploadedPart> = existing
        .completed
        .iter()
        .filter(|part| planned.contains(&part.number))
        .cloned()
        .collect();

    let upload_id = if existing.upload_id.is_empty() {
        let id = provider.multipart_init(&job.bucket, &job.key).await?;
        // Recorded immediately: if everything below fails, a cancel still has
        // an upload id to abort with, so no server-side fragments leak.
        *job.resume.lock().await = Some(ResumeState::Upload(MultipartState {
            upload_id: id.clone(),
            completed: carried.clone(),
        }));
        id
    } else {
        existing.upload_id.clone()
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

        set.spawn(async move {
            let _permit = permit;
            upload_one_part(provider, target, spec, token, progress).await
        });
    }

    while let Some(joined) = set.join_next().await {
        absorb_part(joined, &completed, &mut first_error, &part_token).await;
    }

    // Persist what landed before deciding anything, so a pause, a permanent
    // failure and a later retry all resume from the same place.
    let done = completed.lock().await.clone();
    *job.resume.lock().await = Some(ResumeState::Upload(MultipartState {
        upload_id: upload_id.clone(),
        completed: done.clone(),
    }));

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
        // re-uploading from zero.
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
) -> AppResult<Option<UploadedPart>>
where
    P: Provider + Send + Sync + 'static,
{
    let etag = with_retry(&token, || {
        let provider = Arc::clone(&provider);
        let target = Arc::clone(&target);
        async move {
            provider
                .upload_part_from_file(
                    &target.bucket,
                    &target.key,
                    &target.upload_id,
                    spec.number,
                    &target.path,
                    spec.offset,
                    spec.length,
                )
                .await
        }
    })
    .await?;

    let Some(etag) = etag else {
        return Ok(None);
    };
    progress(spec.length);
    Ok(Some(UploadedPart {
        number: spec.number,
        etag,
        size: spec.length,
    }))
}

/// Runs `op` with the retry policy from `transfer::retry`, giving up early if
/// `token` fires. `Ok(None)` means "stopped, not failed" -- the caller must
/// not treat it as success *or* as an error.
async fn with_retry<T, F, Fut>(token: &CancellationToken, mut op: F) -> AppResult<Option<T>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = AppResult<T>>,
{
    let mut retries = 0u32;
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
            Ok(value) => return Ok(Some(value)),
            Err(err) => {
                retries += 1;
                if !is_retryable(&err) || retries > MAX_RETRIES {
                    return Err(err);
                }
                let delay = backoff_delay(retries);
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
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use crate::provider::{BatchResult, Bucket, ListPage, ObjectHead};
    use crate::transfer::part::MULTIPART_THRESHOLD;

    const MB: u64 = 1024 * 1024;
    const UPLOAD_ID: &str = "u-1";

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
        ) -> AppResult<()> {
            if !self.op_delay.is_zero() {
                tokio::time::sleep(self.op_delay).await;
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

        async fn upload_part_from_file(
            &self,
            _bucket: &str,
            _key: &str,
            upload_id: &str,
            part_number: i32,
            _path: &Path,
            _offset: u64,
            _length: u64,
        ) -> AppResult<String> {
            assert_eq!(
                upload_id, UPLOAD_ID,
                "parts must be sent under the id multipart_init returned"
            );
            self.attempts.lock().unwrap().push(part_number);

            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(live, Ordering::SeqCst);
            if !self.op_delay.is_zero() {
                tokio::time::sleep(self.op_delay).await;
            }
            self.live.fetch_sub(1, Ordering::SeqCst);

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

        async fn head_object(&self, _bucket: &str, _key: &str) -> AppResult<ObjectHead> {
            unimplemented!("an upload never heads objects")
        }

        async fn get_range(
            &self,
            _bucket: &str,
            _key: &str,
            _offset: u64,
            _length: u64,
        ) -> AppResult<Vec<u8>> {
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
    }

    struct Rig {
        switch: Arc<Switch>,
        provider: Arc<FakeProvider>,
        resume: Arc<Mutex<Option<ResumeState>>>,
        reported: Arc<AtomicU64>,
        part_limit: usize,
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
            part_limit,
        }
    }

    impl Rig {
        fn job(&self) -> UploadJob {
            let switch = Arc::clone(&self.switch);
            let reported = Arc::clone(&self.reported);
            UploadJob {
                task_id: "task-1".to_string(),
                bucket: "b".to_string(),
                key: "k".to_string(),
                // Deliberately absent: the runner must never open the file
                // itself -- reading a byte range is the provider's job.
                path: PathBuf::from("/nonexistent/source.bin"),
                part_limit: self.part_limit,
                token: self.switch.token.clone(),
                stop: Arc::new(move || switch.requested()),
                progress: Arc::new(move |bytes| {
                    reported.fetch_add(bytes, Ordering::SeqCst);
                }),
                resume: Arc::clone(&self.resume),
            }
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
    }

    #[tokio::test]
    async fn a_small_file_goes_up_as_one_put_and_reports_completed() {
        let rig = rig(4, |_| {});
        let outcome = rig.run(MULTIPART_THRESHOLD - 1).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(rig.provider.puts(), 1);
        assert_eq!(
            rig.provider.inits(),
            0,
            "below the threshold there is no multipart upload to create"
        );
        assert_eq!(rig.reported(), MULTIPART_THRESHOLD - 1);
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

        let outcome = rig.run(MULTIPART_THRESHOLD).await.unwrap();

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
        let outcome = rig.run(16 * MB).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(rig.provider.inits(), 1);
        assert_eq!(rig.provider.parts_seen(), vec![1, 2]);
        assert_eq!(rig.provider.assembled_numbers(), vec![1, 2]);
        assert_eq!(rig.reported(), 16 * MB);
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

        let outcome = rig.run(16 * MB).await.unwrap();

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

        let outcome = rig.run(16 * MB).await.unwrap();

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

        let outcome = rig.run(16 * MB).await.unwrap();

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
        let rig = rig(4, |_| {});
        rig.seed_resume(MultipartState {
            upload_id: UPLOAD_ID.to_string(),
            completed: vec![UploadedPart {
                number: 1,
                etag: "\"etag-1\"".to_string(),
                size: 8 * MB,
            }],
        })
        .await;

        let outcome = rig.run(16 * MB).await.unwrap();

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
            16 * MB,
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

        let err = rig.run(16 * MB).await.unwrap_err();

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
        // 64MB is eight parts, and part 1 fails for good. The spawn loop has
        // to look at finished parts *as it goes*: a drain placed only after
        // the loop would not see part 1's failure until parts 2..8 had all
        // been uploaded, for an object that is never going to be assembled.
        let rig = rig(1, |fake| {
            fake.failures
                .lock()
                .unwrap()
                .insert(1, (u32::MAX, Fail::Permanent));
        });

        let err = rig.run(64 * MB).await.unwrap_err();

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

        let outcome = rig.run(16 * MB).await.unwrap();

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

        let outcome = rig.run(16 * MB).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.attempts_of(1),
            3,
            "two timeouts must cost two retries, not the task"
        );
        assert_eq!(rig.provider.assembled_numbers(), vec![1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_part_that_exhausts_the_retry_budget_fails_the_task() {
        let rig = rig(4, |fake| {
            fake.failures
                .lock()
                .unwrap()
                .insert(1, (u32::MAX, Fail::Transient));
        });

        let err = rig.run(16 * MB).await.unwrap_err();

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
        // 64MB is eight 8MB parts, so the limit has to be enforced by the
        // spawn loop rather than by there being nothing left to spawn.
        let rig = rig(2, |fake| {
            fake.op_delay = Duration::from_millis(10);
        });

        let outcome = rig.run(64 * MB).await.unwrap();

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
        let rig = rig(4, |_| {});
        rig.seed_resume(MultipartState {
            upload_id: UPLOAD_ID.to_string(),
            completed: vec![UploadedPart {
                number: 99,
                etag: "\"stale\"".to_string(),
                size: 8 * MB,
            }],
        })
        .await;

        let outcome = rig.run(16 * MB).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.assembled_numbers(),
            vec![1, 2],
            "CompleteMultipartUpload rejects a part number the upload never issued, so a stale \
             entry has to be dropped rather than carried into the commit"
        );
        assert_eq!(
            rig.reported(),
            16 * MB,
            "a dropped entry must not be reported as transferred either"
        );
    }

    #[tokio::test]
    async fn a_failed_abort_still_reports_the_cancellation() {
        let rig = rig(1, |fake| {
            *fake.stop_at_part.lock().unwrap() = Some((1, StopKind::Cancel));
            fake.abort_fails = true;
        });

        let outcome = rig.run(16 * MB).await.unwrap();

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
                size: 8 * MB,
            }],
        })
        .await;
        rig.switch.request(StopKind::Cancel);

        let outcome = rig.run(16 * MB).await.unwrap();

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

        let err = rig.run(16 * MB).await.unwrap_err();

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
}
