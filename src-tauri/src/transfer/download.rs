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
    CheckpointWriter, DownloadState, ResumeState, RunOutcome, StopKind, TaskContext, TransferRunner,
};
use crate::transfer::part::{plan_download, PartSpec, TransferTuning};
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
/// arithmetic is testable without a network. `plan` comes from
/// [`crate::transfer::part::plan_download`], the download-specific planner
/// (`TransferTuning`'s `download_*` fields); it no longer shares S3's
/// multipart-upload thresholds with the upload planner.
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
    /// The tuning snapshot the task was admitted under ([`TaskContext::tuning`],
    /// Task 5). Read once, at `from_context`, not re-read mid-transfer -- a
    /// settings change must not reshape a plan this run has already
    /// committed to.
    tuning: TransferTuning,
    token: CancellationToken,
    stop: StopFn,
    progress: ProgressFn,
    /// Rolls back bytes a failed streaming attempt already reported, before
    /// the retry loop decides to retry or fail (Task 4). Wired to
    /// [`crate::transfer::ProgressHandle::retract`] exactly as `progress` is
    /// wired to `add`.
    regress: ProgressFn,
    resume: Arc<Mutex<Option<ResumeState>>>,
    /// Mirrors the in-memory resume slot to a checkpoint file (M4c). `None`
    /// when checkpointing is disabled, in which case `persist_checkpoint` is a
    /// no-op.
    checkpoint: Option<CheckpointWriter>,
}

impl DownloadJob {
    fn from_context(ctx: &TaskContext) -> Self {
        let control = ctx.control.clone();
        let progress = ctx.progress.clone();
        let regress_handle = ctx.progress.clone();
        Self {
            task_id: ctx.task.id.clone(),
            bucket: ctx.task.bucket.clone(),
            key: ctx.task.key.clone(),
            target: PathBuf::from(&ctx.task.local_path),
            part_limit: ctx.part_limit,
            tuning: ctx.tuning,
            token: ctx.control.token(),
            // `ctx.task` is a snapshot taken before the task went `Running`, so
            // its `status` is stale; the control is the only live source of
            // stop intent.
            stop: Arc::new(move || control.requested()),
            progress: Arc::new(move |bytes| progress.add(bytes)),
            regress: Arc::new(move |bytes| regress_handle.retract(bytes)),
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

    /// Mirrors `state` to the checkpoint file (coalesced/gated/best-effort);
    /// a no-op when checkpointing is disabled. See [`CheckpointWriter`].
    fn persist_checkpoint(&self, state: &ResumeState, force: bool) {
        if let Some(writer) = &self.checkpoint {
            writer.persist(state, force);
        }
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

    let plan = plan_download(total, &job.tuning);
    let part_size = plan.chunk_size;
    let chunks = plan.chunks;

    // Chunking is user-configurable (M6+), so a checkpoint recorded under one
    // tuning cannot be trusted under another: its `completed_parts` chunk
    // numbers would silently map to different byte ranges under *this* plan
    // and corrupt the file. `part_size == 0` (a pre-M6 checkpoint, or one
    // this same check already invalidated) is unconditionally a mismatch.
    // Discard the stale `.bcpart` and fall through to a fresh download rather
    // than trust a resume whose byte ranges no longer line up.
    let existing = existing.and_then(|ds| {
        if ds.part_size != part_size {
            tracing::info!(
                task = %job.task_id,
                key = %job.key,
                recorded_part_size = ds.part_size,
                current_part_size = part_size,
                "recorded part size does not match the current plan; discarding the checkpoint and \
                 restarting the download"
            );
            discard_stale_bcpart(&job.task_id, &ds.bcpart);
            None
        } else {
            Some(ds)
        }
    });

    if chunks.len() == 1 {
        tracing::info!(task = %job.task_id, key = %job.key, "downloading as a single stream");
    } else {
        tracing::info!(
            task = %job.task_id,
            key = %job.key,
            chunks = chunks.len(),
            "downloading as multipart"
        );
    }

    // Stage the `.bcpart` and reconcile against the object (M4c). The order
    // differs by fresh vs resume on purpose:
    //
    // * A **fresh** download preallocates its staging file *before* the head --
    //   a local, network-free step -- so a download reaches the disk before it
    //   touches the network (M4b's property, which the dispatch routing test
    //   leans on), and only then heads the object to record the ETag baseline a
    //   later resume compares against. Task 5 persisted `None` here, which no
    //   resume could trust.
    // * A **resume** heads *first*, because it has to decide whether the
    //   existing `.bcpart` can be trusted before reopening it. The `.bcpart` is
    //   only as good as the object still being the one the download started
    //   against, and the ETag is that proof.
    let (partfile, done, etag): (PartFile, Vec<i32>, Option<String>) = match existing {
        None => {
            let pf = PartFile::create(&job.target, total)?;
            match provider.head_object(&job.bucket, &job.key).await {
                Ok(head) => (pf, Vec::new(), head.etag),
                Err(err) => {
                    // A retryable failure re-runs the whole download, which
                    // reuses this same freshly-created `.bcpart`, so keep it. A
                    // permanent one (a 404, an auth error) can never succeed, so
                    // don't leave a stray, untracked staging file behind.
                    if !is_retryable(&err) {
                        pf.abort();
                    }
                    return Err(err);
                }
            }
        }
        Some(ds) => match provider.head_object(&job.bucket, &job.key).await {
            Ok(head) => {
                // Continue only if the object is provably unchanged. A changed
                // ETag -- or no baseline ETag to trust (an in-memory state
                // predating the field, or a head that returned none) -- means
                // the `.bcpart` may not match the current object, so discard it
                // and restart from scratch under the current ETag. A mismatched
                // partial must never be presented as a resumed download.
                if ds.etag.is_some() && ds.etag == head.etag {
                    match PartFile::reopen(&job.target, total, &ds.bcpart) {
                        Ok(pf) => (pf, ds.completed_parts, head.etag),
                        Err(err) => {
                            // The `.bcpart` vanished while paused (an OS temp
                            // sweep, a manual cleanup). Retry never clears the
                            // resume slot, so a hard `reopen(...)?` here would
                            // re-fail on every attempt and strand the task at
                            // Failed with no way to restart. A missing staging
                            // file means the previously-finished chunks' bytes
                            // are gone too, so the only recovery is a clean
                            // restart from an empty `done`. A genuine create
                            // failure below is still a real error (the `?`).
                            tracing::warn!(
                                task = %job.task_id,
                                "reopening .bcpart failed ({err}); restarting the download from scratch"
                            );
                            (PartFile::create(&job.target, total)?, Vec::new(), head.etag)
                        }
                    }
                } else {
                    tracing::info!(
                        task = %job.task_id,
                        key = %job.key,
                        "object changed under the resume; discarding the .bcpart and restarting the download"
                    );
                    discard_stale_bcpart(&job.task_id, &ds.bcpart);
                    (PartFile::create(&job.target, total)?, Vec::new(), head.etag)
                }
            }
            Err(err) => {
                // The head failed on a resume. A definitive 404 means the object
                // is gone: the partial can never become a correct copy of it, so
                // discard it and surface the failure. A transient head failure
                // (timeout, unreachable) must leave the `.bcpart` intact so the
                // engine's retry can reconcile again -- throwing away good bytes
                // on a blip would be its own bug.
                if matches!(err, AppError::KeyNotFound { .. }) {
                    tracing::info!(
                        task = %job.task_id,
                        key = %job.key,
                        "object gone under the resume; discarding the .bcpart"
                    );
                    discard_stale_bcpart(&job.task_id, &ds.bcpart);
                }
                return Err(err);
            }
        },
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
    let init_state = ResumeState::Download(DownloadState {
        etag: etag.clone(),
        completed_parts: done.clone(),
        bcpart: bcpart.clone(),
        part_size,
    });
    *job.resume.lock().await = Some(init_state.clone());
    // The first set, before any chunk lands: `force` so a cancel-before-any-
    // chunk can read the `.bcpart` it must delete even after a restart.
    job.persist_checkpoint(&init_state, true);

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
        let regress = Arc::clone(&job.regress);

        set.spawn(async move {
            let _permit = permit;
            download_one_chunk(
                provider, &bucket, &key, chunk, &partfile, &token, progress, regress,
            )
            .await
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
    let landed_state = ResumeState::Download(DownloadState {
        etag: etag.clone(),
        completed_parts: completed.clone(),
        bcpart: bcpart.clone(),
        part_size,
    });
    *job.resume.lock().await = Some(landed_state.clone());
    // A per-chunk checkpoint refresh: coalesced, so a download that finishes
    // many chunks in a burst writes a bounded number of files, not one each.
    job.persist_checkpoint(&landed_state, false);

    // Read the stop intent once. Anything arriving after this line arrived
    // during the finish, and the contract is explicit that a real completion
    // wins that race. A user-requested stop also wins over "all chunks happened
    // to finish": cancel discards the .bcpart, pause keeps it.
    let stop = job.stopped();

    if stop == Some(StopKind::Cancel) {
        // Deliberately ahead of `first_error`, mirroring upload's ordering. An
        // error makes the engine file the task `Failed`, and cancelling a
        // `Failed` task never reaches a runner again -- so if the error won
        // here, deleting the `.bcpart` would be the cleanup nobody ever
        // performs and the staging file would leak on local disk forever
        // (`clear_finished` does not reap it). The error is worth less than the
        // cleanup: the user asked for this download to go away.
        //
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

    if let Some(err) = first_error {
        return Err(err);
    }

    if stop.is_some() {
        // A pause. The `.bcpart` and the finished chunk numbers stay in the
        // resume slot (persisted above) so the next run reopens the same
        // staging file instead of re-downloading from zero. `force` the
        // checkpoint so the pause point is current even if the per-chunk write
        // above was coalesced away.
        job.persist_checkpoint(&landed_state, true);
        return Ok(RunOutcome::Stopped);
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

/// Best-effort removal of a `.bcpart` whose object changed or vanished under a
/// resume (M4c). A failed delete must not turn the reconcile into a failed
/// task, but it belongs in the log -- a leaked `.bcpart` keeps costing local
/// disk until something reaps it. Mirrors the cancel branch's cleanup.
fn discard_stale_bcpart(task_id: &str, bcpart: &std::path::Path) {
    if let Err(err) = std::fs::remove_file(bcpart) {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                task = %task_id,
                path = %bcpart.display(),
                "removing stale .bcpart on resume mismatch failed: {err}"
            );
        }
    }
}

/// How much of a chunk is read into memory at once. Chunk sizes run
/// 32-256MB (Tasks 1-3); buffering a whole chunk in RAM the way the old
/// `get_range`-based runner did would mean multi-hundred-MB spikes with
/// several chunks in flight (`part_limit` concurrent chunks). 8MB bounds the
/// memory a single chunk's read loop ever holds, independent of chunk size.
const STREAM_BUF: usize = 8 * 1024 * 1024;

/// Downloads one chunk with manual retry, replacing the old single-shot
/// `get_range` + `with_retry` pair (see `upload.rs`'s `with_retry`, whose doc
/// comment explains why the two runners keep separate copies of the retry
/// loop). This one cannot reuse that generic helper: [`stream_chunk_once`]
/// reports bytes to the progress bar incrementally as they land, so a failed
/// attempt -- one that already streamed and reported part of the chunk before
/// erroring -- must roll that partial report back with `regress` *before* the
/// loop decides whether to retry or give up, or a retried chunk would
/// permanently over-report and the bar would never reach 100% even once every
/// chunk truly lands.
#[allow(clippy::too_many_arguments)]
async fn download_one_chunk<P>(
    provider: Arc<P>,
    bucket: &str,
    key: &str,
    chunk: PartSpec,
    partfile: &PartFile,
    token: &CancellationToken,
    progress: ProgressFn,
    regress: ProgressFn,
) -> AppResult<Option<i32>>
where
    P: Provider + Send + Sync + 'static,
{
    let mut retries = 0u32;
    loop {
        if token.is_cancelled() {
            return Ok(None);
        }
        let (err, reported) =
            match stream_chunk_once(&provider, bucket, key, chunk, partfile, token, &progress)
                .await
            {
                Ok(Some(written)) if written == chunk.length => return Ok(Some(chunk.number)),
                // The token fired mid-read; the reported bytes stay reported
                // (pause semantics: the resume path re-reports finished
                // chunks only, so nothing here needs undoing) and nothing
                // more may be written.
                Ok(None) => return Ok(None),
                // A short stream (the object shrank between head and get, or
                // a non-conformant gateway EOF'd early): same failure shape as
                // the old whole-chunk length check, just discovered
                // incrementally instead of in one length comparison.
                Ok(Some(short)) => (
                    AppError::Internal {
                        message: format!(
                            "range read for {key} chunk {} returned {short} bytes, expected {}",
                            chunk.number, chunk.length
                        ),
                    },
                    short,
                ),
                Err((err, reported)) => (err, reported),
            };
        // Conservation before the retry/fail decision: whatever this attempt
        // reported must come back off the bar first, whether the attempt is
        // about to be retried or is about to fail the whole task.
        regress(reported);
        retries += 1;
        if !is_retryable(&err) || retries > MAX_RETRIES {
            return Err(err);
        }
        let delay = backoff_delay(retries);
        tracing::warn!(retry = retries, ?delay, "retrying download chunk: {err}");
        tokio::select! {
            _ = token.cancelled() => return Ok(None),
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// One streaming attempt at `chunk`: opens the range, reads it through a
/// bounded buffer, writes each piece to `partfile` and reports it as it
/// lands. `Ok(Some(n))` with `n < chunk.length` is a short stream (the
/// caller turns it into the same-shaped error the old length check produced);
/// `Ok(Some(chunk.length))` is a complete chunk; `Ok(None)` is a clean
/// cancellation. `Err` carries the bytes *this attempt* already reported, so
/// the caller can retract exactly that before deciding whether to retry.
async fn stream_chunk_once<P>(
    provider: &Arc<P>,
    bucket: &str,
    key: &str,
    chunk: PartSpec,
    partfile: &PartFile,
    token: &CancellationToken,
    progress: &ProgressFn,
) -> Result<Option<u64>, (AppError, u64)>
where
    P: Provider + Send + Sync + 'static,
{
    let mut reader = provider
        .open_range(bucket, key, chunk.offset, chunk.length)
        .await
        .map_err(|err| (err, 0))?;
    let mut written = 0u64;
    // `chunk.length.max(1)` so a 0-byte chunk (the 0-byte-object case) still
    // allocates a non-empty buffer; the read loop below never runs for it
    // anyway since `written < chunk.length` is immediately false.
    let mut buf = vec![0u8; STREAM_BUF.min(chunk.length.max(1) as usize)];
    while written < chunk.length {
        // Cap the slice to what's left of the chunk: a non-conformant gateway
        // that hands back more than it was asked for must not be allowed to
        // write past `chunk.length` into the next chunk's bytes.
        let remaining = (chunk.length - written) as usize;
        let cap = buf.len().min(remaining);
        let n = tokio::select! {
            biased;
            _ = token.cancelled() => return Ok(None),
            r = tokio::io::AsyncReadExt::read(&mut reader, &mut buf[..cap]) => r,
        }
        .map_err(|err| {
            (
                AppError::Internal {
                    message: format!("range stream read for {key} chunk {}: {err}", chunk.number),
                },
                written,
            )
        })?;
        if n == 0 {
            // EOF before the chunk was fully read -- a short stream. The
            // caller builds the error; this attempt's already-reported bytes
            // are `written`.
            return Ok(Some(written));
        }
        partfile
            .write_at(chunk.offset + written, &buf[..n])
            .map_err(|err| (err, written))?;
        written += n as u64;
        progress(n as u64);
    }
    Ok(Some(written))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use crate::provider::{BatchResult, Bucket, ListPage, ObjectHead, UploadedPart};
    use crate::transfer::partfile::bcpart_path;

    const MB: u64 = 1024 * 1024;

    /// The ETag the fake's `head_object` reports by default, and the baseline a
    /// resume test seeds into its `DownloadState`. Kept in one place so an
    /// "unchanged" resume can assert continue and a "changed" resume can flip
    /// the head to something else.
    const OBJECT_ETAG: &str = "etag-v1";

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
        /// Chunk number -> `open_range` calls, retries included -- which is
        /// what makes the retry budget and resume-skips observable.
        calls: StdMutex<HashMap<i32, usize>>,
        live: AtomicUsize,
        peak: AtomicUsize,
        /// Chunk number -> (attempts still to fail, how).
        failures: StdMutex<HashMap<i32, (u32, Fail)>>,
        /// Fire the stop switch when this chunk's range is fetched -- this is
        /// how a user's pause/cancel is made to land in the middle of a run.
        stop_at_chunk: StdMutex<Option<(i32, StopKind)>>,
        /// Return only this many bytes for the named chunk instead of the full
        /// requested length -- a short/partial range response, as from an
        /// object that shrank between head and get or a non-conformant gateway.
        short_chunk: StdMutex<Option<(i32, u64)>>,
        /// How long one range read takes. Non-zero makes concurrency observable
        /// and makes a read abandonable while it is still in flight.
        op_delay: Duration,
        /// The ETag `head_object` reports. `Some(OBJECT_ETAG)` by default so a
        /// fresh download stores a real baseline and an unchanged resume's
        /// stored ETag matches; a test flips it to make "current" differ from a
        /// checkpoint's stored ETag -- i.e. the object changed under the resume.
        head_etag: StdMutex<Option<String>>,
        /// When set, `head_object` fails with `storage/key-not-found`, as if the
        /// object had been deleted between the checkpoint and the resume (a 404).
        head_missing: AtomicBool,
    }

    impl FakeProvider {
        fn new(switch: Arc<Switch>, object: Vec<u8>) -> Self {
            let mut offset_to_number = HashMap::new();
            for p in download_chunks(object.len() as u64) {
                offset_to_number.insert(p.offset, p.number);
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
                short_chunk: StdMutex::new(None),
                op_delay: Duration::ZERO,
                head_etag: StdMutex::new(Some(OBJECT_ETAG.to_string())),
                head_missing: AtomicBool::new(false),
            }
        }

        /// How many `open_range` calls (retries included) targeted `chunk`.
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
        async fn open_range(
            &self,
            _bucket: &str,
            _key: &str,
            offset: u64,
            length: u64,
        ) -> AppResult<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
            let number = *self
                .offset_to_number
                .get(&offset)
                .expect("open_range must target a planned chunk offset");
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
            let mut bytes = self.object[start..end].to_vec();
            // Simulate a short/partial range response for the named chunk: the
            // returned stream EOFs after `keep` bytes instead of the full
            // `length` -- `std::io::Cursor` naturally reads Ok(0) once its
            // backing `Vec` is exhausted, so truncating it here is enough to
            // make `stream_chunk_once`'s read loop observe a short stream.
            if let Some((n, keep)) = *self.short_chunk.lock().unwrap() {
                if n == number {
                    bytes.truncate(keep as usize);
                }
            }
            Ok(Box::new(std::io::Cursor::new(bytes)))
        }

        async fn head_object(&self, _bucket: &str, _key: &str) -> AppResult<ObjectHead> {
            self.heads.fetch_add(1, Ordering::SeqCst);
            if self.head_missing.load(Ordering::SeqCst) {
                return Err(AppError::KeyNotFound {
                    key: "k".to_string(),
                });
            }
            Ok(ObjectHead {
                size: self.object.len() as u64,
                etag: self.head_etag.lock().unwrap().clone(),
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

        async fn delete_prefix(&self, _bucket: &str, _prefix: &str) -> AppResult<BatchResult> {
            unimplemented!("a download never deletes prefixes")
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

        async fn multipart_list(
            &self,
            _bucket: &str,
            _key: &str,
            _upload_id: &str,
        ) -> AppResult<Vec<UploadedPart>> {
            unimplemented!("a download never lists multipart parts")
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

        async fn presign_get(
            &self,
            _bucket: &str,
            _key: &str,
            _expires_secs: u64,
        ) -> AppResult<String> {
            unimplemented!("a download never presigns a share url")
        }
    }

    struct Rig {
        switch: Arc<Switch>,
        provider: Arc<FakeProvider>,
        resume: Arc<Mutex<Option<ResumeState>>>,
        reported: Arc<AtomicU64>,
        /// Cumulative bytes `regress` has retracted -- gross, like `reported`,
        /// so `reported() - regressed()` is the net figure the real progress
        /// bar would show (Task 4).
        regressed: Arc<AtomicU64>,
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
            regressed: Arc::new(AtomicU64::new(0)),
            part_limit,
            _dir: dir,
            target,
        }
    }

    impl Rig {
        fn job(&self) -> DownloadJob {
            let switch = Arc::clone(&self.switch);
            let reported = Arc::clone(&self.reported);
            let regressed = Arc::clone(&self.regressed);
            DownloadJob {
                task_id: "task-1".to_string(),
                bucket: "b".to_string(),
                key: "k".to_string(),
                target: self.target.clone(),
                part_limit: self.part_limit,
                tuning: TransferTuning::default(),
                token: self.switch.token.clone(),
                stop: Arc::new(move || switch.requested()),
                progress: Arc::new(move |bytes| {
                    reported.fetch_add(bytes, Ordering::SeqCst);
                }),
                regress: Arc::new(move |bytes| {
                    regressed.fetch_add(bytes, Ordering::SeqCst);
                }),
                resume: Arc::clone(&self.resume),
                // The runner's decision logic is under test here, not the
                // checkpoint mirror; disabling it keeps these tests filesystem-
                // free (the engine tests cover the writer end).
                checkpoint: None,
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

        fn regressed(&self) -> u64 {
            self.regressed.load(Ordering::SeqCst)
        }

        fn bcpart(&self) -> PathBuf {
            bcpart_path(&self.target)
        }

        fn read_target(&self) -> Vec<u8> {
            std::fs::read(&self.target).unwrap()
        }
    }

    /// The download plan's chunk table for `total`, under the default
    /// (balanced) tuning -- what `run_download` itself derives. Works for
    /// both single- and multi-chunk totals; callers that need more than one
    /// chunk assert `download_chunks(total).len() > 1` themselves.
    fn download_chunks(total: u64) -> Vec<PartSpec> {
        plan_download(total, &TransferTuning::default()).chunks
    }

    /// The download plan's `chunk_size` for `total`, under the default
    /// (balanced) tuning -- what a real run records into `DownloadState`.
    /// Tests that seed a resume state expecting it to be *trusted* use this
    /// so the recorded value always agrees with what `run_download` itself
    /// would derive.
    fn download_chunk_size(total: u64) -> u64 {
        plan_download(total, &TransferTuning::default()).chunk_size
    }

    /// The default (balanced) preset's download threshold -- 64MB. Test
    /// sizes below derive from this and `download_floor()` rather than
    /// hardcoding numbers, so a future preset change cannot leave a
    /// silently-wrong multi-chunk test size behind.
    fn download_threshold() -> u64 {
        TransferTuning::default().download_threshold
    }

    /// The balanced preset's download chunk floor -- 32MB, and (not
    /// coincidentally) exactly `download_threshold() / 2`: a total of
    /// `download_threshold()` resolves to two chunks of this size.
    fn download_floor() -> u64 {
        TransferTuning::default().download_chunk_floor
    }

    // Test 1: a multi-chunk download assembles the exact source bytes (the
    // unit-level analog of the hash round-trip).
    #[tokio::test]
    async fn a_multi_chunk_download_assembles_the_exact_source_bytes() {
        // 2 * floor + 1MB is three chunks: two full-floor chunks and a short
        // 1MB remainder.
        let object = make_object((2 * download_floor() + MB) as usize);
        let rig = rig(4, object.clone(), |_| {});

        let outcome = rig.run(object.len() as u64).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.chunks_seen(),
            vec![1, 2, 3],
            "2 * floor + 1MB is three chunks (floor + floor + 1MB)"
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
    //
    // Two chunks (not one), and the pause fires on the *second*: Task 4's
    // streaming `open_range` observes the token before it starts reading a
    // chunk's own bytes (`stream_chunk_once`'s read loop checks
    // `token.cancelled()` ahead of every read), so a chunk whose fetch never
    // got past `open_range` before the pause landed must NOT count as done --
    // unlike the old single-shot `get_range`, where the whole chunk resolved
    // atomically in one call and a pause requested mid-call could not stop it
    // from finishing. Chunk 1, fetched to completion before chunk 2 is even
    // attempted (part_limit 1 makes this sequential), is the one that must
    // survive into the resume state.
    #[tokio::test]
    async fn pausing_a_download_keeps_the_bcpart_and_the_finished_chunk_numbers() {
        let object = make_object(download_threshold() as usize);
        let rig = rig(1, object.clone(), |fake| {
            *fake.stop_at_chunk.lock().unwrap() = Some((2, StopKind::Pause));
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
        assert_eq!(
            rig.provider.calls_of(2),
            1,
            "the pause landed while chunk 2's open_range call fired it -- the read loop must never \
             have reached a read"
        );
        let state = rig
            .resume_state()
            .await
            .expect("a pause must leave resume state behind");
        assert_eq!(
            state.completed_parts,
            vec![1],
            "chunk 1 finished before chunk 2 was even attempted, so only it is recorded for the \
             resume -- chunk 2's own fetch was interrupted by the pause before any of its bytes \
             streamed"
        );
        assert_eq!(state.bcpart, rig.bcpart());
    }

    // Test 5: a resume reopens the pre-populated `.bcpart` and does NOT
    // re-download the chunks already recorded as complete. The discriminator is
    // "ignore completed_parts on resume", which re-fetches chunk 1 -- caught by
    // its call count being non-zero.
    #[tokio::test]
    async fn resuming_reuses_the_bcpart_and_skips_completed_chunks() {
        // At the threshold -> two equal chunks, so chunk 2 is left to fetch.
        let object = make_object(download_threshold() as usize);
        let total = object.len() as u64;
        let rig = rig(4, object.clone(), |_| {});

        // Pre-stage: chunk 1 already sits in the .bcpart on disk.
        let plan = download_chunks(total);
        assert_eq!(plan.len(), 2, "expected exactly two chunks for this test");
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
            // The head still reports OBJECT_ETAG, so this in-session resume is
            // provably against the same object and continues (M4c).
            etag: Some(OBJECT_ETAG.to_string()),
            completed_parts: vec![1],
            bcpart: rig.bcpart(),
            part_size: download_chunk_size(total),
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
        assert_eq!(
            rig.reported(),
            object.len() as u64,
            "the final total must equal the object size exactly once, not double-counted across \
             the two retries"
        );
        assert_eq!(
            rig.regressed(),
            0,
            "these failures happen in open_range, before any byte of the retried chunk is \
             streamed, so there is nothing yet to retract"
        );
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

    // ---- Task 4: streaming chunk download ----------------------------------
    //
    // These exercise `download_one_chunk`/`stream_chunk_once` directly rather
    // than through `Rig`/`run_download`: the property under test is the read
    // loop's own behavior (incremental progress, short-stream detection, and
    // the retry loop's regress-before-retry bookkeeping), not the
    // whole-download state machine the tests above already cover.

    /// An `AsyncRead` that hands out one `segments` entry per `poll_read`
    /// call, never merging several queued segments into a single read the
    /// way `std::io::Cursor` would. That is what lets a test control -- and
    /// observe -- exactly how many times the read loop iterates for one
    /// chunk, which is what proves progress is reported incrementally rather
    /// than once for the whole chunk. Once the queue is empty, further reads
    /// report EOF (`Ok(0)`), so a `segments` total shorter than the chunk
    /// length naturally simulates a short/partial stream -- no separate
    /// "fail" mode is needed for that case.
    struct SegmentedReader {
        segments: std::collections::VecDeque<Vec<u8>>,
        /// Fires this token right after the first segment is handed out, so a
        /// test can land a cancellation deterministically between two read
        /// loop iterations without any real concurrency.
        cancel_after_first: Option<CancellationToken>,
    }

    impl SegmentedReader {
        fn new(segments: Vec<Vec<u8>>) -> Self {
            Self {
                segments: segments.into(),
                cancel_after_first: None,
            }
        }

        fn cancelling_after_first(segments: Vec<Vec<u8>>, token: CancellationToken) -> Self {
            Self {
                segments: segments.into(),
                cancel_after_first: Some(token),
            }
        }
    }

    impl tokio::io::AsyncRead for SegmentedReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if let Some(seg) = self.segments.pop_front() {
                buf.put_slice(&seg);
                if let Some(token) = self.cancel_after_first.take() {
                    token.cancel();
                }
            }
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// What a single `open_range` call should do -- built from the same
    /// [`Fail`] enum the whole-download `FakeProvider` above uses, so a
    /// scripted failure carries the same retryable-vs-not meaning the other
    /// tests in this file rely on.
    enum Attempt {
        Fail(Fail),
        Stream(SegmentedReader),
    }

    /// A minimal [`Provider`] whose `open_range` plays back a fixed script of
    /// [`Attempt`]s, one per call, and panics on every other method -- these
    /// tests call `download_one_chunk`/`stream_chunk_once` directly, so
    /// nothing else is ever reached.
    struct ScriptedProvider {
        calls: AtomicUsize,
        attempts: StdMutex<std::collections::VecDeque<Attempt>>,
    }

    impl ScriptedProvider {
        fn new(attempts: Vec<Attempt>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                attempts: StdMutex::new(attempts.into()),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn open_range(
            &self,
            _bucket: &str,
            _key: &str,
            _offset: u64,
            _length: u64,
        ) -> AppResult<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self
                .attempts
                .lock()
                .unwrap()
                .pop_front()
                .expect("ScriptedProvider ran out of scripted open_range attempts")
            {
                Attempt::Fail(how) => Err(how.error()),
                Attempt::Stream(reader) => Ok(Box::new(reader)),
            }
        }

        async fn test_connection(&self) -> AppResult<()> {
            unimplemented!("not exercised by these tests")
        }
        async fn list_buckets(&self) -> AppResult<Vec<Bucket>> {
            unimplemented!("not exercised by these tests")
        }
        async fn create_bucket(&self, _name: &str) -> AppResult<()> {
            unimplemented!("not exercised by these tests")
        }
        async fn delete_bucket(&self, _name: &str) -> AppResult<()> {
            unimplemented!("not exercised by these tests")
        }
        async fn list_objects(
            &self,
            _bucket: &str,
            _prefix: &str,
            _token: Option<&str>,
            _max_keys: i32,
        ) -> AppResult<ListPage> {
            unimplemented!("not exercised by these tests")
        }
        async fn delete_objects(&self, _bucket: &str, _keys: &[String]) -> AppResult<BatchResult> {
            unimplemented!("not exercised by these tests")
        }
        async fn delete_prefix(&self, _bucket: &str, _prefix: &str) -> AppResult<BatchResult> {
            unimplemented!("not exercised by these tests")
        }
        async fn rename_object(
            &self,
            _bucket: &str,
            _from_key: &str,
            _to_key: &str,
        ) -> AppResult<()> {
            unimplemented!("not exercised by these tests")
        }
        async fn create_folder(&self, _bucket: &str, _prefix: &str) -> AppResult<()> {
            unimplemented!("not exercised by these tests")
        }
        async fn put_object_from_file(
            &self,
            _bucket: &str,
            _key: &str,
            _path: &Path,
            _length: u64,
        ) -> AppResult<()> {
            unimplemented!("not exercised by these tests")
        }
        async fn multipart_init(&self, _bucket: &str, _key: &str) -> AppResult<String> {
            unimplemented!("not exercised by these tests")
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
            unimplemented!("not exercised by these tests")
        }
        async fn multipart_complete(
            &self,
            _bucket: &str,
            _key: &str,
            _upload_id: &str,
            _parts: &[UploadedPart],
        ) -> AppResult<()> {
            unimplemented!("not exercised by these tests")
        }
        async fn multipart_abort(
            &self,
            _bucket: &str,
            _key: &str,
            _upload_id: &str,
        ) -> AppResult<()> {
            unimplemented!("not exercised by these tests")
        }
        async fn multipart_list(
            &self,
            _bucket: &str,
            _key: &str,
            _upload_id: &str,
        ) -> AppResult<Vec<UploadedPart>> {
            unimplemented!("not exercised by these tests")
        }
        async fn head_object(&self, _bucket: &str, _key: &str) -> AppResult<ObjectHead> {
            unimplemented!("not exercised by these tests")
        }
        async fn list_objects_flat(
            &self,
            _bucket: &str,
            _prefix: &str,
            _token: Option<&str>,
            _max_keys: i32,
        ) -> AppResult<ListPage> {
            unimplemented!("not exercised by these tests")
        }
        async fn presign_get(
            &self,
            _bucket: &str,
            _key: &str,
            _expires_secs: u64,
        ) -> AppResult<String> {
            unimplemented!("not exercised by these tests")
        }
    }

    /// A tracking `progress`/`regress` pair: every call appends the byte
    /// count to a shared log rather than just summing it, so a test can
    /// assert the *shape* of the calls (how many, in what order) and not just
    /// the total -- a bare running sum would hide a single big report where
    /// several incremental ones were expected.
    #[allow(clippy::type_complexity)]
    fn tracking_fns() -> (ProgressFn, Arc<StdMutex<Vec<u64>>>, ProgressFn, Arc<StdMutex<Vec<u64>>>) {
        let reported = Arc::new(StdMutex::new(Vec::<u64>::new()));
        let regressed = Arc::new(StdMutex::new(Vec::<u64>::new()));
        let reported_for_closure = Arc::clone(&reported);
        let progress: ProgressFn = Arc::new(move |bytes| reported_for_closure.lock().unwrap().push(bytes));
        let regressed_for_closure = Arc::clone(&regressed);
        let regress: ProgressFn = Arc::new(move |bytes| regressed_for_closure.lock().unwrap().push(bytes));
        (progress, reported, regress, regressed)
    }

    // Task 4 Step 1, test 1: a chunk streamed in several pieces (each smaller
    // than the fixed read buffer) lands byte-for-byte in the `.bcpart`, and
    // progress is reported once per piece rather than once for the whole
    // chunk.
    #[tokio::test]
    async fn streamed_chunk_lands_bytes_and_reports_incremental_progress() {
        let segments = vec![b"abc".to_vec(), b"defgh".to_vec(), b"ij".to_vec()];
        let expected: Vec<u8> = segments.iter().flatten().copied().collect();
        let total = expected.len() as u64;
        let chunk = PartSpec {
            number: 1,
            offset: 0,
            length: total,
        };
        let provider = Arc::new(ScriptedProvider::new(vec![Attempt::Stream(SegmentedReader::new(
            segments,
        ))]));

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.bin");
        let pf = PartFile::create(&target, total).unwrap();
        let token = CancellationToken::new();
        let (progress, reported, regress, regressed) = tracking_fns();

        let result = download_one_chunk(provider, "b", "k", chunk, &pf, &token, progress, regress)
            .await
            .unwrap();

        assert_eq!(result, Some(1));
        pf.finish().unwrap();
        assert_eq!(
            std::fs::read(&target).unwrap(),
            expected,
            "the assembled file must be byte-for-byte the streamed source"
        );
        let calls = reported.lock().unwrap().clone();
        assert!(
            calls.len() > 1,
            "progress must be reported once per read, not once for the whole chunk: {calls:?}"
        );
        assert_eq!(
            calls.iter().sum::<u64>(),
            total,
            "the sum of every incremental report must equal the chunk length"
        );
        assert!(
            regressed.lock().unwrap().is_empty(),
            "a chunk that never failed must never regress"
        );
    }

    // Task 4 Step 1, test 2: a stream that EOFs before the chunk's full
    // length must fail (the same failure shape as the old whole-chunk length
    // check), and every byte it reported on the way there must come back off
    // the bar -- progress conservation, no ghost bytes left behind.
    #[tokio::test]
    async fn short_stream_errors_and_retracts_reported_bytes() {
        let seg_a = vec![1u8; 100];
        let seg_b = vec![2u8; 50];
        let streamed = (seg_a.len() + seg_b.len()) as u64; // 150
        let chunk = PartSpec {
            number: 1,
            offset: 0,
            length: 200, // more than the stream actually provides
        };
        let provider = Arc::new(ScriptedProvider::new(vec![Attempt::Stream(SegmentedReader::new(
            vec![seg_a, seg_b],
        ))]));

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.bin");
        let pf = PartFile::create(&target, chunk.length).unwrap();
        let token = CancellationToken::new();
        let (progress, reported, regress, regressed) = tracking_fns();

        let err = download_one_chunk(Arc::clone(&provider), "b", "k", chunk, &pf, &token, progress, regress)
            .await
            .unwrap_err();

        assert_eq!(
            err.code(),
            "internal",
            "a short stream is a broken-invariant failure, not a storage error"
        );
        assert_eq!(
            provider.calls(),
            1,
            "a short stream is an Internal error, which is never retried"
        );
        pf.abort();

        let reported_total: u64 = reported.lock().unwrap().iter().sum();
        let regressed_total: u64 = regressed.lock().unwrap().iter().sum();
        assert_eq!(
            reported_total, streamed,
            "the bytes actually streamed before EOF must still have been reported incrementally"
        );
        assert_eq!(
            regressed_total, reported_total,
            "every byte this attempt reported must be retracted -- progress conservation"
        );
    }

    // Task 4 Step 1, test 3: `open_range` itself failing (a connection never
    // established, before any bytes flow) is retried under the same
    // network-only policy `transfer::retry` already governs -- and the
    // manual retry loop's regress call runs on that path too, even though
    // there is nothing to undo yet (no reader was ever obtained).
    #[tokio::test(start_paused = true)]
    async fn throttled_open_range_retries_then_succeeds() {
        let object = b"hello world, this is one full chunk of bytes".to_vec();
        let length = object.len() as u64;
        let chunk = PartSpec {
            number: 1,
            offset: 0,
            length,
        };
        let provider = Arc::new(ScriptedProvider::new(vec![
            Attempt::Fail(Fail::Transient),
            Attempt::Stream(SegmentedReader::new(vec![object.clone()])),
        ]));

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.bin");
        let pf = PartFile::create(&target, length).unwrap();
        let token = CancellationToken::new();
        let (progress, reported, regress, regressed) = tracking_fns();

        let result = download_one_chunk(
            Arc::clone(&provider),
            "b",
            "k",
            chunk,
            &pf,
            &token,
            progress,
            regress,
        )
        .await
        .unwrap();

        assert_eq!(result, Some(1));
        pf.finish().unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), object);
        assert_eq!(
            provider.calls(),
            2,
            "one failed open_range plus one that succeeded"
        );
        let reported_total: u64 = reported.lock().unwrap().iter().sum();
        let regressed_total: u64 = regressed.lock().unwrap().iter().sum();
        assert_eq!(
            reported_total, length,
            "the successful retry must report exactly the chunk's bytes once, not double-counted \
             from the failed first attempt"
        );
        assert_eq!(
            regressed_total, 0,
            "the first attempt failed before any bytes were read, so the regress/retry mechanism \
             ran with nothing to undo"
        );
    }

    // Task 4: a cancellation observed mid-read must return `Ok(None)` and
    // leave whatever was already reported in place -- pause semantics, not a
    // failure to roll back. The resume path re-reports finished *chunks*
    // only, so a regressed partial chunk here would just vanish from the bar.
    #[tokio::test]
    async fn cancelling_mid_stream_leaves_already_reported_bytes_in_place() {
        let seg_a = vec![9u8; 100];
        let seg_b = vec![8u8; 100];
        let chunk = PartSpec {
            number: 1,
            offset: 0,
            length: (seg_a.len() + seg_b.len()) as u64,
        };
        let token = CancellationToken::new();
        let reader = SegmentedReader::cancelling_after_first(vec![seg_a.clone(), seg_b], token.clone());
        let provider = Arc::new(ScriptedProvider::new(vec![Attempt::Stream(reader)]));

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.bin");
        let pf = PartFile::create(&target, chunk.length).unwrap();
        let (progress, reported, regress, regressed) = tracking_fns();

        let result = download_one_chunk(provider, "b", "k", chunk, &pf, &token, progress, regress)
            .await
            .unwrap();

        assert!(
            result.is_none(),
            "a cancellation observed mid-read must return Ok(None), not an error or a false success"
        );
        let reported_total: u64 = reported.lock().unwrap().iter().sum();
        assert_eq!(
            reported_total,
            seg_a.len() as u64,
            "the first segment's bytes were reported before the cancel landed"
        );
        assert!(
            regressed.lock().unwrap().is_empty(),
            "pause semantics: a cancellation must NOT retract bytes already reported"
        );
        pf.abort();
    }

    // Test 8: in-flight range reads never exceed the configured part_limit.
    // 4 * floor is four equal chunks, so the limit has to be enforced by the
    // spawn loop rather than by there being nothing left to spawn. A plain
    // zero-filled buffer, not `make_object`: this test never reads the bytes
    // back, only counts chunks and concurrency, so there is nothing to gain
    // from paying for distinctive content at this size.
    #[tokio::test(start_paused = true)]
    async fn in_flight_chunks_never_exceed_the_part_limit() {
        let object = vec![0u8; (4 * download_floor()) as usize];
        let rig = rig(2, object.clone(), |fake| {
            fake.op_delay = Duration::from_millis(10);
        });

        let outcome = rig.run(object.len() as u64).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(rig.provider.chunks_seen().len(), 4);
        assert_eq!(
            rig.provider.peak(),
            2,
            "the semaphore must be acquired before spawning, or all four chunks go out at once"
        );
    }

    // Test 9 (Finding 1): a cancel that races a non-retryable chunk error still
    // wins -- the download is `Stopped` and the `.bcpart` discarded, not
    // `Failed` with a leaked staging file. Modelled on upload.rs's
    // `a_cancel_racing_a_part_failure_still_aborts_the_upload`. Chunk 1's range
    // read fires the cancel *and then* returns a permanent error, so by join
    // time `first_error` is `Some` *and* the stop intent is `Cancel` at once.
    // Cancel must be handled ahead of `first_error`: the engine files an
    // errored download `Failed`, and cancelling a `Failed` task never reaches a
    // runner again, so the `.bcpart`-deleting cancel branch would otherwise be
    // the cleanup nobody ever performs and the staging file would leak forever.
    #[tokio::test]
    async fn a_cancel_racing_a_chunk_failure_still_discards_the_bcpart() {
        let object = make_object((16 * MB) as usize);
        let rig = rig(1, object.clone(), |fake| {
            *fake.stop_at_chunk.lock().unwrap() = Some((1, StopKind::Cancel));
            fake.failures
                .lock()
                .unwrap()
                .insert(1, (u32::MAX, Fail::Permanent));
        });

        let outcome = rig.run(object.len() as u64).await.unwrap();

        assert_eq!(
            outcome,
            RunOutcome::Stopped,
            "cancel must win over a co-occurring non-retryable error"
        );
        assert!(
            !rig.bcpart().exists(),
            "a cancel discards the staging file even when an error raced it -- otherwise the \
             .bcpart leaks forever, since the Failed task never reruns to clean it up"
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

    // Test 10 (Finding 2): a range read that returns fewer bytes than requested
    // must fail the task, not silently leave a zero gap in the preallocated
    // `.bcpart` and rename a corrupt file into place. Without the byte-count
    // assertion the short chunk is marked complete, `finish()` renames, and the
    // download reports `Completed` for a file with a hole in it -- the one hole
    // in "Completed = bytes landed" that the chunk-counting M3 guard cannot see.
    #[tokio::test]
    async fn a_short_range_read_fails_the_task_and_never_completes() {
        let object = make_object((16 * MB) as usize);
        let total = object.len() as u64;
        let short = download_chunks(total)[0].length - 1;
        let rig = rig(4, object.clone(), |fake| {
            *fake.short_chunk.lock().unwrap() = Some((1, short));
        });

        let err = rig.run(total).await.unwrap_err();

        assert_eq!(
            err.code(),
            "internal",
            "a short range read is a broken-invariant failure, not a storage error"
        );
        assert!(
            !rig.target.exists(),
            "a short chunk must never be renamed into place as a complete download"
        );
    }

    // Test 11 (Finding 3): a resume whose `.bcpart` has vanished (an OS temp
    // sweep, a manual cleanup) must fall back to a clean restart rather than
    // fail `reopen` forever. With the old `reopen(...)?` this run errored and,
    // because retry never clears the resume slot, stayed stuck at Failed. The
    // resume records chunk 1 as done, but the missing staging file means its
    // bytes are gone too, so a correct restart re-fetches chunk 1 as well.
    #[tokio::test]
    async fn a_resume_with_a_missing_bcpart_restarts_cleanly() {
        // At the threshold -> two equal chunks, so chunk 2 exists to compare
        // against the re-fetched chunk 1.
        let object = make_object(download_threshold() as usize);
        let total = object.len() as u64;
        let rig = rig(4, object.clone(), |_| {});

        rig.seed_resume(DownloadState {
            // The etag still matches the head, so the resume is trusted and
            // reaches the reopen -- which then hits the missing-.bcpart fallback
            // this test is about, not the M4c etag discard.
            etag: Some(OBJECT_ETAG.to_string()),
            completed_parts: vec![1],
            bcpart: rig.bcpart(),
            part_size: download_chunk_size(total),
        })
        .await;
        assert!(
            !rig.bcpart().exists(),
            "the staging file is gone before the resume -- that is the condition under test"
        );

        let outcome = rig.run(total).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.chunks_seen(),
            vec![1, 2],
            "a clean restart must re-fetch chunk 1 too -- its bytes vanished with the .bcpart"
        );
        assert_eq!(
            rig.read_target(),
            object,
            "the restarted download must still assemble the exact source object"
        );
        assert!(rig.resume_state().await.is_none());
    }

    // Test 12 (M4c Task 8): a cross-restart resume whose object CHANGED -- the
    // head's ETag no longer matches the checkpoint's stored ETag -- must discard
    // the stale `.bcpart` and re-fetch EVERY chunk, the one the checkpoint marked
    // complete included. Splicing fresh chunks onto bytes from a different object
    // would assemble a plausible-looking corrupt file. The stale chunk 1 is
    // pre-staged with a garbage fill, so trusting it shows up twice: chunk 1 is
    // never re-fetched, and its garbage survives into the assembled file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn download_resume_restarts_when_the_etag_changed() {
        // At the threshold -> two equal chunks.
        let object = make_object(download_threshold() as usize);
        let total = object.len() as u64;
        let rig = rig(4, object.clone(), |fake| {
            // "current" object differs from the one the checkpoint was cut against.
            *fake.head_etag.lock().unwrap() = Some("etag-v2".to_string());
        });

        // Pre-stage a stale chunk 1 recorded complete under the OLD etag, with
        // deliberately wrong bytes so a reused-partial bug corrupts the file.
        let plan = download_chunks(total);
        let c1 = plan[0];
        {
            let pf = PartFile::create(&rig.target, total).unwrap();
            pf.write_at(c1.offset, &vec![0xABu8; c1.length as usize])
                .unwrap();
            // Dropped without finish(): the .bcpart stays on disk holding the
            // stale chunk 1.
        }
        assert!(rig.bcpart().exists());
        rig.seed_resume(DownloadState {
            etag: Some(OBJECT_ETAG.to_string()),
            completed_parts: vec![1],
            bcpart: rig.bcpart(),
            part_size: download_chunk_size(total),
        })
        .await;

        let outcome = rig.run(total).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.chunks_seen(),
            vec![1, 2],
            "a changed object must re-fetch every chunk, not trust the stale .bcpart"
        );
        assert_eq!(
            rig.provider.calls_of(1),
            1,
            "the chunk the checkpoint marked complete must be re-downloaded after the etag changed"
        );
        assert_eq!(
            rig.read_target(),
            object,
            "the restart must assemble the CURRENT object, not the stale partial's garbage bytes"
        );
        assert!(
            !rig.bcpart().exists(),
            "a completed restart renames its fresh .bcpart into place"
        );
        assert!(rig.resume_state().await.is_none());
    }

    // Test 13 (M4c Task 8): a cross-restart resume whose object is UNCHANGED --
    // the head's ETag still matches the checkpoint's stored ETag -- must reuse
    // the `.bcpart` and fetch only the missing chunks, exactly as M4b's
    // in-session resume does. Guards the reconcile against over-discarding a
    // still-valid partial: if it re-fetched everything this would fail on chunk
    // 1's call count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn download_resume_continues_when_the_etag_matches() {
        // At the threshold -> two equal chunks, so chunk 2 is left to fetch.
        let object = make_object(download_threshold() as usize);
        let total = object.len() as u64;
        let rig = rig(4, object.clone(), |_| {}); // default head etag == OBJECT_ETAG

        // Pre-stage chunk 1 with its CORRECT bytes; the resume records it done
        // under the same etag the head still reports.
        let plan = download_chunks(total);
        let c1 = plan[0];
        {
            let pf = PartFile::create(&rig.target, total).unwrap();
            pf.write_at(
                c1.offset,
                &object[c1.offset as usize..(c1.offset + c1.length) as usize],
            )
            .unwrap();
        }
        rig.seed_resume(DownloadState {
            etag: Some(OBJECT_ETAG.to_string()),
            completed_parts: vec![1],
            bcpart: rig.bcpart(),
            part_size: download_chunk_size(total),
        })
        .await;

        let outcome = rig.run(total).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.calls_of(1),
            0,
            "an unchanged object must not re-download a chunk already in the .bcpart"
        );
        assert_eq!(
            rig.provider.calls_of(2),
            1,
            "only the missing chunk is fetched on an unchanged resume"
        );
        assert_eq!(
            rig.read_target(),
            object,
            "the resumed download must still assemble the exact source object"
        );
        assert!(rig.resume_state().await.is_none());
    }

    // Test 14 (M4c Task 8): a resume whose object has VANISHED (head 404 /
    // NotFound) must never present the stale `.bcpart` as a resumed download: it
    // discards the partial and surfaces the not-found failure. Under M4b this
    // resumed and "completed" against the still-in-memory fake bytes -- a file
    // the server no longer has.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn download_resume_discards_and_fails_when_the_object_is_gone() {
        let object = make_object((16 * MB) as usize);
        let total = object.len() as u64;
        let rig = rig(4, object.clone(), |fake| {
            fake.head_missing.store(true, Ordering::SeqCst);
        });

        let plan = download_chunks(total);
        let c1 = plan[0];
        {
            let pf = PartFile::create(&rig.target, total).unwrap();
            pf.write_at(
                c1.offset,
                &object[c1.offset as usize..(c1.offset + c1.length) as usize],
            )
            .unwrap();
        }
        assert!(rig.bcpart().exists());
        rig.seed_resume(DownloadState {
            etag: Some(OBJECT_ETAG.to_string()),
            completed_parts: vec![1],
            bcpart: rig.bcpart(),
            part_size: download_chunk_size(total),
        })
        .await;

        let err = rig.run(total).await.unwrap_err();

        assert_eq!(err.code(), "storage/key-not-found");
        assert!(
            !rig.bcpart().exists(),
            "a resume against a vanished object discards the stale partial"
        );
        assert!(
            !rig.target.exists(),
            "nothing is renamed into place when the object is gone"
        );
        assert_eq!(
            rig.provider.chunks_seen(),
            Vec::<i32>::new(),
            "not a single chunk is fetched once the head says the object is gone"
        );
    }

    // Test 15 (Task 3, spec §4.6): a resume whose recorded `part_size` no
    // longer matches the current plan must be discarded wholesale -- its
    // `completed_parts` chunk numbers would otherwise map to different byte
    // ranges under this plan and corrupt the file. The etag is left matching
    // so the etag-based reconcile alone would happily continue; only the
    // part_size mismatch must trigger the discard. Modelled on
    // `download_resume_restarts_when_the_etag_changed`, with a mismatched
    // `part_size` standing in for the changed etag.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn download_resume_restarts_when_the_recorded_part_size_does_not_match_the_plan() {
        // At the threshold -> two equal chunks.
        let object = make_object(download_threshold() as usize);
        let total = object.len() as u64;
        let rig = rig(4, object.clone(), |_| {}); // default head etag == OBJECT_ETAG

        // Pre-stage a stale chunk 1 recorded complete under a DIFFERENT part
        // size, with deliberately wrong bytes so a reused-partial bug
        // corrupts the file.
        let plan = download_chunks(total);
        let c1 = plan[0];
        {
            let pf = PartFile::create(&rig.target, total).unwrap();
            pf.write_at(c1.offset, &vec![0xABu8; c1.length as usize])
                .unwrap();
            // Dropped without finish(): the .bcpart stays on disk holding the
            // stale chunk 1.
        }
        assert!(rig.bcpart().exists());
        rig.seed_resume(DownloadState {
            etag: Some(OBJECT_ETAG.to_string()),
            completed_parts: vec![1],
            bcpart: rig.bcpart(),
            // Deliberately wrong: the current plan's chunk_size is
            // `download_chunk_size(total)`.
            part_size: download_chunk_size(total) + 1,
        })
        .await;

        let outcome = rig.run(total).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.chunks_seen(),
            vec![1, 2],
            "a mismatched recorded part_size must discard the checkpoint and re-fetch every chunk"
        );
        assert_eq!(
            rig.provider.calls_of(1),
            1,
            "the chunk the checkpoint marked complete must be re-downloaded once the recorded \
             part_size no longer matches the plan"
        );
        assert_eq!(
            rig.read_target(),
            object,
            "the restart must assemble the CURRENT object, not the stale partial's garbage bytes"
        );
        assert!(
            !rig.bcpart().exists(),
            "a completed restart renames its fresh .bcpart into place"
        );
        assert!(rig.resume_state().await.is_none());
    }

    // Test 16 (Task 3, spec §4.6): a legacy checkpoint with no recorded
    // `part_size` (deserializes to 0 via `#[serde(default)]`) must be treated
    // exactly like a mismatch -- discarded wholesale rather than trusted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn download_resume_restarts_when_the_recorded_part_size_is_zero() {
        let object = make_object(download_threshold() as usize);
        let total = object.len() as u64;
        let rig = rig(4, object.clone(), |_| {});

        let plan = download_chunks(total);
        let c1 = plan[0];
        {
            let pf = PartFile::create(&rig.target, total).unwrap();
            pf.write_at(c1.offset, &vec![0xABu8; c1.length as usize])
                .unwrap();
        }
        rig.seed_resume(DownloadState {
            etag: Some(OBJECT_ETAG.to_string()),
            completed_parts: vec![1],
            bcpart: rig.bcpart(),
            part_size: 0,
        })
        .await;

        let outcome = rig.run(total).await.unwrap();

        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(
            rig.provider.chunks_seen(),
            vec![1, 2],
            "a zero (legacy) part_size must discard the checkpoint and re-fetch every chunk"
        );
        assert_eq!(rig.read_target(), object);
        assert!(rig.resume_state().await.is_none());
    }
}
