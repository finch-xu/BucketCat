//! The download runner: single Range GET below the multipart threshold,
//! otherwise N concurrent Range GETs into a preallocated `.bcpart`, renamed
//! into place only when every chunk has landed (design §5).
//!
//! ## Who decides that a download happened
//!
//! Mirrors the upload runner: [`RunOutcome::Completed`] is a claim the engine
//! records verbatim, so it is returned from exactly one place -- straight
//! after [`PartFile::finish`] (the atomic rename) returned `Ok`. A `Completed`
//! next to a `.bcpart` that was never renamed would report success for a file
//! the user does not have, and nothing downstream could catch it. Every other
//! way out is an error or [`RunOutcome::Stopped`].
//!
//! ## Cancel and pause differ in what they leave on disk
//!
//! Both report `Stopped`; the difference is the residue. A **cancel** deletes
//! the `.bcpart` and clears the resume slot -- there is nothing to continue. A
//! **pause** keeps both the `.bcpart` and the finished chunk numbers so the
//! next run reopens the same staging file and skips the chunks already on
//! disk.
//!
//! ## Testability
//!
//! [`run_download`] is generic over the [`Provider`], exactly as `upload.rs`'s
//! `run_upload` is: the outcome-deciding logic below (cancel/pause/resume,
//! progress replay, retry) is the part nothing downstream can second-guess, so
//! it is driven in tests by a fake provider that serves ranges from an
//! in-memory buffer into a real `tempfile::tempdir()`. [`DownloadJob`] narrows
//! a [`TaskContext`] to the slice a download needs, going through closures for
//! the pieces (`ProgressHandle`, `TaskControl`) the engine keeps
//! unconstructible from outside.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::provider::Provider;
use crate::transfer::engine::{
    DownloadState, ResumeState, RunOutcome, StopKind, TaskContext, TransferRunner,
};
use crate::transfer::part::{plan_upload, PartSpec, UploadPlan};
use crate::transfer::partfile::PartFile;
use crate::transfer::retry::{backoff_delay, is_retryable, MAX_RETRIES};

/// Reports transferred bytes.
///
/// A closure rather than a [`crate::transfer::ProgressHandle`] because that
/// type is only constructible inside `transfer::engine`; going through a
/// closure is what lets the code below -- where a wrong answer means claiming
/// a download that did not happen -- be tested at all.
type ProgressFn = Arc<dyn Fn(u64) + Send + Sync>;

/// Reads the task's stop intent. A closure for the same reason as
/// [`ProgressFn`]: [`crate::transfer::TaskControl`] has no public constructor.
type StopFn = Arc<dyn Fn() -> Option<StopKind> + Send + Sync>;

/// Chunks of `plan` not yet written to the `.bcpart`. Pure so resume
/// arithmetic is testable without a network. Reuses upload's part planning
/// (D1): the offset/length split is identical; the S3-multipart caps just
/// never bind for a download.
pub fn pending_chunks(plan: &[PartSpec], done: &[i32]) -> Vec<PartSpec> {
    let finished: HashSet<i32> = done.iter().copied().collect();
    plan.iter()
        .filter(|c| !finished.contains(&c.number))
        .copied()
        .collect()
}

/// The slice of a [`TaskContext`] a download actually uses.
struct DownloadJob {
    task_id: String,
    bucket: String,
    key: String,
    /// Final destination; the runner stages into `<target>.bcpart` and renames
    /// it into place on finish.
    target: PathBuf,
    part_limit: usize,
    token: CancellationToken,
    stop: StopFn,
    progress: ProgressFn,
    resume: Arc<Mutex<Option<ResumeState>>>,
}

impl DownloadJob {
    fn from_context(ctx: &TaskContext) -> Self {
        let control = ctx.control.clone();
        let progress = ctx.progress.clone();
        Self {
            task_id: ctx.task.id.clone(),
            bucket: ctx.task.bucket.clone(),
            key: ctx.task.key.clone(),
            target: PathBuf::from(&ctx.task.local_path),
            part_limit: ctx.part_limit,
            token: ctx.control.token(),
            // `ctx.task` is a snapshot taken before the task went `Running`, so
            // its `status` is stale; the control is the only live source of
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

pub struct DownloadRunner;

#[async_trait]
impl TransferRunner for DownloadRunner {
    async fn run(&self, ctx: TaskContext) -> AppResult<RunOutcome> {
        let provider = ctx.hub.provider(&ctx.task.connection_id).await?;
        run_download(&DownloadJob::from_context(&ctx), provider, ctx.task.total).await
    }
}

/// Downloads `total` bytes of `job.bucket`/`job.key` into `job.target`.
///
/// Generic over the provider so the decision points below can be exercised
/// against a fake; production always passes the hub's `S3Provider`.
async fn run_download<P>(job: &DownloadJob, provider: Arc<P>, total: u64) -> AppResult<RunOutcome>
where
    P: Provider + Send + Sync + 'static,
{
    // Resume: reuse the existing .bcpart + completed chunks if present.
    let existing = match job.resume.lock().await.clone() {
        Some(ResumeState::Download(ds)) => Some(ds),
        _ => None,
    };

    let plan = plan_upload(total);
    let chunks: Vec<PartSpec> = match &plan {
        UploadPlan::Single { length } => vec![PartSpec {
            number: 1,
            offset: 0,
            length: *length,
        }],
        UploadPlan::Multipart { parts, .. } => parts.clone(),
    };

    match &plan {
        UploadPlan::Single { .. } => {
            tracing::info!(task = %job.task_id, key = %job.key, "downloading as a single stream");
        }
        UploadPlan::Multipart { .. } => {
            tracing::info!(
                task = %job.task_id,
                key = %job.key,
                chunks = chunks.len(),
                "downloading as multipart"
            );
        }
    }

    // Open (or reopen) the staging file. On a fresh start create+preallocate;
    // on resume the .bcpart already holds the finished chunks.
    let (partfile, done) = if let Some(ds) = existing {
        (
            PartFile::reopen(&job.target, total, &ds.bcpart)?,
            ds.completed_parts,
        )
    } else {
        (PartFile::create(&job.target, total)?, Vec::new())
    };
    let partfile = Arc::new(partfile);
    let bcpart = partfile.bcpart_path().to_path_buf();

    // A recorded completion naming a chunk this plan does not contain is
    // unusable: it cannot be finished, and carrying it forward would make the
    // `completed.len() == chunks.len()` guard below trip on a phantom surplus.
    // `pending_chunks` already ignores such entries; dropping them here as well
    // keeps the finished list and the plan in step (mirrors upload's `carried`).
    let planned: HashSet<i32> = chunks.iter().map(|c| c.number).collect();
    let done: Vec<i32> = done.into_iter().filter(|n| planned.contains(n)).collect();

    // Re-report already-finished chunks so the bar doesn't under-count after
    // the engine zeroed `transferred` on resume.
    for c in chunks.iter().filter(|c| done.contains(&c.number)) {
        job.report(c.length);
    }
    // Record resume state immediately so a cancel before any chunk lands still
    // knows which .bcpart to delete.
    *job.resume.lock().await = Some(ResumeState::Download(DownloadState {
        etag: None,
        completed_parts: done.clone(),
        bcpart: bcpart.clone(),
    }));

    let pending = pending_chunks(&chunks, &done);
    let permits = Arc::new(Semaphore::new(job.part_limit.max(1)));
    // A child token, so stopping the remaining chunks after one of them failed
    // for good is distinguishable from a user-requested stop: a chunk that gave
    // up because a sibling died must never read as a pause.
    let chunk_token = job.token.child_token();
    // M1: a plain `Vec`, accumulated in the sequential drain loop below. This
    // used to be an `Arc<Mutex<Vec<i32>>>`, but nothing ever moved it into a
    // spawned task -- the lock guarded a value only one task ever touched.
    let mut completed: Vec<i32> = done;
    let mut set: JoinSet<AppResult<Option<i32>>> = JoinSet::new();

    for chunk in pending {
        if chunk_token.is_cancelled() {
            break;
        }
        // Acquiring before spawning bounds in-flight chunks: the loop blocks
        // here until an earlier chunk releases its permit.
        let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
            break;
        };
        let provider = Arc::clone(&provider);
        let partfile = Arc::clone(&partfile);
        let bucket = job.bucket.clone();
        let key = job.key.clone();
        let token = chunk_token.clone();
        let progress = Arc::clone(&job.progress);

        set.spawn(async move {
            let _permit = permit;
            download_one_chunk(provider, &bucket, &key, chunk, &partfile, &token, progress).await
        });
    }

    let mut first_error: Option<AppError> = None;
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(Some(number))) => completed.push(number),
            // `None` = the chunk observed cancellation and bailed cleanly.
            Ok(Ok(None)) => {}
            Ok(Err(err)) => {
                if first_error.is_none() {
                    first_error = Some(err);
                    chunk_token.cancel();
                }
            }
            Err(join_err) => {
                if first_error.is_none() {
                    first_error = Some(AppError::Internal {
                        message: format!("download chunk task panicked: {join_err}"),
                    });
                    chunk_token.cancel();
                }
            }
        }
    }

    // Persist what landed before deciding anything, so a pause, a permanent
    // failure and a later retry all resume from the same place.
    *job.resume.lock().await = Some(ResumeState::Download(DownloadState {
        etag: None,
        completed_parts: completed.clone(),
        bcpart: bcpart.clone(),
    }));

    if let Some(err) = first_error {
        return Err(err);
    }

    // A user-requested stop wins over "all chunks happened to finish": cancel
    // discards the .bcpart, pause keeps it.
    match job.stopped() {
        Some(StopKind::Cancel) => {
            // Drop the Arc so the file handle closes before we delete it.
            drop(partfile);
            // Best effort: a failed delete must not turn the cancellation into a
            // failed task, but it belongs in the log -- a leaked `.bcpart` keeps
            // costing local disk until something reaps it.
            if let Err(err) = std::fs::remove_file(&bcpart) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        task = %job.task_id,
                        path = %bcpart.display(),
                        "removing .bcpart on cancel failed: {err}"
                    );
                }
            }
            *job.resume.lock().await = None;
            return Ok(RunOutcome::Stopped);
        }
        Some(StopKind::Pause) => return Ok(RunOutcome::Stopped),
        None => {}
    }

    // M3: belt-and-suspenders. Nothing above explains a short list, and
    // renaming one into place would present a truncated file as a complete
    // download the user cannot tell from a good one. Fail loudly rather than
    // silently corrupt.
    if completed.len() != chunks.len() {
        return Err(AppError::Internal {
            message: format!(
                "download of {} finished with {} of {} chunks and no error to explain the gap",
                job.key,
                completed.len(),
                chunks.len()
            ),
        });
    }

    // Every chunk landed and no stop was requested: rename into place.
    // `Completed` is returned only after this succeeds.
    Arc::try_unwrap(partfile)
        .map_err(|_| AppError::Internal {
            message: "download finished with outstanding chunk handles".to_string(),
        })?
        .finish()?;
    *job.resume.lock().await = None;
    Ok(RunOutcome::Completed)
}

async fn download_one_chunk<P>(
    provider: Arc<P>,
    bucket: &str,
    key: &str,
    chunk: PartSpec,
    partfile: &PartFile,
    token: &CancellationToken,
    progress: ProgressFn,
) -> AppResult<Option<i32>>
where
    P: Provider + Send + Sync + 'static,
{
    let bytes = with_retry(token, || {
        let provider = Arc::clone(&provider);
        let bucket = bucket.to_string();
        let key = key.to_string();
        async move {
            provider
                .get_range(&bucket, &key, chunk.offset, chunk.length)
                .await
        }
    })
    .await?;

    // `None` = the token fired mid-request; the bytes were never observed, so
    // nothing may be written or reported.
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    partfile.write_at(chunk.offset, &bytes)?;
    progress(chunk.length);
    Ok(Some(chunk.number))
}

/// Runs `op` with the retry policy from `transfer::retry`, giving up early if
/// `token` fires. `Ok(None)` means "stopped, not failed" -- the caller must
/// not treat it as success *or* as an error.
///
/// Deliberately duplicated from `upload.rs` rather than shared: this task's
/// scope is limited to `download.rs`, and extracting a `pub(crate)` helper
/// would have to edit `retry.rs` and `upload.rs`'s call site too. The policy is
/// network-only, 1s/2s/4s.
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
                tracing::warn!(retry = retries, ?delay, "retrying download step: {err}");
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

    use crate::provider::{BatchResult, Bucket, ListPage, ObjectHead, UploadedPart};
    use crate::transfer::partfile::bcpart_path;

    const MB: u64 = 1024 * 1024;

    fn spec(number: i32) -> PartSpec {
        PartSpec {
            number,
            offset: (number as u64 - 1) * 8,
            length: 8,
        }
    }

    #[test]
    fn nothing_done_means_every_chunk_pending() {
        let plan = [spec(1), spec(2), spec(3)];
        assert_eq!(pending_chunks(&plan, &[]), plan.to_vec());
    }

    #[test]
    fn completed_chunks_are_skipped_on_resume() {
        let plan = [spec(1), spec(2), spec(3), spec(4)];
        let got = pending_chunks(&plan, &[1, 3]);
        assert_eq!(got.iter().map(|c| c.number).collect::<Vec<_>>(), vec![2, 4]);
    }

    #[test]
    fn a_fully_done_plan_leaves_nothing() {
        let plan = [spec(1), spec(2)];
        assert!(pending_chunks(&plan, &[1, 2]).is_empty());
    }

    #[test]
    fn stale_completions_outside_the_plan_are_ignored() {
        let plan = [spec(1), spec(2)];
        assert_eq!(
            pending_chunks(&plan, &[99])
                .iter()
                .map(|c| c.number)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    // ---- The runner itself -------------------------------------------------
    //
    // A wrong `RunOutcome` is the one failure mode nothing downstream can
    // detect (the engine files `Completed` for a file that may not exist), so
    // the outcome of every path below is asserted directly, alongside the
    // on-disk residue (`.bcpart`, target file) the contract promises.

    /// A distinctive, long-period byte pattern, so a chunk written at the wrong
    /// offset is caught by the byte-equality assertions rather than hidden by a
    /// short-period fill that repeats across chunk boundaries. (xorshift64.)
    fn make_object(size: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(size);
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..size {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            v.push(state as u8);
        }
        v
    }

    /// Which failure a fake range read hands back. `AppError` is not `Clone`,
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

    /// Serves ranges of an in-memory "remote object" and records what the
    /// runner asked for. Can be told to fail specific chunks, or to fire the
    /// stop switch when a given chunk is fetched.
    ///
    /// Only the transfer-plane reads a download performs are implemented;
    /// every admin-plane method panics, because reaching one from a download
    /// says the runner is doing something it never should more clearly than a
    /// stub that quietly succeeds.
    struct FakeProvider {
        switch: Arc<Switch>,
        object: Vec<u8>,
        /// Chunk offset -> 1-based chunk number, built from the same plan the
        /// runner uses, so failures and call counts can be addressed by chunk.
        offset_to_number: HashMap<u64, i32>,
        heads: AtomicUsize,
        /// Chunk number -> `get_range` calls, retries included -- which is what
        /// makes the retry budget and resume-skips observable.
        calls: StdMutex<HashMap<i32, usize>>,
        live: AtomicUsize,
        peak: AtomicUsize,
        /// Chunk number -> (attempts still to fail, how).
        failures: StdMutex<HashMap<i32, (u32, Fail)>>,
        /// Fire the stop switch when this chunk's range is fetched -- this is
        /// how a user's pause/cancel is made to land in the middle of a run.
        stop_at_chunk: StdMutex<Option<(i32, StopKind)>>,
        /// How long one range read takes. Non-zero makes concurrency observable
        /// and makes a read abandonable while it is still in flight.
        op_delay: Duration,
    }

    impl FakeProvider {
        fn new(switch: Arc<Switch>, object: Vec<u8>) -> Self {
            let mut offset_to_number = HashMap::new();
            match plan_upload(object.len() as u64) {
                UploadPlan::Single { .. } => {
                    offset_to_number.insert(0, 1);
                }
                UploadPlan::Multipart { parts, .. } => {
                    for p in parts {
                        offset_to_number.insert(p.offset, p.number);
                    }
                }
            }
            Self {
                switch,
                object,
                offset_to_number,
                heads: AtomicUsize::new(0),
                calls: StdMutex::new(HashMap::new()),
                live: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                failures: StdMutex::new(HashMap::new()),
                stop_at_chunk: StdMutex::new(None),
                op_delay: Duration::ZERO,
            }
        }

        /// How many `get_range` calls (retries included) targeted `chunk`.
        fn calls_of(&self, chunk: i32) -> usize {
            self.calls.lock().unwrap().get(&chunk).copied().unwrap_or(0)
        }

        /// Distinct chunks the runner fetched, ascending.
        fn chunks_seen(&self) -> Vec<i32> {
            let mut v: Vec<i32> = self.calls.lock().unwrap().keys().copied().collect();
            v.sort_unstable();
            v
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        async fn get_range(
            &self,
            _bucket: &str,
            _key: &str,
            offset: u64,
            length: u64,
        ) -> AppResult<Vec<u8>> {
            let number = *self
                .offset_to_number
                .get(&offset)
                .expect("get_range must target a planned chunk offset");
            *self.calls.lock().unwrap().entry(number).or_insert(0) += 1;

            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(live, Ordering::SeqCst);
            if !self.op_delay.is_zero() {
                tokio::time::sleep(self.op_delay).await;
            }
            self.live.fetch_sub(1, Ordering::SeqCst);

            let stop_at = *self.stop_at_chunk.lock().unwrap();
            if let Some((n, kind)) = stop_at {
                if n == number {
                    self.switch.request(kind);
                }
            }

            let failure = {
                let mut failures = self.failures.lock().unwrap();
                match failures.get_mut(&number) {
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

            let start = offset as usize;
            let end = start + length as usize;
            Ok(self.object[start..end].to_vec())
        }

        async fn head_object(&self, _bucket: &str, _key: &str) -> AppResult<ObjectHead> {
            self.heads.fetch_add(1, Ordering::SeqCst);
            Ok(ObjectHead {
                size: self.object.len() as u64,
                etag: None,
                content_type: None,
            })
        }

        async fn test_connection(&self) -> AppResult<()> {
            unimplemented!("a download never tests the connection")
        }

        async fn list_buckets(&self) -> AppResult<Vec<Bucket>> {
            unimplemented!("a download never lists buckets")
        }

        async fn create_bucket(&self, _name: &str) -> AppResult<()> {
            unimplemented!("a download never creates buckets")
        }

        async fn delete_bucket(&self, _name: &str) -> AppResult<()> {
            unimplemented!("a download never deletes buckets")
        }

        async fn list_objects(
            &self,
            _bucket: &str,
            _prefix: &str,
            _token: Option<&str>,
            _max_keys: i32,
        ) -> AppResult<ListPage> {
            unimplemented!("a download never lists objects")
        }

        async fn delete_objects(&self, _bucket: &str, _keys: &[String]) -> AppResult<BatchResult> {
            unimplemented!("a download never deletes objects")
        }

        async fn rename_object(
            &self,
            _bucket: &str,
            _from_key: &str,
            _to_key: &str,
        ) -> AppResult<()> {
            unimplemented!("a download never renames objects")
        }

        async fn create_folder(&self, _bucket: &str, _prefix: &str) -> AppResult<()> {
            unimplemented!("a download never creates folders")
        }

        async fn put_object_from_file(
            &self,
            _bucket: &str,
            _key: &str,
            _path: &Path,
            _length: u64,
        ) -> AppResult<()> {
            unimplemented!("a download never puts objects")
        }

        async fn multipart_init(&self, _bucket: &str, _key: &str) -> AppResult<String> {
            unimplemented!("a download never starts a multipart upload")
        }

        #[allow(clippy::too_many_arguments)]
        async fn upload_part_from_file(
            &self,
            _bucket: &str,
            _key: &str,
            _upload_id: &str,
            _part_number: i32,
            _path: &Path,
            _offset: u64,
            _length: u64,
        ) -> AppResult<String> {
            unimplemented!("a download never uploads parts")
        }

        async fn multipart_complete(
            &self,
            _bucket: &str,
            _key: &str,
            _upload_id: &str,
            _parts: &[UploadedPart],
        ) -> AppResult<()> {
            unimplemented!("a download never completes a multipart upload")
        }

        async fn multipart_abort(
            &self,
            _bucket: &str,
            _key: &str,
            _upload_id: &str,
        ) -> AppResult<()> {
            unimplemented!("a download never aborts a multipart upload")
        }

        async fn list_objects_flat(
            &self,
            _bucket: &str,
            _prefix: &str,
            _token: Option<&str>,
            _max_keys: i32,
        ) -> AppResult<ListPage> {
            unimplemented!("a download never lists objects")
        }
    }

    struct Rig {
        switch: Arc<Switch>,
        provider: Arc<FakeProvider>,
        resume: Arc<Mutex<Option<ResumeState>>>,
        reported: Arc<AtomicU64>,
        part_limit: usize,
        // Kept for its RAII lifetime: the target lives under this tempdir, so
        // dropping it early would delete the file mid-test.
        _dir: tempfile::TempDir,
        target: PathBuf,
    }

    fn rig(part_limit: usize, object: Vec<u8>, configure: impl FnOnce(&mut FakeProvider)) -> Rig {
        let switch = Switch::new();
        let mut provider = FakeProvider::new(Arc::clone(&switch), object);
        configure(&mut provider);
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.bin");
        Rig {
            switch,
            provider: Arc::new(provider),
            resume: Arc::new(Mutex::new(None)),
            reported: Arc::new(AtomicU64::new(0)),
            part_limit,
            _dir: dir,
            target,
        }
    }

    impl Rig {
        fn job(&self) -> DownloadJob {
            let switch = Arc::clone(&self.switch);
            let reported = Arc::clone(&self.reported);
            DownloadJob {
                task_id: "task-1".to_string(),
                bucket: "b".to_string(),
                key: "k".to_string(),
                target: self.target.clone(),
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
            run_download(&self.job(), Arc::clone(&self.provider), total).await
        }

        async fn seed_resume(&self, state: DownloadState) {
            *self.resume.lock().await = Some(ResumeState::Download(state));
        }

        async fn resume_state(&self) -> Option<DownloadState> {
            match self.resume.lock().await.clone() {
                Some(ResumeState::Download(ds)) => Some(ds),
                _ => None,
            }
        }

        fn reported(&self) -> u64 {
            self.reported.load(Ordering::SeqCst)
        }

        fn bcpart(&self) -> PathBuf {
            bcpart_path(&self.target)
        }

        fn read_target(&self) -> Vec<u8> {
            std::fs::read(&self.target).unwrap()
        }
    }

    /// The multipart plan for `total`, or a panic if `total` is single-stream.
    fn multipart_plan(total: u64) -> Vec<PartSpec> {
        match plan_upload(total) {
            UploadPlan::Multipart { parts, .. } => parts,
            UploadPlan::Single { .. } => panic!("expected a multipart plan for {total}"),
        }
    }

    // Test 1: a multi-chunk download assembles the exact source bytes (the
    // unit-level analog of the hash round-trip).
    #[tokio::test]
    async fn a_multi_chunk_download_assembles_the_exact_source_bytes() {
        let object = make_object((17 * MB) as usize);
        let rig = rig(4, object.clone(), |_| {});

        let outcome = rig.run(object.len() as u64).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.chunks_seen(),
            vec![1, 2, 3],
            "17MB is three chunks (8MB + 8MB + 1MB)"
        );
        assert_eq!(
            rig.read_target(),
            object,
            "the reassembled file must be byte-for-byte the source object"
        );
        assert_eq!(rig.reported(), object.len() as u64);
        assert!(rig.resume_state().await.is_none());
    }

    // Test 2 (positive half of the RunOutcome contract): `Completed` is only
    // returned once the staging file has been renamed into place. The
    // discriminator is "return Completed before finish()", which leaves the
    // target absent even though the outcome is Completed -- caught here by the
    // on-disk assertions, not by the outcome alone.
    #[tokio::test]
    async fn completed_is_returned_only_after_the_file_is_renamed_into_place() {
        let object = make_object((16 * MB) as usize);
        let rig = rig(4, object.clone(), |_| {});

        let outcome = rig.run(object.len() as u64).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert!(
            rig.target.exists(),
            "a Completed download must have the file on disk, not a stray .bcpart"
        );
        assert!(
            !rig.bcpart().exists(),
            "finish renames the staging file away"
        );
        assert_eq!(rig.read_target(), object);
        assert!(
            rig.resume_state().await.is_none(),
            "a finished download leaves nothing to resume"
        );
    }

    // Test 2 (negative half): a permanent failure on one chunk fails the task
    // (returns Err) and never reports Completed, leaving no target file.
    #[tokio::test]
    async fn a_permanent_chunk_failure_fails_the_task_and_never_completes() {
        let object = make_object((16 * MB) as usize);
        let rig = rig(1, object.clone(), |fake| {
            fake.failures
                .lock()
                .unwrap()
                .insert(1, (u32::MAX, Fail::Permanent));
        });

        let err = rig.run(object.len() as u64).await.unwrap_err();

        assert_eq!(err.code(), "auth/access-denied");
        assert!(
            !rig.target.exists(),
            "a failed download must never rename a partial file into place"
        );
        assert_eq!(
            rig.provider.calls_of(1),
            1,
            "a non-network error fails identically next time; retrying only delays the feedback"
        );
        let state = rig
            .resume_state()
            .await
            .expect("a failure keeps the .bcpart + finished chunks for a retry");
        assert_eq!(state.bcpart, rig.bcpart());
    }

    // Test 3: cancelling mid-run deletes the `.bcpart` and never produces the
    // target. The cancel is fired from inside the first chunk's range read (with
    // part_limit 1, chunks are sequential), so it provably lands mid-run.
    #[tokio::test]
    async fn cancelling_a_download_deletes_the_bcpart_and_leaves_no_target() {
        let object = make_object((16 * MB) as usize);
        let rig = rig(1, object.clone(), |fake| {
            *fake.stop_at_chunk.lock().unwrap() = Some((1, StopKind::Cancel));
        });

        let outcome = rig.run(object.len() as u64).await.unwrap();

        assert_eq!(outcome, RunOutcome::Stopped);
        assert_eq!(
            rig.provider.calls_of(1),
            1,
            "the cancel landed after chunk 1 was fetched -- this is a mid-run stop, not a pre-run one"
        );
        assert!(
            !rig.bcpart().exists(),
            "a cancel discards the staging file -- there is nothing to continue"
        );
        assert!(
            !rig.target.exists(),
            "a cancelled download must never produce the target file"
        );
        assert!(
            rig.resume_state().await.is_none(),
            "a cancel clears the resume slot"
        );
    }

    // Test 4: pausing mid-run keeps the `.bcpart` and records the finished
    // chunk numbers so the resume reopens the same staging file.
    #[tokio::test]
    async fn pausing_a_download_keeps_the_bcpart_and_the_finished_chunk_numbers() {
        let object = make_object((16 * MB) as usize);
        let rig = rig(1, object.clone(), |fake| {
            *fake.stop_at_chunk.lock().unwrap() = Some((1, StopKind::Pause));
        });

        let outcome = rig.run(object.len() as u64).await.unwrap();

        assert_eq!(outcome, RunOutcome::Stopped);
        assert!(
            rig.bcpart().exists(),
            "a pause keeps the staging file so the resume can reopen it"
        );
        assert!(
            !rig.target.exists(),
            "a pause has not finished the download"
        );
        let state = rig
            .resume_state()
            .await
            .expect("a pause must leave resume state behind");
        assert_eq!(
            state.completed_parts,
            vec![1],
            "the chunk that landed before the pause is recorded for the resume"
        );
        assert_eq!(state.bcpart, rig.bcpart());
    }

    // Test 5: a resume reopens the pre-populated `.bcpart` and does NOT
    // re-download the chunks already recorded as complete. The discriminator is
    // "ignore completed_parts on resume", which re-fetches chunk 1 -- caught by
    // its call count being non-zero.
    #[tokio::test]
    async fn resuming_reuses_the_bcpart_and_skips_completed_chunks() {
        let object = make_object((16 * MB) as usize);
        let total = object.len() as u64;
        let rig = rig(4, object.clone(), |_| {});

        // Pre-stage: chunk 1 already sits in the .bcpart on disk.
        let plan = multipart_plan(total);
        let c1 = plan[0];
        {
            let pf = PartFile::create(&rig.target, total).unwrap();
            pf.write_at(
                c1.offset,
                &object[c1.offset as usize..(c1.offset + c1.length) as usize],
            )
            .unwrap();
            // Dropped without finish(): the .bcpart stays on disk holding chunk 1.
        }
        rig.seed_resume(DownloadState {
            etag: None,
            completed_parts: vec![1],
            bcpart: rig.bcpart(),
        })
        .await;

        let outcome = rig.run(total).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.calls_of(1),
            0,
            "chunk 1 was already in the .bcpart; re-downloading it wastes bandwidth and \
             double-counts progress"
        );
        assert_eq!(
            rig.provider.calls_of(2),
            1,
            "chunk 2 was the only one left to fetch"
        );
        assert_eq!(
            rig.read_target(),
            object,
            "the resumed download must still assemble the exact source object"
        );
        assert_eq!(
            rig.reported(),
            total,
            "the engine zeroes transferred on resume, so a resumed run must replay the finished \
             chunk (8MB) plus fetch the rest (8MB) to reach 16MB -- not stall at 8MB"
        );
        assert!(rig.resume_state().await.is_none());
    }

    // Test 6: a 0-byte object downloads to a correct empty local file.
    #[tokio::test]
    async fn a_zero_byte_object_downloads_to_an_empty_file() {
        let rig = rig(4, Vec::new(), |_| {});

        let outcome = rig.run(0).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert!(rig.target.exists(), "the empty file must still be created");
        assert_eq!(rig.read_target(), Vec::<u8>::new());
        assert_eq!(
            rig.provider.calls_of(1),
            1,
            "the single zero-length range is still fetched"
        );
        assert_eq!(rig.reported(), 0);
    }

    // Test 7 (retry within budget): a chunk that fails transiently twice then
    // succeeds still completes. `start_paused` so the backoff sleeps run on the
    // virtual clock rather than costing real seconds.
    #[tokio::test(start_paused = true)]
    async fn a_chunk_that_fails_transiently_within_budget_still_completes() {
        let object = make_object((16 * MB) as usize);
        let rig = rig(4, object.clone(), |fake| {
            fake.failures
                .lock()
                .unwrap()
                .insert(1, (2, Fail::Transient));
        });

        let outcome = rig.run(object.len() as u64).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.calls_of(1),
            3,
            "two timeouts must cost two retries, not the task"
        );
        assert_eq!(rig.read_target(), object);
    }

    // Test 7 (retry budget exhausted): a chunk that fails four times fails the
    // task -- MAX_RETRIES retries after the first attempt.
    #[tokio::test(start_paused = true)]
    async fn a_chunk_that_exhausts_the_retry_budget_fails_the_task() {
        let object = make_object((16 * MB) as usize);
        let rig = rig(4, object.clone(), |fake| {
            fake.failures
                .lock()
                .unwrap()
                .insert(1, (u32::MAX, Fail::Transient));
        });

        let err = rig.run(object.len() as u64).await.unwrap_err();

        assert_eq!(err.code(), "network/timeout");
        assert_eq!(
            rig.provider.calls_of(1),
            MAX_RETRIES as usize + 1,
            "MAX_RETRIES is retries *after* the first attempt"
        );
        assert!(!rig.target.exists(), "an exhausted retry must not finish");
    }

    // Test 8: in-flight range reads never exceed the configured part_limit.
    // 64MB is eight 8MB chunks, so the limit has to be enforced by the spawn
    // loop rather than by there being nothing left to spawn.
    #[tokio::test(start_paused = true)]
    async fn in_flight_chunks_never_exceed_the_part_limit() {
        let object = make_object((64 * MB) as usize);
        let rig = rig(2, object.clone(), |fake| {
            fake.op_delay = Duration::from_millis(10);
        });

        let outcome = rig.run(object.len() as u64).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(rig.provider.chunks_seen().len(), 8);
        assert_eq!(
            rig.provider.peak(),
            2,
            "the semaphore must be acquired before spawning, or all eight chunks go out at once"
        );
    }
}
