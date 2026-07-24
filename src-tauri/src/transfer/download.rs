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

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::provider::Provider;
use crate::transfer::engine::{
    DownloadState, ResumeState, RunOutcome, TaskContext, TransferRunner,
};
use crate::transfer::part::{plan_upload, PartSpec, UploadPlan};
use crate::transfer::partfile::PartFile;
use crate::transfer::retry::{backoff_delay, is_retryable, MAX_RETRIES};

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

pub struct DownloadRunner;

#[async_trait]
impl TransferRunner for DownloadRunner {
    async fn run(&self, ctx: TaskContext) -> AppResult<RunOutcome> {
        let provider = ctx.hub.provider(&ctx.task.connection_id).await?;
        let bucket = ctx.task.bucket.clone();
        let key = ctx.task.key.clone();
        let target = PathBuf::from(&ctx.task.local_path);
        let token = ctx.control.token();

        // Resume: reuse the existing .bcpart + completed chunks if present.
        let existing = match ctx.resume.lock().await.clone() {
            Some(ResumeState::Download(ds)) => Some(ds),
            _ => None,
        };

        let total = ctx.task.total;
        let plan = plan_upload(total);
        let chunks: Vec<PartSpec> = match &plan {
            UploadPlan::Single { length } => vec![PartSpec {
                number: 1,
                offset: 0,
                length: *length,
            }],
            UploadPlan::Multipart { parts, .. } => parts.clone(),
        };

        // Open (or reopen) the staging file. On a fresh start create+preallocate;
        // on resume the .bcpart already holds the finished chunks.
        let (partfile, done) = if let Some(ds) = existing {
            (
                PartFile::reopen(&target, total, &ds.bcpart)?,
                ds.completed_parts,
            )
        } else {
            (PartFile::create(&target, total)?, Vec::new())
        };
        let partfile = Arc::new(partfile);
        let bcpart = partfile.bcpart_path().to_path_buf();

        // Re-report already-finished chunks so the bar doesn't under-count
        // after the engine zeroed `transferred` on resume.
        for c in chunks.iter().filter(|c| done.contains(&c.number)) {
            ctx.progress.add(c.length);
        }
        // Record resume state immediately so a cancel before any chunk lands
        // still knows which .bcpart to delete.
        *ctx.resume.lock().await = Some(ResumeState::Download(DownloadState {
            etag: None,
            completed_parts: done.clone(),
            bcpart: bcpart.clone(),
        }));

        let pending = pending_chunks(&chunks, &done);
        let permits = Arc::new(Semaphore::new(ctx.part_limit.max(1)));
        // A child token, so stopping the remaining chunks after one of them
        // failed for good is distinguishable from a user-requested stop: a
        // chunk that gave up because a sibling died must never read as a pause.
        let chunk_token = token.child_token();
        let completed = Arc::new(tokio::sync::Mutex::new(done));
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
            let bucket = bucket.clone();
            let key = key.clone();
            let token = chunk_token.clone();
            let progress = ctx.progress.clone();

            set.spawn(async move {
                let _permit = permit;
                download_one_chunk(provider, &bucket, &key, chunk, &partfile, &token, &progress)
                    .await
            });
        }

        let mut first_error: Option<AppError> = None;
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(Ok(Some(number))) => completed.lock().await.push(number),
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
        let done_now = completed.lock().await.clone();
        *ctx.resume.lock().await = Some(ResumeState::Download(DownloadState {
            etag: None,
            completed_parts: done_now.clone(),
            bcpart: bcpart.clone(),
        }));

        if let Some(err) = first_error {
            return Err(err);
        }

        // A user-requested stop wins over "all chunks happened to finish":
        // cancel discards the .bcpart, pause keeps it.
        match ctx.control.requested() {
            Some(crate::transfer::engine::StopKind::Cancel) => {
                // Drop the Arc so the file handle closes before we delete it.
                drop(partfile);
                let _ = std::fs::remove_file(&bcpart);
                *ctx.resume.lock().await = None;
                return Ok(RunOutcome::Stopped);
            }
            Some(crate::transfer::engine::StopKind::Pause) => return Ok(RunOutcome::Stopped),
            None => {}
        }

        // Every chunk landed and no stop was requested: rename into place.
        // `Completed` is returned only after this succeeds.
        Arc::try_unwrap(partfile)
            .map_err(|_| AppError::Internal {
                message: "download finished with outstanding chunk handles".to_string(),
            })?
            .finish()?;
        *ctx.resume.lock().await = None;
        Ok(RunOutcome::Completed)
    }
}

async fn download_one_chunk(
    provider: Arc<crate::provider::S3Provider>,
    bucket: &str,
    key: &str,
    chunk: PartSpec,
    partfile: &PartFile,
    token: &CancellationToken,
    progress: &crate::transfer::engine::ProgressHandle,
) -> AppResult<Option<i32>> {
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
    progress.add(chunk.length);
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
                tokio::select! {
                    _ = token.cancelled() => return Ok(None),
                    _ = tokio::time::sleep(backoff_delay(retries)) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
