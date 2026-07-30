//! Routes a task to the upload or download runner by its direction, so the
//! engine can stay direction-agnostic and hold a single runner (design: the
//! engine's scheduling never branches on direction).

use async_trait::async_trait;
use std::sync::Arc;

use crate::error::AppResult;
use crate::transfer::download::DownloadRunner;
use crate::transfer::engine::{RunOutcome, TaskContext, TransferRunner};
use crate::transfer::model::Direction;
use crate::transfer::upload::UploadRunner;

pub struct DispatchRunner {
    pub upload: Arc<UploadRunner>,
    pub download: Arc<DownloadRunner>,
}

#[async_trait]
impl TransferRunner for DispatchRunner {
    async fn run(&self, ctx: TaskContext) -> AppResult<RunOutcome> {
        match ctx.task.direction {
            Direction::Upload => self.upload.run(ctx).await,
            Direction::Download => self.download.run(ctx).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderHub;
    use crate::store::{Connection, SecureStore};
    use crate::transfer::engine::{EngineConfig, EnqueueSpec, TransferEngine, TransferSink};
    use crate::transfer::model::TransferTaskDto;
    use crate::transfer::{bcpart_path, spawn_aggregator, ProgressSink};
    use std::time::Duration;

    // `TaskContext` cannot be built from this module: `TaskControl::new` is
    // private to `engine` and `ProgressHandle`'s fields are private too, with
    // no public constructor (see upload.rs/download.rs's own doc comments on
    // exactly this point). The only place a real `TaskContext` gets built is
    // `engine.rs`'s private `drive()`, so the only reachable way to exercise
    // `DispatchRunner::run` with a genuine `ctx` is through the public
    // `TransferEngine` API below -- which also happens to exercise
    // `EnqueueSpec::for_download` for free.
    //
    // Distinguishing "which concrete runner ran" can't go through
    // `TransferTaskDto::error_code`: both runners' only network-free failure
    // is `AppError::FileIo`, which collapses to the identical
    // `"local/file-io"` code for both directions (the offending path is not
    // surfaced to the DTO). So this test uses a signal that is unique to one
    // runner and produced *before* any network call: `DownloadRunner::run`
    // always creates `<local_path>.bcpart` locally (`PartFile::create`)
    // before it ever touches the provider, while `UploadRunner::run` never
    // creates any file near its `local_path`. A `DispatchRunner` that routed
    // by direction incorrectly (swapped, or always picking one runner) would
    // flip one of the two assertions below.

    struct NoopSink;
    impl TransferSink for NoopSink {
        fn state_changed(&self, _task: &TransferTaskDto) {}
    }

    struct NoopProgress;
    impl ProgressSink for NoopProgress {
        fn flush(&self, _batch: Vec<crate::transfer::ProgressPayload>) {}
    }

    /// Waits for `check` to hold, or panics after a generous, still-fast
    /// budget. Filesystem side effects here are local-only, so this never
    /// legitimately needs more than a handful of scheduling rounds.
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
    async fn dispatch_routes_each_direction_to_its_own_runner() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Arc::new(ProviderHub::new(SecureStore {
            path: dir.path().join("connections.enc"),
        }));
        // A connection profile just has to *build* (see `from_connection`);
        // it never needs to actually be reachable, because both filesystem
        // signals below fire before either runner makes a network call.
        hub.mutate(|list| {
            list.push(Connection {
                id: "c1".to_string(),
                provider: "minio".to_string(),
                name: "n".to_string(),
                endpoint: "http://127.0.0.1:9000".to_string(),
                region: String::new(),
                access_key_id: "AK".to_string(),
                secret_access_key: "SK".to_string(),
                default_bucket: None,
                api_token: None,
            });
            Ok(())
        })
        .await
        .unwrap();

        let progress_tx = spawn_aggregator(Arc::new(NoopProgress));
        let engine = TransferEngine::new(
            hub,
            Arc::new(DispatchRunner {
                upload: Arc::new(UploadRunner),
                download: Arc::new(DownloadRunner),
            }),
            Arc::new(NoopSink),
            progress_tx,
            EngineConfig::default(),
            // This test exercises direction routing, not checkpointing.
            None,
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
        );

        // --- Direction::Download must reach DownloadRunner ------------------
        let download_target = dir.path().join("downloaded.bin");
        let download_spec = EnqueueSpec::for_download(
            "c1".to_string(),
            "bucket".to_string(),
            "key".to_string(),
            download_target.clone(),
            32,
            "downloaded.bin".to_string(),
        );
        engine.enqueue(download_spec).await.unwrap();

        let download_bcpart = bcpart_path(&download_target);
        eventually(
            || download_bcpart.exists(),
            "the download task's .bcpart appears, proving DownloadRunner ran",
        )
        .await;
        assert_eq!(
            std::fs::metadata(&download_bcpart).unwrap().len(),
            32,
            "the .bcpart must be preallocated to the task's total"
        );

        // --- Direction::Upload must NOT reach DownloadRunner ----------------
        let upload_source = dir.path().join("to-upload.bin");
        std::fs::write(&upload_source, b"hello world").unwrap();
        let upload_spec = EnqueueSpec::for_upload(
            "c1".to_string(),
            "bucket".to_string(),
            "key".to_string(),
            upload_source.clone(),
        )
        .unwrap();
        engine.enqueue(upload_spec).await.unwrap();

        // Give a wrongly-dispatched download every chance to create a
        // `.bcpart` next to the upload source before concluding it did not.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !bcpart_path(&upload_source).exists(),
            "an upload-direction task must never reach DownloadRunner"
        );
    }
}
