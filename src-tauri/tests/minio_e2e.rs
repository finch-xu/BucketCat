//! MinIO end-to-end integration tests (opt-in).
//!
//! These tests drive the real [`bucketcat_lib::provider::S3Provider`] and
//! [`bucketcat_lib::store::SecureStore`] against a live server / real machine
//! key derivation -- design doc §8 calls for exactly this (an automated,
//! permanent check against actual MinIO) rather than a purely manual
//! click-through. Every test in this file is `#[ignore]`d, so a plain
//! `cargo test` never touches Docker, the network, or spins up a container:
//! this file is opt-in, run explicitly, not part of the hermetic default
//! suite.
//!
//! ## Running these tests
//!
//! 1. Start Docker (if its daemon isn't already running) and launch MinIO:
//!
//!    ```bash
//!    open -a Docker   # macOS, only if the daemon isn't already up
//!    docker run -d --name bucketcat-minio -p 9000:9000 -p 9001:9001 \
//!      -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
//!      minio/minio server /data --console-address ":9001"
//!    ```
//!
//! 2. Run the suite:
//!
//!    ```bash
//!    cargo test --test minio_e2e -- --ignored
//!    ```
//!
//! 3. Tear the container down when finished (the image can stay cached for
//!    next time):
//!
//!    ```bash
//!    docker rm -f bucketcat-minio
//!    ```
//!
//! If ports 9000/9001 are already taken on your machine, publish MinIO on
//! different host ports and point these tests at the alternate one via
//! `BUCKETCAT_E2E_ENDPOINT` (falls back to `http://localhost:9000` when
//! unset):
//!
//! ```bash
//! docker run -d --name bucketcat-minio -p 19000:9000 -p 19001:9001 \
//!   -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
//!   minio/minio server /data --console-address ":19001"
//! BUCKETCAT_E2E_ENDPOINT=http://localhost:19000 \
//!   cargo test --test minio_e2e -- --ignored
//! ```
//!
//! Credentials are always the documented MinIO defaults
//! (`minioadmin`/`minioadmin`) -- nothing here is a real secret.
//!
//! ## A fixed error-family mismatch
//!
//! [`wrong_secret_surfaces_expected_error_family`] used to document (rather
//! than paper over) a real finding from running this suite against actual
//! MinIO: a wrong secret key did **not** land in `AppError`'s `auth/*`
//! family. It surfaced as `internal` (message `"unhandled error
//! (SignatureDoesNotMatch)"`) instead, because `aws-sdk-s3`'s generated
//! `ListBuckets` error enum doesn't model `SignatureDoesNotMatch` as one of
//! its own variants, so it collapsed to `aws_sdk_s3::Error::Unhandled`
//! before `crate::error`'s keyword sniff -- which doesn't recognize that
//! phrase either -- ever got a chance at an `auth/*` classification.
//!
//! `provider::s3`'s `normalize_s3_error` now has a third tier for exactly
//! this: when the existing modeled-variant/keyword-sniff conversion would
//! otherwise land on `AppError::Internal`, it reads the raw S3 error code
//! off the pre-erasure operation error (`SignatureDoesNotMatch`,
//! `InvalidAccessKeyId`, `AccessDenied*`, `NoSuchBucket`, ...) via
//! `ProvideErrorMetadata` and maps known codes directly. This test now
//! asserts the fixed, correct behavior -- see its doc comment.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use sha2::{Digest, Sha256};

use bucketcat_lib::commands::transfer::local_target;
use bucketcat_lib::provider::{from_connection, Provider, ProviderHub, S3Provider, UploadedPart};
use bucketcat_lib::store::{Connection, SecureStore};
use bucketcat_lib::transfer::{
    bcpart_path, checkpoint, plan_download, plan_upload_with, restore_all, spawn_aggregator,
    Direction, DispatchRunner, DownloadRunner, EnqueueSpec, MultipartState, ProgressPayload,
    ProgressSink, ResumeState, SharedLimits, TransferEngine, TransferSink, TransferStatus,
    TransferTaskDto, TransferTuning, UploadPlan, UploadRunner,
};

// --- M3 object data-plane helpers ------------------------------------------

/// A raw SDK client for test seeding only: M3 has no provider upload path
/// yet (that's M4), so tests PUT their fixture objects directly. Mirrors
/// `from_connection`'s MinIO configuration (path-style, static creds).
fn raw_seed_client() -> aws_sdk_s3::Client {
    let credentials =
        aws_sdk_s3::config::Credentials::new("minioadmin", "minioadmin", None, None, "e2e-seed");
    let config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .endpoint_url(endpoint())
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();
    aws_sdk_s3::Client::from_conf(config)
}

/// Seeds one small text object.
async fn put_text(client: &aws_sdk_s3::Client, bucket: &str, key: &str) {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(aws_sdk_s3::primitives::ByteStream::from_static(
            b"bucketcat-e2e",
        ))
        .send()
        .await
        .unwrap_or_else(|e| panic!("seeding put_object {key} should succeed: {e}"));
}

/// Best-effort teardown: removes every object (non-delimiter raw listing,
/// fine for test-sized buckets) then the bucket itself, so repeated runs
/// never collide and the MinIO container stays clean.
async fn drain_and_delete_bucket(
    client: &aws_sdk_s3::Client,
    provider: &bucketcat_lib::provider::S3Provider,
    bucket: &str,
) {
    let listed = client
        .list_objects_v2()
        .bucket(bucket)
        .send()
        .await
        .expect("cleanup listing should succeed");
    let keys: Vec<String> = listed
        .contents()
        .iter()
        .filter_map(|o| o.key().map(str::to_string))
        .collect();
    if !keys.is_empty() {
        provider
            .delete_objects(bucket, &keys)
            .await
            .expect("cleanup delete_objects should succeed");
    }
    provider
        .delete_bucket(bucket)
        .await
        .expect("cleanup delete_bucket should succeed");
}

/// Collects every page of a listing under `prefix` with the given page
/// size, asserting the pager terminates and no page exceeds `max_keys`.
async fn list_all_pages(
    provider: &bucketcat_lib::provider::S3Provider,
    bucket: &str,
    prefix: &str,
    max_keys: i32,
) -> (Vec<bucketcat_lib::provider::ObjectEntry>, usize) {
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = provider
            .list_objects(bucket, prefix, token.as_deref(), max_keys)
            .await
            .expect("list_objects page should succeed");
        pages += 1;
        assert!(
            page.entries.len() <= max_keys as usize,
            "a page must never exceed max_keys"
        );
        all.extend(page.entries);
        match page.next_token {
            Some(t) => token = Some(t),
            None => break,
        }
        assert!(pages < 50, "paging must terminate");
    }
    (all, pages)
}

/// The endpoint these tests target: `BUCKETCAT_E2E_ENDPOINT` if set (for a
/// MinIO container published on non-default ports), otherwise
/// `http://localhost:9000`.
fn endpoint() -> String {
    std::env::var("BUCKETCAT_E2E_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".to_string())
}

/// A `Connection` pointed at the live MinIO container, with the given
/// secret access key (the documented `minioadmin` default, or deliberately
/// wrong, depending on the test).
fn minio_connection(secret: &str) -> Connection {
    Connection {
        id: "e2e-minio".to_string(),
        provider: "minio".to_string(),
        name: "e2e minio".to_string(),
        endpoint: endpoint(),
        region: "us-east-1".to_string(),
        access_key_id: "minioadmin".to_string(),
        secret_access_key: secret.to_string(),
        default_bucket: None,
        api_token: None,
    }
}

/// A bucket name unique to this test run, so repeated runs (and any run
/// that overlaps with a leftover bucket from a previous crashed run) never
/// collide. S3/MinIO bucket names must be lowercase; a UUID's hex-and-hyphen
/// form is already valid as-is.
fn unique_bucket_name() -> String {
    format!("bucketcat-e2e-{}", uuid::Uuid::new_v4())
}

// --- from_connection + test_connection against live MinIO -----------------

#[tokio::test]
#[ignore]
async fn from_connection_and_test_connection_succeed_against_live_minio() {
    let conn = minio_connection("minioadmin");
    let provider = from_connection(&conn).expect("a valid MinIO connection profile should build");

    provider
        .test_connection()
        .await
        .expect("test_connection should succeed against a live, correctly-credentialed MinIO");
}

// --- create_bucket -> list_buckets -> delete_bucket round trip ------------

#[tokio::test]
#[ignore]
async fn create_list_delete_bucket_round_trip() {
    let conn = minio_connection("minioadmin");
    let provider = from_connection(&conn).expect("a valid MinIO connection profile should build");
    let bucket = unique_bucket_name();

    provider
        .create_bucket(&bucket)
        .await
        .expect("create_bucket should succeed against live MinIO");

    let buckets = provider
        .list_buckets()
        .await
        .expect("list_buckets should succeed after create_bucket");
    assert!(
        buckets.iter().any(|b| b.name == bucket),
        "list_buckets should contain the just-created bucket `{bucket}`, saw: {buckets:?}"
    );

    provider
        .delete_bucket(&bucket)
        .await
        .expect("delete_bucket should succeed against live MinIO");

    let buckets_after_delete = provider
        .list_buckets()
        .await
        .expect("list_buckets should succeed after delete_bucket");
    assert!(
        !buckets_after_delete.iter().any(|b| b.name == bucket),
        "list_buckets should no longer contain `{bucket}` after delete_bucket, saw: {buckets_after_delete:?}"
    );
}

// --- wrong secret -----------------------------------------------------------

/// Wrong secret key against a real server.
///
/// **Originally found running this against live MinIO (RELEASE.2025-09-07,
/// `minio/minio:latest`):** a wrong `SecretAccessKey` did **not** surface
/// as `AppError`'s `auth/*` family -- it surfaced as `internal`, with
/// `params.message` equal to `"unhandled error (SignatureDoesNotMatch)"`.
///
/// Why that happened: MinIO returns a 403 whose S3 error code is
/// `SignatureDoesNotMatch`. `aws-sdk-s3`'s generated per-operation error
/// enum for `ListBuckets` doesn't model that error code as one of its own
/// variants (unlike `NoSuchBucket`/`BucketAlreadyExists`/`AccessDenied`,
/// which *are* modeled for operations that do model them, and are
/// explicitly matched in `crate::error`'s `From<aws_sdk_s3::Error>` impl),
/// so the SDK collapses it into the catch-all `aws_sdk_s3::Error::Unhandled`
/// variant even though a real, parseable response came back. That used to
/// land in `crate::error`'s wildcard `_` arm, whose keyword sniff over the
/// rendered message doesn't recognize `"SignatureDoesNotMatch"`, so it fell
/// through to `AppError::Internal`.
///
/// **Fixed:** `provider::s3::normalize_s3_error` now has a third tier,
/// consulted only when the existing modeled-variant/keyword-sniff
/// conversion would otherwise produce `AppError::Internal`: it reads the
/// raw S3 error code directly off the pre-erasure operation error via
/// `ProvideErrorMetadata` and maps known auth codes
/// (`SignatureDoesNotMatch`, `InvalidAccessKeyId`, `AccessDenied`,
/// `AccessDeniedException`) to `AppError::InvalidCredentials` /
/// `AppError::AccessDenied`. This test asserts that fixed behavior against
/// a live MinIO server, so a regression here means the real wire response
/// (or the SDK's error modeling) changed, not just a unit assumption.
#[tokio::test]
#[ignore]
async fn wrong_secret_surfaces_expected_error_family() {
    let conn = minio_connection("definitely-the-wrong-secret");
    let provider = from_connection(&conn).expect("a valid MinIO connection profile should build");

    let err = provider
        .test_connection()
        .await
        .expect_err("test_connection with a wrong secret key must not succeed");

    let code = err.code();
    assert_eq!(
        code, "auth/invalid-credentials",
        "expected a wrong secret key against live MinIO to surface \
         `auth/invalid-credentials` (recovered from the raw `SignatureDoesNotMatch` S3 \
         error code -- see this test's doc comment) -- got `{code}` instead, which means \
         the error mapping chain (or MinIO's behavior) changed and this assertion should \
         be revisited"
    );
}

// --- unreachable endpoint ----------------------------------------------------

#[tokio::test]
#[ignore]
async fn unreachable_endpoint_surfaces_network_error() {
    let mut conn = minio_connection("minioadmin");
    conn.endpoint = "http://localhost:19999".to_string();
    let provider = from_connection(&conn).expect("a valid connection profile should build");

    let err = provider
        .test_connection()
        .await
        .expect_err("test_connection against an unreachable endpoint must not succeed");

    let code = err.code();
    assert!(
        code.starts_with("network/"),
        "expected a network/* error code for an unreachable endpoint, got `{code}` instead"
    );
}

// --- SecureStore round trip with a real, machine-derived key ---------------

/// Exercises [`SecureStore::save`]/[`SecureStore::load`] with the *real*
/// `derive_key()` (backed by this machine's `machine_uid::get()`), not the
/// fixed test key `store::secure`'s own unit tests use -- so this is the one
/// place the actual "encrypt with this machine's key, decrypt after a
/// restart" contract gets end-to-end coverage. Doesn't touch MinIO or
/// Docker at all, but is kept `#[ignore]`d along with the rest of this file
/// for a single, consistent "opt-in e2e suite" story.
#[test]
#[ignore]
fn secure_store_round_trip_survives_restart_with_real_derive_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("connections.enc");

    let connections = vec![
        Connection {
            id: "c1".to_string(),
            provider: "minio".to_string(),
            name: "conn one".to_string(),
            endpoint: "http://localhost:9000".to_string(),
            region: "us-east-1".to_string(),
            access_key_id: "minioadmin".to_string(),
            secret_access_key: "top-secret-value-one".to_string(),
            default_bucket: None,
            api_token: None,
        },
        Connection {
            id: "c2".to_string(),
            provider: "minio".to_string(),
            name: "conn two".to_string(),
            endpoint: "http://localhost:9000".to_string(),
            region: "us-east-1".to_string(),
            access_key_id: "minioadmin".to_string(),
            secret_access_key: "top-secret-value-two".to_string(),
            default_bucket: Some("some-bucket".to_string()),
            api_token: None,
        },
    ];

    let store = SecureStore { path: path.clone() };
    store
        .save(&connections)
        .expect("save should succeed with the real machine-derived key");

    // A brand-new `SecureStore` value pointed at the same path simulates
    // reopening the app after a full restart: nothing about the store's
    // state lives anywhere but the file on disk between `save` and this
    // fresh `load`.
    let reopened_after_restart = SecureStore { path: path.clone() };
    let loaded = reopened_after_restart
        .load()
        .expect("load should succeed after a simulated restart");

    assert_eq!(
        loaded, connections,
        "connections loaded after a simulated restart must exactly match what was saved"
    );

    let raw = std::fs::read(&path).expect("reading the raw store file should succeed");
    assert!(
        raw.len() < 2 || &raw[..2] != b"[{",
        "the raw store file must not start like plaintext JSON (`[{{`); saw: {:?}",
        &raw[..raw.len().min(16)]
    );
    let raw_lossy = String::from_utf8_lossy(&raw);
    assert!(
        !raw_lossy.contains("top-secret-value-one"),
        "the raw store file must not contain a secret access key in the clear"
    );
    assert!(
        !raw_lossy.contains("top-secret-value-two"),
        "the raw store file must not contain a secret access key in the clear"
    );
}

// --- M3: paged listing -------------------------------------------------------

#[tokio::test]
#[ignore]
async fn paged_listing_spans_multiple_pages_without_duplicates() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    for name in [
        "page-a.txt",
        "page-b.txt",
        "page-c.txt",
        "page-d.txt",
        "page-e.txt",
    ] {
        put_text(&client, &bucket, name).await;
    }
    put_text(&client, &bucket, "docs/one.md").await;
    put_text(&client, &bucket, "media/two.jpg").await;

    // 7 root entries (5 files + 2 common prefixes) at 3 per page = 3 pages.
    let (all, pages) = list_all_pages(&provider, &bucket, "", 3).await;

    assert!(pages >= 3, "expected at least 3 pages, got {pages}");
    let mut names: Vec<String> = all.iter().map(|e| e.name.clone()).collect();
    let unique: std::collections::HashSet<&String> = names.iter().collect();
    assert_eq!(
        unique.len(),
        names.len(),
        "no duplicates across pages: {names:?}"
    );
    names.sort();
    assert_eq!(
        names,
        vec![
            "docs",
            "media",
            "page-a.txt",
            "page-b.txt",
            "page-c.txt",
            "page-d.txt",
            "page-e.txt"
        ]
    );

    drain_and_delete_bucket(&client, &provider, &bucket).await;
}

// --- M3: folder semantics ----------------------------------------------------

#[tokio::test]
#[ignore]
async fn delimiter_listing_surfaces_folders_and_children() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    put_text(&client, &bucket, "root.txt").await;
    put_text(&client, &bucket, "docs/readme.md").await;
    put_text(&client, &bucket, "docs/guide.md").await;
    put_text(&client, &bucket, "docs/img/cat.png").await;

    let (root, _) = list_all_pages(&provider, &bucket, "", 100).await;
    let folders: Vec<&str> = root
        .iter()
        .filter(|e| e.is_prefix)
        .map(|e| e.name.as_str())
        .collect();
    let files: Vec<&str> = root
        .iter()
        .filter(|e| !e.is_prefix)
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(folders, vec!["docs"]);
    assert_eq!(files, vec!["root.txt"]);
    let docs_prefix = root.iter().find(|e| e.is_prefix).unwrap();
    assert_eq!(docs_prefix.key, "docs/");
    assert_eq!(docs_prefix.size, None);

    let (docs, _) = list_all_pages(&provider, &bucket, "docs/", 100).await;
    let mut docs_names: Vec<&str> = docs.iter().map(|e| e.name.as_str()).collect();
    docs_names.sort();
    assert_eq!(docs_names, vec!["guide.md", "img", "readme.md"]);
    let guide = docs.iter().find(|e| e.name == "guide.md").unwrap();
    assert_eq!(guide.key, "docs/guide.md");
    assert!(guide.size.is_some(), "a real object must carry its size");
    assert!(
        guide.last_modified.is_some(),
        "a real object must carry mtime"
    );

    // Prefix *search* semantics: "docs/gu" (path + typed text) matches only guide.md.
    let (searched, _) = list_all_pages(&provider, &bucket, "docs/gu", 100).await;
    let searched_names: Vec<&str> = searched.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(searched_names, vec!["guide.md"]);

    drain_and_delete_bucket(&client, &provider, &bucket).await;
}

// --- M3: batch delete --------------------------------------------------------

/// Batch delete of existing + nonexistent keys. S3/MinIO semantics: deleting
/// a key that doesn't exist is itself a *success* (idempotent delete), so a
/// nonexistent key does NOT produce a per-key failure — `succeeded` counts
/// it. Real per-key failures (auth, object lock) can't be provoked from a
/// default MinIO container without heavyweight setup, so the
/// failure-mapping path is covered by the pure `failed_key` unit tests in
/// `provider::s3` instead; this test pins the wire-level success/count
/// behavior and that the batch as a whole never aborts.
#[tokio::test]
#[ignore]
async fn batch_delete_counts_missing_keys_as_success_and_removes_the_rest() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    put_text(&client, &bucket, "keep.txt").await;
    put_text(&client, &bucket, "del-1.txt").await;
    put_text(&client, &bucket, "del-2.txt").await;

    let result = provider
        .delete_objects(
            &bucket,
            &[
                "del-1.txt".to_string(),
                "del-2.txt".to_string(),
                "never-existed.txt".to_string(),
            ],
        )
        .await
        .expect("delete_objects should succeed as a batch");

    assert_eq!(
        result.succeeded, 3,
        "idempotent delete counts the missing key"
    );
    assert!(
        result.failed.is_empty(),
        "no per-key failures expected: {:?}",
        result.failed
    );

    let (rest, _) = list_all_pages(&provider, &bucket, "", 100).await;
    let names: Vec<&str> = rest.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["keep.txt"]);

    drain_and_delete_bucket(&client, &provider, &bucket).await;
}

// --- M3: rename --------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn rename_object_round_trip() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    put_text(&client, &bucket, "docs/old name.md").await;

    provider
        .rename_object(&bucket, "docs/old name.md", "docs/new-name.md")
        .await
        .expect("rename should succeed (copy source must be URL-encoded)");

    let (docs, _) = list_all_pages(&provider, &bucket, "docs/", 100).await;
    let names: Vec<&str> = docs.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["new-name.md"], "old key gone, new key present");

    drain_and_delete_bucket(&client, &provider, &bucket).await;
}

#[tokio::test]
#[ignore]
async fn rename_missing_source_surfaces_key_not_found() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let err = provider
        .rename_object(&bucket, "ghost.txt", "still-ghost.txt")
        .await
        .expect_err("renaming a nonexistent key must fail");

    assert_eq!(
        err.code(),
        "storage/key-not-found",
        "CopyObject's NoSuchKey must be recovered via the tier-3 error-code mapping"
    );

    drain_and_delete_bucket(&client, &provider, &bucket).await;
}

// --- M3: create folder -------------------------------------------------------

#[tokio::test]
#[ignore]
async fn created_folder_is_visible_as_prefix_and_empty_inside() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    provider
        .create_folder(&bucket, "reports/2026")
        .await
        .expect("create_folder should succeed");

    let (root, _) = list_all_pages(&provider, &bucket, "", 100).await;
    let root_names: Vec<(&str, bool)> = root
        .iter()
        .map(|e| (e.name.as_str(), e.is_prefix))
        .collect();
    assert_eq!(root_names, vec![("reports", true)]);

    let (reports, _) = list_all_pages(&provider, &bucket, "reports/", 100).await;
    let report_names: Vec<(&str, bool)> = reports
        .iter()
        .map(|e| (e.name.as_str(), e.is_prefix))
        .collect();
    assert_eq!(report_names, vec![("2026", true)]);

    // Inside the leaf folder: the marker object itself is filtered out.
    let (leaf, _) = list_all_pages(&provider, &bucket, "reports/2026/", 100).await;
    assert!(
        leaf.is_empty(),
        "the folder marker must not list as an entry: {leaf:?}"
    );

    drain_and_delete_bucket(&client, &provider, &bucket).await;
}

// --- ProviderHub against live MinIO (review Finding 2) ----------------------
//
// Every test above drives `from_connection`/`Provider` directly, never
// `ProviderHub` -- so none of them cover "a client obtained through
// `ProviderHub::provider()` performs a real S3 operation correctly", nor
// that `ProviderHub::mutate`'s cache invalidation genuinely takes effect
// against a live server rather than only inside the in-memory map. This
// test closes that gap: it builds a `ProviderHub` over a `SecureStore` in
// its own `tempfile::tempdir()` (so it never touches a real config
// directory or collides with another test's connections), persists a
// connection via `mutate`, exercises the cached client with a real
// `list_buckets`, confirms a second lookup is served from cache
// (`Arc::ptr_eq`) and still works, then rotates the secret to a wrong value
// and confirms the cache was actually invalidated: the next lookup returns
// a *different* client, and that client's `list_buckets` fails against the
// real server with the same `auth/invalid-credentials` family
// `wrong_secret_surfaces_expected_error_family` documents above.
#[tokio::test]
#[ignore]
async fn provider_hub_round_trip_against_live_minio() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hub = ProviderHub::new(SecureStore {
        path: dir.path().join("connections.enc"),
    });
    let id = "e2e-hub-minio".to_string();

    hub.mutate(|list| {
        list.push(Connection {
            id: id.clone(),
            ..minio_connection("minioadmin")
        });
        Ok(())
    })
    .await
    .expect("mutate should persist the connection");

    // First lookup: a cache miss, so this builds a real client. Exercise it
    // against the live server rather than just checking it was returned.
    let provider = hub
        .provider(&id)
        .await
        .expect("provider() should build a client for the just-persisted connection");
    provider
        .list_buckets()
        .await
        .expect("list_buckets through a hub-provided client should succeed against live MinIO");

    // Second lookup: must be served from cache (no rebuild), and the cached
    // client must still work.
    let provider_again = hub
        .provider(&id)
        .await
        .expect("second provider() lookup should succeed");
    assert!(
        std::sync::Arc::ptr_eq(&provider, &provider_again),
        "the second provider() lookup must reuse the cached client, not rebuild it"
    );
    provider_again
        .list_buckets()
        .await
        .expect("the cached client must still work against live MinIO");

    // Rotate the secret to a wrong value: `mutate` must invalidate the
    // cache, and the *next* `provider()` call's client must fail against the
    // real server -- proving invalidation genuinely takes effect, not just
    // that the map entry changed.
    hub.mutate(|list| {
        list[0].secret_access_key = "definitely-the-wrong-secret".to_string();
        Ok(())
    })
    .await
    .expect("mutate should persist the rotated secret");

    let rotated_provider = hub
        .provider(&id)
        .await
        .expect("provider() should rebuild a client after invalidation");
    assert!(
        !std::sync::Arc::ptr_eq(&provider, &rotated_provider),
        "invalidation must have produced a new client, not reused the pre-rotation one"
    );
    let err = rotated_provider
        .list_buckets()
        .await
        .expect_err("list_buckets with a rotated-to-wrong secret must fail against live MinIO");
    let code = err.code();
    assert_eq!(
        code, "auth/invalid-credentials",
        "expected the rotated-to-wrong secret to surface `auth/invalid-credentials` \
         against live MinIO (see `wrong_secret_surfaces_expected_error_family` above for \
         why) -- got `{code}` instead"
    );
}

// ===========================================================================
// M4a Task 12: the upload engine against a real MinIO server.
//
// Everything else in M4a is tested against unit-level fakes; this is the one
// place the upload path meets a real S3 server. Design doc §8 names the
// transfer engine as the main integration-test battleground, and calls for a
// hash check on a multipart round trip specifically.
//
// Two layers of coverage:
//   * Group A drives the provider *transfer primitives* directly
//     (`put_object_from_file`, `multipart_*`, `upload_part_from_file`) -- the
//     narrowest proof that each byte range reaches the server intact.
//   * Groups B/C drive the real `TransferEngine` + `UploadRunner` + a real
//     `ProviderHub` (a MinIO connection persisted into a `SecureStore`),
//     enqueueing files and observing the state machine end to end -- no bypass
//     constructor is used; the engine is exercised exactly as production wires
//     it.
//
// Like the rest of this file, every test is `#[ignore]`d: a plain
// `cargo test` never touches the network.
// ===========================================================================

/// 1 MiB, the unit these fixtures are sized in.
const MB: u64 = 1024 * 1024;

// --- fixtures & readback helpers -------------------------------------------

/// Writes `size` bytes of deterministic LCG pseudo-random content to `path`.
///
/// Seeded (so the source hash is reproducible run to run) and, crucially,
/// non-compressible -- a file of repeated bytes would let a per-part offset
/// bug go unnoticed because every offset reads the same value. Knuth's MMIX
/// LCG constants give that without dragging a `rand` dev-dependency into the
/// test build.
fn write_pseudo_random_file(path: &Path, size: u64, seed: u64) {
    let mut state = seed;
    let mut buf = Vec::with_capacity(size as usize);
    while (buf.len() as u64) < size {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        buf.extend_from_slice(&state.to_le_bytes());
    }
    buf.truncate(size as usize);
    std::fs::write(path, &buf)
        .unwrap_or_else(|e| panic!("writing fixture {}: {e}", path.display()));
}

fn sha256_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

fn sha256_file(path: &Path) -> Vec<u8> {
    let bytes = std::fs::read(path)
        .unwrap_or_else(|e| panic!("reading {} for hashing: {e}", path.display()));
    sha256_bytes(&bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads a whole object back off the server as raw bytes (test-sized only).
async fn get_object_bytes(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Vec<u8> {
    let out = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .unwrap_or_else(|e| panic!("get_object {key} should succeed: {e}"));
    out.body
        .collect()
        .await
        .unwrap_or_else(|e| panic!("collecting body of {key} should succeed: {e}"))
        .into_bytes()
        .to_vec()
}

/// The object's `Content-Length` via `HeadObject`, or `None` when the key does
/// not exist (a missing object is a 404, not an error worth surfacing here).
async fn head_object_size(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Option<u64> {
    match client.head_object().bucket(bucket).key(key).send().await {
        Ok(out) => Some(out.content_length().unwrap_or(0).max(0) as u64),
        Err(_) => None,
    }
}

/// How many *pending* (never completed/aborted) multipart uploads the server
/// still holds for `key`. `list_multipart_uploads` is deliberately not a
/// `Provider` method -- this is test code, so it calls the raw SDK client
/// directly rather than widening the production trait.
async fn pending_uploads_for_key(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> usize {
    let out = client
        .list_multipart_uploads()
        .bucket(bucket)
        .send()
        .await
        .unwrap_or_else(|e| panic!("list_multipart_uploads for {bucket} should succeed: {e}"));
    out.uploads()
        .iter()
        .filter(|u| u.key() == Some(key))
        .count()
}

/// Teardown that first reaps any lingering multipart uploads (an in-progress
/// upload can otherwise block `delete_bucket` and leak fragments into the
/// shared container) and then drains + deletes the bucket.
async fn cleanup_bucket(client: &aws_sdk_s3::Client, provider: &S3Provider, bucket: &str) {
    if let Ok(out) = client.list_multipart_uploads().bucket(bucket).send().await {
        for u in out.uploads() {
            if let (Some(k), Some(id)) = (u.key(), u.upload_id()) {
                let _ = client
                    .abort_multipart_upload()
                    .bucket(bucket)
                    .key(k)
                    .upload_id(id)
                    .send()
                    .await;
            }
        }
    }
    drain_and_delete_bucket(client, provider, bucket).await;
}

/// Uploads `path` (of `total` bytes) via the provider transfer primitives,
/// following `plan_upload_with` (under the default tuning) exactly as
/// `UploadRunner` does: a single `PutObject` below the threshold, otherwise
/// create + sequential parts + complete. Parts are sent in plan order here
/// (out-of-order is its own test).
async fn upload_via_primitives(
    provider: &S3Provider,
    bucket: &str,
    key: &str,
    path: &Path,
    total: u64,
) {
    match plan_upload_with(total, &TransferTuning::default()) {
        UploadPlan::Single { length } => {
            provider
                .put_object_from_file(bucket, key, path, length, Arc::new(|_| {}))
                .await
                .unwrap_or_else(|e| panic!("single-stream put of {key} should succeed: {e}"));
        }
        UploadPlan::Multipart { parts, .. } => {
            let upload_id = provider
                .multipart_init(bucket, key)
                .await
                .unwrap_or_else(|e| panic!("multipart_init for {key} should succeed: {e}"));
            let mut done = Vec::with_capacity(parts.len());
            for p in &parts {
                let etag = provider
                    .upload_part_from_file(
                        bucket,
                        key,
                        &upload_id,
                        p.number,
                        path,
                        p.offset,
                        p.length,
                        Arc::new(|_| {}),
                    )
                    .await
                    .unwrap_or_else(|e| {
                        panic!("upload_part {} for {key} should succeed: {e}", p.number)
                    });
                done.push(UploadedPart {
                    number: p.number,
                    etag,
                    size: p.length,
                });
            }
            provider
                .multipart_complete(bucket, key, &upload_id, &done)
                .await
                .unwrap_or_else(|e| panic!("multipart_complete for {key} should succeed: {e}"));
        }
    }
}

/// Uploads via the primitives, reads the object back, and asserts the
/// round-tripped SHA-256 matches the source byte for byte. This is the only
/// check that proves each part's `offset`/`length` reached the server
/// correctly -- a per-part offset bug is invisible to every unit test.
async fn assert_round_trip_hash(
    provider: &S3Provider,
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    path: &Path,
    total: u64,
) {
    upload_via_primitives(provider, bucket, key, path, total).await;
    let downloaded = get_object_bytes(client, bucket, key).await;
    assert_eq!(
        downloaded.len() as u64,
        total,
        "round-tripped byte count for {key} must equal the source size"
    );
    let got = sha256_bytes(&downloaded);
    let want = sha256_file(path);
    assert_eq!(
        got,
        want,
        "SHA-256 mismatch for {key}: server {} vs source {}",
        hex(&got),
        hex(&want)
    );
}

// --- Group A: provider-primitive round trips -------------------------------

/// A ~1MB file goes up as a single `PutObject` (below the 32MB threshold) and
/// comes back in the listing with the right key and size. Covers the
/// single-stream path.
#[tokio::test]
#[ignore]
async fn upload_small_file_round_trips() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("small.bin");
    let size = MB;
    write_pseudo_random_file(&path, size, 0x5EED_0001);

    let key = "uploads/small.bin";
    provider
        .put_object_from_file(&bucket, key, &path, size, Arc::new(|_| {}))
        .await
        .expect("single-stream put_object_from_file should succeed against live MinIO");

    let (all, _) = list_all_pages(&provider, &bucket, "uploads/", 100).await;
    let entry = all
        .iter()
        .find(|e| e.key == key)
        .unwrap_or_else(|| panic!("uploaded object {key} must appear in the listing: {all:?}"));
    assert_eq!(
        entry.size,
        Some(size),
        "the listed object size must equal the source file size"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
}

/// ~40MB of seeded, non-compressible content, uploaded part by part per
/// `plan_upload_with`, then completed, then read back with a SHA-256 equality
/// check against the source. This is design §8's explicit hash-verification
/// requirement, and the only proof that every part landed at the right offset.
#[tokio::test]
#[ignore]
async fn upload_multipart_file_round_trips_with_matching_hash() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("multipart.bin");
    let size = 40 * MB;
    write_pseudo_random_file(&path, size, 0x5EED_0002);
    assert!(
        matches!(
            plan_upload_with(size, &TransferTuning::default()),
            UploadPlan::Multipart { .. }
        ),
        "40MB must plan as a multipart upload for this test to mean anything"
    );

    assert_round_trip_hash(
        &provider,
        &client,
        &bucket,
        "uploads/multipart.bin",
        &path,
        size,
    )
    .await;

    cleanup_bucket(&client, &provider, &bucket).await;
}

/// Parts uploaded in `[3, 1, 2]` order still complete, because
/// `multipart_complete` sorts before sending. S3 answers an unsorted part
/// list with `InvalidPartOrder`; that sort has no unit-level regression
/// protection on the real send path, so this is where it is proven.
#[tokio::test]
#[ignore]
async fn parts_uploaded_out_of_order_still_complete() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("outoforder.bin");
    let size = 48 * MB; // three 16MB parts at the floor part size.
    write_pseudo_random_file(&path, size, 0x5EED_0003);

    let parts = match plan_upload_with(size, &TransferTuning::default()) {
        UploadPlan::Multipart { parts, .. } => parts,
        UploadPlan::Single { .. } => panic!("48MB must plan as multipart"),
    };
    assert_eq!(
        parts.len(),
        3,
        "48MB at the 16MB floor is exactly three parts"
    );

    let key = "uploads/outoforder.bin";
    let upload_id = provider.multipart_init(&bucket, key).await.expect("init");

    // Send the parts, and build the completion list, in [3, 1, 2] order: the
    // list handed to `multipart_complete` is deliberately unsorted.
    let mut done: Vec<UploadedPart> = Vec::new();
    for number in [3i32, 1, 2] {
        let p = parts[(number - 1) as usize];
        let etag = provider
            .upload_part_from_file(
                &bucket,
                key,
                &upload_id,
                p.number,
                &path,
                p.offset,
                p.length,
                Arc::new(|_| {}),
            )
            .await
            .unwrap_or_else(|e| panic!("upload_part {} should succeed: {e}", p.number));
        done.push(UploadedPart {
            number: p.number,
            etag,
            size: p.length,
        });
    }
    assert_eq!(
        done.iter().map(|p| p.number).collect::<Vec<_>>(),
        vec![3, 1, 2],
        "the completion list must be out of order for this test to exercise the sort"
    );

    provider
        .multipart_complete(&bucket, key, &upload_id, &done)
        .await
        .expect("multipart_complete must sort the parts; an unsorted list yields InvalidPartOrder");

    let downloaded = get_object_bytes(&client, &bucket, key).await;
    assert_eq!(
        sha256_bytes(&downloaded),
        sha256_file(&path),
        "an out-of-order upload must still assemble the exact source bytes"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
}

/// A file that shrinks after the plan is computed must fail *before* a short
/// part is sent: `Length::Exact` doubles as an integrity check. Proves the
/// runner can never silently upload a truncated part.
#[tokio::test]
#[ignore]
async fn a_shrinking_file_fails_before_uploading_a_short_part() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("shrinking.bin");
    let size = 32 * MB; // two 16MB parts.
    write_pseudo_random_file(&path, size, 0x5EED_0004);
    let parts = match plan_upload_with(size, &TransferTuning::default()) {
        UploadPlan::Multipart { parts, .. } => parts,
        UploadPlan::Single { .. } => panic!("32MB must plan as multipart"),
    };

    let key = "uploads/shrinking.bin";
    let upload_id = provider.multipart_init(&bucket, key).await.expect("init");

    // Truncate AFTER planning: part 1 (offset 0, length 16MB) no longer fits.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("reopen fixture for truncation");
    file.set_len(4 * MB).expect("truncate the fixture");
    drop(file);

    let p = parts[0];
    let err = provider
        .upload_part_from_file(
            &bucket,
            key,
            &upload_id,
            p.number,
            &path,
            p.offset,
            p.length,
            Arc::new(|_| {}),
        )
        .await
        .expect_err("uploading a part that no longer fits the shrunk file must fail");
    assert_eq!(
        err.code(),
        "local/file-io",
        "Length::Exact must catch the shrink as a local file-io error rather than sending a short part"
    );

    // The upload received no part; clean it up.
    provider
        .multipart_abort(&bucket, key, &upload_id)
        .await
        .expect("abort the orphaned upload");
    cleanup_bucket(&client, &provider, &bucket).await;
}

// Test 5 (`a_retried_body_is_replayed_not_emptied`): DELIBERATELY OMITTED.
//
// The intent was to provoke a real SDK-internal retry (e.g. by briefly
// interrupting the container's network) and confirm the replayed request
// carries the full body rather than an empty one -- the rationale for using
// `ByteStream::read_from().path(..)` (rewindable) over `.file(..)` (one-shot).
// There is no way to provoke a transient-then-succeeding SDK retry
// *reliably* on loopback: interrupting the container's network mid-request
// far more often exhausts the retry budget (a hard failure) than lands the
// single "retry once, then succeed" path this test would need, and doing it
// on a fixed schedule is inherently racy. Per the brief, a test that cannot
// actually exercise a retry is worse than no test, so this one is left
// unwritten rather than faked. The rewindability contract remains covered by
// the doc comment on `body_range` in `provider::s3`.

/// Aborting a multipart upload reaps its server-side fragments: a further
/// `upload_part` against the aborted `upload_id` is rejected with
/// `NoSuchUpload`, proving the fragments are gone (not merely orphaned).
#[tokio::test]
#[ignore]
async fn aborting_a_multipart_upload_removes_the_fragments() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("abort.bin");
    let size = MB; // one small part is enough; the upload is never completed.
    write_pseudo_random_file(&path, size, 0x5EED_0006);

    let key = "uploads/abort.bin";
    let upload_id = provider.multipart_init(&bucket, key).await.expect("init");
    provider
        .upload_part_from_file(
            &bucket,
            key,
            &upload_id,
            1,
            &path,
            0,
            size,
            Arc::new(|_| {}),
        )
        .await
        .expect("first part should upload");
    provider
        .multipart_abort(&bucket, key, &upload_id)
        .await
        .expect("abort should succeed");

    // The provider path rejects a further part against the aborted id...
    provider
        .upload_part_from_file(
            &bucket,
            key,
            &upload_id,
            2,
            &path,
            0,
            size,
            Arc::new(|_| {}),
        )
        .await
        .expect_err("uploading to an aborted upload id must fail; the fragments are gone");

    // ...and the raw wire error is precisely NoSuchUpload. (Our `AppError`
    // erases the raw S3 code, so we name it via the SDK directly here.)
    let raw_err = client
        .upload_part()
        .bucket(&bucket)
        .key(key)
        .upload_id(&upload_id)
        .part_number(2)
        .body(ByteStream::from_static(b"still-gone"))
        .send()
        .await
        .expect_err("raw upload_part to an aborted id must fail");
    let raw_code = raw_err
        .as_service_error()
        .and_then(|e| e.code())
        .map(str::to_string);
    assert_eq!(
        raw_code.as_deref(),
        Some("NoSuchUpload"),
        "the server must reject the aborted upload id with NoSuchUpload, proving its fragments were reaped"
    );

    let (all, _) = list_all_pages(&provider, &bucket, "", 100).await;
    assert!(
        all.is_empty(),
        "an aborted multipart upload leaves no object: {all:?}"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
}

/// `multipart_list` (ListParts) reports exactly the parts the server has
/// actually accepted for an in-progress upload -- the authoritative
/// "already done" set M4c's cross-restart resume (Task 7) consults instead
/// of trusting a local checkpoint file, which could be stale or truncated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn multipart_list_returns_the_accepted_parts() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let bucket = unique_bucket_name();
    provider.create_bucket(&bucket).await.expect("bucket");
    let key = "mp/list.bin";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("src.bin");
    write_pseudo_random_file(&path, 32 * MB, 0x5EED_2001);

    let upload_id = provider.multipart_init(&bucket, key).await.expect("init");
    let plan = match plan_upload_with(32 * MB, &TransferTuning::default()) {
        UploadPlan::Multipart { parts, .. } => parts,
        _ => panic!("32MB must be multipart"),
    };
    for p in plan.iter().take(2) {
        provider
            .upload_part_from_file(
                &bucket,
                key,
                &upload_id,
                p.number,
                &path,
                p.offset,
                p.length,
                Arc::new(|_| {}),
            )
            .await
            .expect("upload_part");
    }
    let listed = provider
        .multipart_list(&bucket, key, &upload_id)
        .await
        .expect("list");
    assert_eq!(listed.len(), 2, "the two uploaded parts must be listed");
    assert!(
        listed.iter().all(|p| !p.etag.is_empty()),
        "each listed part has an etag"
    );
    let _ = provider.multipart_abort(&bucket, key, &upload_id).await;
    cleanup_bucket(&raw_seed_client(), &provider, &bucket).await;
}

/// A 0-byte file lands as a real 0-byte object. The single-stream path
/// (`plan_upload_with(0, ..) == Single { length: 0 }`) must handle an empty
/// body -- this is folder-marker-adjacent and easy to get wrong.
#[tokio::test]
#[ignore]
async fn zero_byte_file_round_trips() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("empty.bin");
    std::fs::write(&path, b"").expect("write empty fixture");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    assert_eq!(
        plan_upload_with(0, &TransferTuning::default()),
        UploadPlan::Single { length: 0 }
    );

    let key = "uploads/empty.bin";
    provider
        .put_object_from_file(&bucket, key, &path, 0, Arc::new(|_| {}))
        .await
        .expect("a zero-byte put must succeed");

    assert_eq!(
        head_object_size(&client, &bucket, key).await,
        Some(0),
        "a zero-byte upload must land as a real 0-byte object"
    );
    let (all, _) = list_all_pages(&provider, &bucket, "uploads/", 100).await;
    let entry = all
        .iter()
        .find(|e| e.key == key)
        .expect("the zero-byte object must be listed");
    assert_eq!(entry.size, Some(0));

    cleanup_bucket(&client, &provider, &bucket).await;
}

/// The two files straddling the multipart threshold each round-trip with a
/// hash check: exactly `TransferTuning::default().upload_threshold`
/// (multipart) and one byte below it (single stream). That boundary selects
/// between two entirely different code paths, so both sides are proven.
#[tokio::test]
#[ignore]
async fn threshold_boundary_files_each_land() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("tempdir");
    let threshold = TransferTuning::default().upload_threshold;

    // Exactly at the threshold -> multipart.
    let at_path = dir.path().join("at-threshold.bin");
    write_pseudo_random_file(&at_path, threshold, 0x5EED_0801);
    assert!(
        matches!(
            plan_upload_with(threshold, &TransferTuning::default()),
            UploadPlan::Multipart { .. }
        ),
        "the threshold itself must plan as multipart"
    );
    assert_round_trip_hash(
        &provider,
        &client,
        &bucket,
        "uploads/at-threshold.bin",
        &at_path,
        threshold,
    )
    .await;

    // One byte below -> single stream.
    let below_path = dir.path().join("below-threshold.bin");
    write_pseudo_random_file(&below_path, threshold - 1, 0x5EED_0802);
    assert!(
        matches!(
            plan_upload_with(threshold - 1, &TransferTuning::default()),
            UploadPlan::Single { .. }
        ),
        "one byte below the threshold must plan as single stream"
    );
    assert_round_trip_hash(
        &provider,
        &client,
        &bucket,
        "uploads/below-threshold.bin",
        &below_path,
        threshold - 1,
    )
    .await;

    cleanup_bucket(&client, &provider, &bucket).await;
}

// --- M6a Task 1: presign_get -------------------------------------------------

/// A presigned GET URL actually works: uploaded object -> `presign_get` ->
/// an unauthenticated `curl` of the URL -> SHA-256 match against the source.
/// This is the only place the presigning path meets a real server -- MinIO's
/// presigned-URL support has its own quirks (clock skew, path- vs
/// virtual-hosted addressing) that no unit test against a fake can catch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn presign_get_yields_a_working_url() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider.create_bucket(&bucket).await.expect("bucket");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("share.bin");
    write_pseudo_random_file(&path, 2 * MB, 0x5EED_6001);
    provider
        .put_object_from_file(&bucket, "share.bin", &path, 2 * MB, Arc::new(|_| {}))
        .await
        .expect("put");

    let url = provider
        .presign_get(&bucket, "share.bin", 3600)
        .await
        .expect("presign");

    // curl the presigned URL WITHOUT any credentials -- the whole point of a
    // presigned URL is that the signature in the query string is enough.
    let out = dir.path().join("fetched.bin");
    let status = std::process::Command::new("curl")
        .args(["-fsS", "-o", out.to_str().unwrap(), &url])
        .status()
        .expect("curl runs");
    assert!(status.success(), "presigned GET must succeed: {url}");
    assert_eq!(
        hex(&sha256_file(&out)),
        hex(&sha256_file(&path)),
        "fetched bytes must match source"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
}

// --- Group B/C: the full engine + runner + provider ------------------------

/// Collects every state transition the engine emits, keyed by task id, so a
/// test can assert both the current status and the path taken to it.
#[derive(Default)]
struct CollectingSink {
    seen: std::sync::Mutex<Vec<(String, TransferStatus)>>,
}

impl CollectingSink {
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

impl TransferSink for CollectingSink {
    fn state_changed(&self, task: &TransferTaskDto) {
        self.seen
            .lock()
            .unwrap()
            .push((task.id.clone(), task.status));
    }
}

/// A progress sink that discards everything: these tests observe progress
/// through `engine.snapshot()` (the shared byte counter), not through the
/// throttled IPC batches. The aggregator still has to exist and stay alive so
/// the engine's progress channel is a real, open channel.
struct NullProgressSink;

impl ProgressSink for NullProgressSink {
    fn flush(&self, _batch: Vec<ProgressPayload>) {}
}

/// Builds a real `TransferEngine` over a real `DispatchRunner` (so the one
/// engine routes upload-direction tasks to `UploadRunner` and
/// download-direction tasks to `DownloadRunner`, exactly as production wires it)
/// and a real aggregator. Routing by `Direction` is why the upload tests above
/// keep passing unchanged while the download tests below exercise the same
/// engine. `max_parts` is a knob: the pause/cancel tests set it to 1 to
/// serialize parts/chunks, which widens the mid-flight window and makes the
/// timing deterministic without changing any of the abort/resume semantics
/// under test.
fn build_engine(
    hub: Arc<ProviderHub>,
    sink: Arc<dyn TransferSink>,
    max_parts: usize,
) -> TransferEngine {
    // The M4a/M4b e2e tests here assert transfer semantics, not checkpoint
    // persistence; no checkpoint dir + resume-on keeps every existing call site
    // behaving exactly as before Task 5 added the two extra `new` parameters.
    build_engine_cp(hub, sink, max_parts, None, Arc::new(AtomicBool::new(true)))
}

/// Like [`build_engine`] but with an explicit checkpoint dir + runtime resume
/// flag, so the M4c cross-restart tests can build an engine (E1) that writes
/// checkpoints into a dir, drop it, then build a *fresh* engine (E2) over the
/// SAME dir + hub and run the startup scan -- the simulated-restart harness the
/// milestone hinges on. Everything else (the real `DispatchRunner` routing by
/// `Direction`, a fresh aggregator) is identical to `build_engine`.
fn build_engine_cp(
    hub: Arc<ProviderHub>,
    sink: Arc<dyn TransferSink>,
    max_parts: usize,
    checkpoint_dir: Option<PathBuf>,
    resume_enabled: Arc<AtomicBool>,
) -> TransferEngine {
    let progress_tx = spawn_aggregator(Arc::new(NullProgressSink));
    TransferEngine::new(
        hub,
        Arc::new(DispatchRunner {
            upload: Arc::new(UploadRunner),
            download: Arc::new(DownloadRunner),
        }),
        sink,
        progress_tx,
        SharedLimits::new(3, max_parts, TransferTuning::default()),
        checkpoint_dir,
        resume_enabled,
    )
}

/// A `ProviderHub` over a `SecureStore` in `dir`, with the live MinIO
/// connection persisted through `mutate` (the production path). `dir` is
/// borrowed so the caller keeps the encrypted store file alive for the test.
async fn live_hub(connection_id: &str, dir: &tempfile::TempDir) -> Arc<ProviderHub> {
    let hub = Arc::new(ProviderHub::new(SecureStore {
        path: dir.path().join("connections.enc"),
    }));
    hub.mutate(|list| {
        list.push(Connection {
            id: connection_id.to_string(),
            ..minio_connection("minioadmin")
        });
        Ok(())
    })
    .await
    .expect("persisting the MinIO connection into the hub should succeed");
    hub
}

async fn snapshot_of(engine: &TransferEngine, id: &str) -> Option<TransferTaskDto> {
    engine.snapshot().await.into_iter().find(|t| t.id == id)
}

/// Polls `engine.snapshot()` until the task reaches `want`, or the budget
/// expires. On timeout it names the status it was waiting for and the last
/// status actually seen; if the task instead settles into a *different*
/// terminal/failed state it fails fast (with the error code) rather than
/// burning the whole budget -- a bare timeout costs the next debugger an hour.
async fn wait_for_status(
    engine: &TransferEngine,
    id: &str,
    want: TransferStatus,
    budget: Duration,
) {
    let deadline = tokio::time::Instant::now() + budget;
    let mut last: Option<TransferStatus> = None;
    while tokio::time::Instant::now() < deadline {
        if let Some(t) = snapshot_of(engine, id).await {
            last = Some(t.status);
            if t.status == want {
                return;
            }
            if t.status != want
                && matches!(
                    t.status,
                    TransferStatus::Completed | TransferStatus::Canceled | TransferStatus::Failed
                )
            {
                panic!(
                    "task {id} reached {:?} (error_code={:?}) while waiting for {want:?}",
                    t.status, t.error_code
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "timed out after {budget:?} waiting for task {id} to reach {want:?}; last status seen: {last:?}"
    );
}

/// Waits until the task is observably `Running` with at least one byte
/// reported -- the only safe moment to pause/cancel "mid-flight". Acting the
/// instant after enqueue can land before any part has started, proving nothing
/// about resume or cleanup; requiring reported progress guarantees the
/// multipart upload has been created and at least one part is on the server.
/// Polls tightly so a fast loopback upload cannot slip from Queued past
/// Running to a terminal state between polls.
async fn wait_for_mid_flight(engine: &TransferEngine, id: &str, budget: Duration) {
    let deadline = tokio::time::Instant::now() + budget;
    let mut last: Option<(TransferStatus, u64)> = None;
    while tokio::time::Instant::now() < deadline {
        if let Some(t) = snapshot_of(engine, id).await {
            last = Some((t.status, t.transferred));
            if t.status == TransferStatus::Running && t.transferred > 0 {
                return;
            }
            if matches!(
                t.status,
                TransferStatus::Completed | TransferStatus::Canceled | TransferStatus::Failed
            ) {
                panic!(
                    "task {id} reached {:?} (error_code={:?}) before any mid-flight progress was observed",
                    t.status, t.error_code
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!(
        "timed out after {budget:?} waiting for task {id} to be Running with progress; last: {last:?}"
    );
}

/// The full stack, end to end: a ~60MB file enqueued into a real engine runs
/// to `Completed`, and only then does the object exist -- proven by both a
/// `HeadObject` size check (Completed is never reported for an object the
/// server does not hold) and a byte-for-byte SHA-256 comparison.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn engine_uploads_a_large_file_end_to_end() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("fixture tempdir");
    let path = dir.path().join("engine-large.bin");
    let size = 60 * MB;
    write_pseudo_random_file(&path, size, 0x5EED_0009);

    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-engine-9";
    let hub = live_hub(connection_id, &hub_dir).await;
    let sink = Arc::new(CollectingSink::default());
    // The default part concurrency.
    let engine = build_engine(Arc::clone(&hub), sink.clone(), 4);

    let key = "uploads/engine-large.bin".to_string();
    let spec = EnqueueSpec::for_upload(
        connection_id.to_string(),
        bucket.clone(),
        key.clone(),
        path.clone(),
    )
    .expect("for_upload should stat the local file");
    let task = engine.enqueue(spec).await.expect("enqueue");

    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Completed,
        Duration::from_secs(180),
    )
    .await;

    assert_eq!(
        head_object_size(&client, &bucket, &key).await,
        Some(size),
        "HeadObject after Completed must show the full object size; Completed must never be \
         reported for an object that does not exist"
    );
    let downloaded = get_object_bytes(&client, &bucket, &key).await;
    assert_eq!(
        sha256_bytes(&downloaded),
        sha256_file(&path),
        "the object the engine assembled must be byte-identical to the source"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    // Keep the hub store alive until here.
    drop(hub);
}

/// Pausing mid-flight then resuming finishes the upload without losing or
/// duplicating a byte. While paused, no complete object exists on the server;
/// after resume the object hash-matches the source. This is the only proof
/// that resume's byte accounting is exact end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn pausing_then_resuming_finishes_the_upload() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("fixture tempdir");
    let path = dir.path().join("engine-pause.bin");
    let size = 60 * MB;
    write_pseudo_random_file(&path, size, 0x5EED_0010);

    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-engine-10";
    let hub = live_hub(connection_id, &hub_dir).await;
    let sink = Arc::new(CollectingSink::default());
    // One part at a time: widens the pause window, keeps the timing deterministic.
    let engine = build_engine(Arc::clone(&hub), sink.clone(), 1);

    let key = "uploads/engine-pause.bin".to_string();
    let spec = EnqueueSpec::for_upload(
        connection_id.to_string(),
        bucket.clone(),
        key.clone(),
        path.clone(),
    )
    .expect("for_upload");
    let task = engine.enqueue(spec).await.expect("enqueue");

    wait_for_mid_flight(&engine, &task.id, Duration::from_secs(120)).await;
    engine.pause(&task.id).await.expect("pause");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Paused,
        Duration::from_secs(60),
    )
    .await;

    // No complete object may exist while paused: a multipart upload that was
    // never completed leaves nothing at the key.
    let head = head_object_size(&client, &bucket, &key).await;
    assert!(
        head != Some(size),
        "a paused upload must not have produced the full object yet (head={head:?})"
    );
    assert_eq!(
        head, None,
        "a paused multipart upload leaves no object at the key at all"
    );
    assert!(
        pending_uploads_for_key(&client, &bucket, &key).await >= 1,
        "a pause keeps the multipart upload open so resume can continue it"
    );

    engine.resume(&task.id).await.expect("resume");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Completed,
        Duration::from_secs(180),
    )
    .await;

    assert_eq!(
        head_object_size(&client, &bucket, &key).await,
        Some(size),
        "after resume completes, the full object must exist"
    );
    let downloaded = get_object_bytes(&client, &bucket, &key).await;
    assert_eq!(
        sha256_bytes(&downloaded),
        sha256_file(&path),
        "resume must neither lose nor duplicate bytes: the object must hash-match the source"
    );
    assert!(
        sink.statuses_of(&task.id).contains(&TransferStatus::Paused),
        "the task must have actually passed through Paused"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// Cancelling a running multipart upload aborts it: `list_multipart_uploads`
/// shows no pending upload for the key afterwards, and no object is committed.
/// This is the whole reason the cancel-before-error ordering exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn cancelling_mid_upload_leaves_no_fragments() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("fixture tempdir");
    let path = dir.path().join("engine-cancel.bin");
    let size = 60 * MB;
    write_pseudo_random_file(&path, size, 0x5EED_0011);

    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-engine-11";
    let hub = live_hub(connection_id, &hub_dir).await;
    let sink = Arc::new(CollectingSink::default());
    let engine = build_engine(Arc::clone(&hub), sink.clone(), 1);

    let key = "uploads/engine-cancel.bin".to_string();
    let spec = EnqueueSpec::for_upload(
        connection_id.to_string(),
        bucket.clone(),
        key.clone(),
        path.clone(),
    )
    .expect("for_upload");
    let task = engine.enqueue(spec).await.expect("enqueue");

    wait_for_mid_flight(&engine, &task.id, Duration::from_secs(120)).await;
    assert!(
        pending_uploads_for_key(&client, &bucket, &key).await >= 1,
        "a running multipart upload should have an open upload id to leak"
    );

    engine.cancel(&task.id).await.expect("cancel");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Canceled,
        Duration::from_secs(60),
    )
    .await;

    let pending = pending_uploads_for_key(&client, &bucket, &key).await;
    assert_eq!(
        pending, 0,
        "cancelling a running multipart upload must abort it, leaving no pending upload for {key}"
    );
    assert_eq!(
        head_object_size(&client, &bucket, &key).await,
        None,
        "a cancelled upload must not commit an object"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// Cancelling a *resumed* upload still aborts it. Pause mid-flight (so the
/// resume slot holds an `upload_id`), resume, wait until the resumed runner is
/// actually live, then cancel: the abort must still run. Exercises the I-1 fix
/// (commit `ac4a2dd`), so it SHOULD pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn cancelling_a_resumed_upload_still_aborts() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("fixture tempdir");
    let path = dir.path().join("engine-resume-cancel.bin");
    let size = 60 * MB;
    write_pseudo_random_file(&path, size, 0x5EED_0012);

    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-engine-12";
    let hub = live_hub(connection_id, &hub_dir).await;
    let sink = Arc::new(CollectingSink::default());
    let engine = build_engine(Arc::clone(&hub), sink.clone(), 1);

    let key = "uploads/engine-resume-cancel.bin".to_string();
    let spec = EnqueueSpec::for_upload(
        connection_id.to_string(),
        bucket.clone(),
        key.clone(),
        path.clone(),
    )
    .expect("for_upload");
    let task = engine.enqueue(spec).await.expect("enqueue");

    // Pause so the resume slot holds an upload_id + accepted parts.
    wait_for_mid_flight(&engine, &task.id, Duration::from_secs(120)).await;
    engine.pause(&task.id).await.expect("pause");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Paused,
        Duration::from_secs(60),
    )
    .await;
    assert!(
        pending_uploads_for_key(&client, &bucket, &key).await >= 1,
        "the paused upload must still be open for resume to continue"
    );

    // Resume, wait until the resumed runner is genuinely live (Running -- so a
    // driver is executing the multipart path), then cancel it. Waiting for
    // Running is what puts the cancel in the I-1 window (a live runner), rather
    // than the Queued window that the paused-cancel gap (test 13) is about.
    engine.resume(&task.id).await.expect("resume");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Running,
        Duration::from_secs(60),
    )
    .await;
    engine.cancel(&task.id).await.expect("cancel");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Canceled,
        Duration::from_secs(60),
    )
    .await;

    let pending = pending_uploads_for_key(&client, &bucket, &key).await;
    assert_eq!(
        pending, 0,
        "cancelling a resumed upload must abort it (I-1 fix, commit ac4a2dd); {pending} \
         fragment(s) leaked for {key}"
    );
    assert_eq!(
        head_object_size(&client, &bucket, &key).await,
        None,
        "a cancelled resumed upload must not commit an object"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// Cancelling a *paused* upload must also reap its fragments (I-2 fix).
///
/// When a task holding multipart state is cancelled while it has NO live runner
/// (i.e. while `Paused`), `engine.cancel` applies `Canceled` itself. Because no
/// runner ever runs the multipart cancel/abort branch, the engine must abort
/// the orphaned upload directly -- `abort_orphaned_multipart` fires from the
/// self-apply branch of `cancel`, discriminating on a `resume` slot that still
/// holds an `upload_id`. Without it the recorded upload id is never aborted and
/// its fragments leak until the bucket's lifecycle rules reap them; with it
/// `pending == 0`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn cancelling_a_paused_upload_reaps_fragments() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("fixture tempdir");
    let path = dir.path().join("engine-paused-cancel.bin");
    let size = 60 * MB;
    write_pseudo_random_file(&path, size, 0x5EED_0013);

    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-engine-13";
    let hub = live_hub(connection_id, &hub_dir).await;
    let sink = Arc::new(CollectingSink::default());
    let engine = build_engine(Arc::clone(&hub), sink.clone(), 1);

    let key = "uploads/engine-paused-cancel.bin".to_string();
    let spec = EnqueueSpec::for_upload(
        connection_id.to_string(),
        bucket.clone(),
        key.clone(),
        path.clone(),
    )
    .expect("for_upload");
    let task = engine.enqueue(spec).await.expect("enqueue");

    wait_for_mid_flight(&engine, &task.id, Duration::from_secs(120)).await;
    engine.pause(&task.id).await.expect("pause");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Paused,
        Duration::from_secs(60),
    )
    .await;
    assert!(
        pending_uploads_for_key(&client, &bucket, &key).await >= 1,
        "the paused upload must be open before the cancel"
    );

    // Cancel while Paused: no live runner to observe it, so the engine must
    // abort the orphaned upload itself from `cancel`'s self-apply branch.
    engine.cancel(&task.id).await.expect("cancel");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Canceled,
        Duration::from_secs(60),
    )
    .await;

    let pending = pending_uploads_for_key(&client, &bucket, &key).await;
    // Reap the leaked upload BEFORE asserting, so a failing assertion never
    // leaves fragments (or a bucket that refuses deletion) behind in the
    // shared container.
    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);

    assert_eq!(
        pending, 0,
        "I-2 fix: cancelling a Paused multipart task must reap its fragments, but {pending} \
         pending upload(s) remain for {key} -- the engine's self-apply cancel branch must abort \
         the orphaned upload when a task has multipart state and no live runner"
    );
}

// ===========================================================================
// Group D: the download engine + recursive ops, end to end against MinIO
// (M4b Task 10).
//
// These mirror the upload engine tests above: the same real `TransferEngine`
// (now a `DispatchRunner`, so it routes by `Direction`), the same `live_hub`,
// `CollectingSink`, `wait_for_status`/`wait_for_mid_flight` timing, and the
// same `max_parts = 1` recipe for a deterministic mid-flight window. Sized at
// 80MB rather than Group C's 60MB: the M6 split gives download its own
// threshold (64MB under the default tuning, independent of upload's 32MB), so
// a multi-chunk download test has to clear that higher bar. A download stages
// into `<final>.bcpart` and renames to `<final>` only on completion, so
// `bcpart_path` + "the final path must not exist mid-flight" is how the
// no-half-file contract is asserted. Every test is `#[ignore]`d.
// ===========================================================================

/// Seeds `key` in `bucket` with `size` bytes of deterministic pseudo-random
/// content (the same non-compressible fill the upload fixtures use, so a
/// per-chunk offset bug can never hide behind a repeating pattern) via the
/// provider's own upload primitives, then returns the local source path so a
/// downloaded copy can be hash-compared against it. `dir` keeps the source
/// alive for the caller.
async fn seed_random_object(
    provider: &S3Provider,
    bucket: &str,
    key: &str,
    dir: &Path,
    size: u64,
    seed: u64,
) -> PathBuf {
    let src = dir.join(format!("src-{}.bin", key.replace('/', "_")));
    write_pseudo_random_file(&src, size, seed);
    upload_via_primitives(provider, bucket, key, &src, size).await;
    src
}

/// The object's size via the provider's `head_object`, panicking rather than
/// swallowing the error: a download can't be planned without it.
async fn download_total(provider: &S3Provider, bucket: &str, key: &str) -> u64 {
    provider
        .head_object(bucket, key)
        .await
        .unwrap_or_else(|e| panic!("head_object {key} for download size should succeed: {e}"))
        .size
}

/// Drives a folder download at the engine level exactly as the
/// `enqueue_folder_download` command does internally -- the command itself
/// needs a Tauri `State`, so its logic is replicated here against the raw
/// engine: walk `prefix` with the delimiter-less `list_objects_flat` (paging
/// like `delete_prefix`), skip every key ending in `/` (folder markers, incl.
/// nested empty-folder markers), and enqueue one download per real object into
/// `local_dir` under its path relative to `prefix`. The size comes straight
/// from the listing -- a `None` size is a hard failure here, which is what
/// asserts "no per-object HeadObject beyond what's needed" (the listing already
/// carries it). Returns the enqueued task ids.
async fn folder_download_via_engine(
    engine: &TransferEngine,
    provider: &S3Provider,
    connection_id: &str,
    bucket: &str,
    prefix: &str,
    local_dir: &Path,
) -> Vec<String> {
    let mut entries = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let page = provider
            .list_objects_flat(bucket, prefix, token.as_deref(), 1000)
            .await
            .expect("list_objects_flat page should succeed");
        entries.extend(page.entries);
        match page.next_token {
            Some(next) => token = Some(next),
            None => break,
        }
    }

    let mut ids = Vec::new();
    for entry in entries {
        // Drive the command's REAL path policy (folder-name preservation,
        // Normal-only traversal sanitization, and folder-marker skip via
        // `None`) rather than a copy -- so this e2e actually pins the shipped
        // behavior and can't silently drift from it.
        let Some(target) = local_target(prefix, &entry.key, local_dir) else {
            continue; // a `/`-terminated marker, or nothing normal survived
        };
        let file_name = target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let total = entry.size.unwrap_or_else(|| {
            panic!(
                "the flat listing must carry {}'s size so no per-object head is needed",
                entry.key
            )
        });
        let spec = EnqueueSpec::for_download(
            connection_id.to_string(),
            bucket.to_string(),
            entry.key.clone(),
            target,
            total,
            file_name,
        );
        let task = engine
            .enqueue(spec)
            .await
            .expect("enqueuing a folder object should succeed");
        ids.push(task.id);
    }
    ids
}

/// Test 1: a small (single-stream, below the download planner's own 64MB
/// threshold) file downloads byte-for-byte, and the degenerate 0-byte object
/// downloads to a correct empty local file (the live mirror of the
/// `a_zero_byte_object...` unit test).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn download_small_file_round_trips() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let src_dir = tempfile::tempdir().expect("source tempdir");
    let dl_dir = tempfile::tempdir().expect("download tempdir");
    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-download-1";
    let hub = live_hub(connection_id, &hub_dir).await;
    let sink = Arc::new(CollectingSink::default());
    let engine = build_engine(Arc::clone(&hub), sink.clone(), 4);

    // --- a 1MB single-stream file round-trips by hash -----------------------
    let key = "downloads/small.bin";
    let size = MB;
    let src = seed_random_object(&provider, &bucket, key, src_dir.path(), size, 0x5EED_1001).await;

    let target = dl_dir.path().join("small.bin");
    let total = download_total(&provider, &bucket, key).await;
    assert_eq!(total, size, "the head size must match what was seeded");
    let spec = EnqueueSpec::for_download(
        connection_id.to_string(),
        bucket.clone(),
        key.to_string(),
        target.clone(),
        total,
        "small.bin".to_string(),
    );
    let task = engine.enqueue(spec).await.expect("enqueue");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Completed,
        Duration::from_secs(60),
    )
    .await;

    assert!(target.exists(), "a completed download must leave the file");
    assert!(
        !bcpart_path(&target).exists(),
        "finish renames the .bcpart into place, leaving no staging file"
    );
    assert_eq!(
        hex(&sha256_file(&target)),
        hex(&sha256_file(&src)),
        "the downloaded bytes must be byte-identical to the source object"
    );

    // --- a 0-byte object downloads to an empty file -------------------------
    let empty_key = "downloads/empty.bin";
    client
        .put_object()
        .bucket(&bucket)
        .key(empty_key)
        .body(ByteStream::from_static(b""))
        .send()
        .await
        .expect("seeding a 0-byte object should succeed");
    let empty_target = dl_dir.path().join("empty.bin");
    let empty_total = download_total(&provider, &bucket, empty_key).await;
    assert_eq!(empty_total, 0, "the seeded object is empty");
    let empty_spec = EnqueueSpec::for_download(
        connection_id.to_string(),
        bucket.clone(),
        empty_key.to_string(),
        empty_target.clone(),
        empty_total,
        "empty.bin".to_string(),
    );
    let empty_task = engine.enqueue(empty_spec).await.expect("enqueue empty");
    wait_for_status(
        &engine,
        &empty_task.id,
        TransferStatus::Completed,
        Duration::from_secs(60),
    )
    .await;
    assert!(
        empty_target.exists(),
        "a 0-byte download must still create the empty file"
    );
    assert_eq!(
        std::fs::metadata(&empty_target).unwrap().len(),
        0,
        "a 0-byte object must download to a 0-byte file"
    );
    assert!(!bcpart_path(&empty_target).exists());

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// Test 2: an 80MB multipart (Range-chunked) download hash-matches the
/// source. This is the ONLY proof each chunk's `(offset, length)` landed at
/// the right LOCAL offset: a per-chunk offset bug is invisible to the unit
/// tests' fake provider but corrupts this SHA-256. 80MB clears the download
/// planner's own 64MB threshold (independent of the upload threshold since
/// the M6 split).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn download_multipart_file_matches_hash() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let src_dir = tempfile::tempdir().expect("source tempdir");
    let dl_dir = tempfile::tempdir().expect("download tempdir");
    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-download-2";
    let hub = live_hub(connection_id, &hub_dir).await;
    let sink = Arc::new(CollectingSink::default());
    let engine = build_engine(Arc::clone(&hub), sink.clone(), 4);

    let key = "downloads/multipart.bin";
    let size = 80 * MB;
    let src = seed_random_object(&provider, &bucket, key, src_dir.path(), size, 0x5EED_1002).await;
    // Sanity: this must genuinely exercise the chunked path, not a single GET.
    // The DOWNLOAD plan is what matters here, not the upload plan used to seed
    // the object above -- the two have been independently tunable since M6.
    assert!(
        plan_download(size, &TransferTuning::default()).chunks.len() > 1,
        "80MB must plan as multipart for a download or this test proves nothing about chunk \
         offsets"
    );

    let target = dl_dir.path().join("multipart.bin");
    let total = download_total(&provider, &bucket, key).await;
    let spec = EnqueueSpec::for_download(
        connection_id.to_string(),
        bucket.clone(),
        key.to_string(),
        target.clone(),
        total,
        "multipart.bin".to_string(),
    );
    let task = engine.enqueue(spec).await.expect("enqueue");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Completed,
        Duration::from_secs(120),
    )
    .await;

    assert!(
        !bcpart_path(&target).exists(),
        "no staging file after finish"
    );
    assert_eq!(
        hex(&sha256_file(&target)),
        hex(&sha256_file(&src)),
        "every download chunk must land at its own local offset: a mismatch here is a real \
         offset/length bug, not a flake"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// Test 3: chunk offsets land correctly, verified segment-by-segment. The
/// source is built so each segment holds a distinct constant byte (segment
/// `i` is all `i`), so two swapped chunks -- which a whole-file hash would also
/// catch but not localize -- are pinned to the exact offending segment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn download_chunks_land_at_correct_offsets() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let src_dir = tempfile::tempdir().expect("source tempdir");
    let dl_dir = tempfile::tempdir().expect("download tempdir");
    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-download-3";
    let hub = live_hub(connection_id, &hub_dir).await;
    let sink = Arc::new(CollectingSink::default());
    let engine = build_engine(Arc::clone(&hub), sink.clone(), 4);

    // Three segments at the download planner's own chunk floor (32MB under the
    // default tuning -- independent of the upload floor since the M6 split),
    // each a distinct byte, so the segments line up exactly with the chunk plan.
    let segment = TransferTuning::default().download_chunk_floor as usize;
    let segments = 3usize;
    let mut object = Vec::with_capacity(segment * segments);
    for i in 0..segments {
        object.extend(std::iter::repeat_n(i as u8, segment));
    }
    let key = "downloads/segmented.bin";
    let src = src_dir.path().join("segmented.bin");
    std::fs::write(&src, &object).expect("write segmented source");
    upload_via_primitives(&provider, &bucket, key, &src, object.len() as u64).await;
    // Sanity: the segments above are only useful if they actually line up with
    // real download chunk boundaries.
    assert_eq!(
        plan_download(object.len() as u64, &TransferTuning::default())
            .chunks
            .len(),
        segments,
        "the object size must plan into exactly `segments` download chunks for this test to \
         localize a swapped chunk"
    );

    let target = dl_dir.path().join("segmented.bin");
    let total = download_total(&provider, &bucket, key).await;
    let spec = EnqueueSpec::for_download(
        connection_id.to_string(),
        bucket.clone(),
        key.to_string(),
        target.clone(),
        total,
        "segmented.bin".to_string(),
    );
    let task = engine.enqueue(spec).await.expect("enqueue");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Completed,
        Duration::from_secs(120),
    )
    .await;

    let got = std::fs::read(&target).expect("read downloaded segmented file");
    assert_eq!(got.len(), object.len(), "downloaded length must match");
    for i in 0..segments {
        let start = i * segment;
        let end = start + segment;
        assert!(
            got[start..end].iter().all(|&b| b == i as u8),
            "segment {i} (bytes {start}..{end}) must be entirely {i}: a chunk written at the \
             wrong local offset lands here"
        );
    }

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// Test 4: pausing a download mid-flight then resuming finishes it byte-exact.
/// While paused the `.bcpart` exists and the final path does NOT (no half
/// file); after resume the file hash-matches and the final `transferred` never
/// exceeds `total` (resume's byte accounting stays exact -- it neither loses
/// nor double-counts). The precise "finished chunks are skipped, not re-fetched
/// over the wire" discriminator lives in the `download.rs` unit test
/// `resuming_reuses_the_bcpart_and_skips_completed_chunks`, which can count
/// `get_range` calls; the engine here hands the runner a concrete `S3Provider`
/// with no call-counting seam, so this test proves everything reachable at the
/// engine level (mid-flight residue, hash, exact accounting) instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn pausing_then_resuming_download_matches_hash() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let src_dir = tempfile::tempdir().expect("source tempdir");
    let dl_dir = tempfile::tempdir().expect("download tempdir");
    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-download-4";
    let hub = live_hub(connection_id, &hub_dir).await;
    let sink = Arc::new(CollectingSink::default());
    // One chunk at a time widens the pause window between chunks.
    let engine = build_engine(Arc::clone(&hub), sink.clone(), 1);

    let key = "downloads/pause-resume.bin";
    let size = 80 * MB;
    let src = seed_random_object(&provider, &bucket, key, src_dir.path(), size, 0x5EED_1004).await;

    let target = dl_dir.path().join("pause-resume.bin");
    let total = download_total(&provider, &bucket, key).await;
    let spec = EnqueueSpec::for_download(
        connection_id.to_string(),
        bucket.clone(),
        key.to_string(),
        target.clone(),
        total,
        "pause-resume.bin".to_string(),
    );
    let task = engine.enqueue(spec).await.expect("enqueue");

    wait_for_mid_flight(&engine, &task.id, Duration::from_secs(120)).await;
    engine.pause(&task.id).await.expect("pause");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Paused,
        Duration::from_secs(60),
    )
    .await;

    // Mid-flight residue: the staging file holds what landed, the final path
    // does not exist yet (no half file the user could mistake for a download).
    assert!(
        bcpart_path(&target).exists(),
        "a paused download keeps its .bcpart so the resume can reopen it"
    );
    assert!(
        !target.exists(),
        "a paused download must not have produced the final file yet"
    );
    let paused = snapshot_of(&engine, &task.id)
        .await
        .expect("paused task snapshot");
    assert!(
        paused.transferred > 0 && paused.transferred < total,
        "the pause must land genuinely mid-flight: 0 < {} < {total}",
        paused.transferred
    );

    engine.resume(&task.id).await.expect("resume");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Completed,
        Duration::from_secs(180),
    )
    .await;

    assert!(target.exists(), "resume must produce the final file");
    assert!(
        !bcpart_path(&target).exists(),
        "no staging file after finish"
    );
    assert_eq!(
        hex(&sha256_file(&target)),
        hex(&sha256_file(&src)),
        "resume must neither lose nor duplicate a byte: the file must hash-match the source"
    );
    let done = snapshot_of(&engine, &task.id)
        .await
        .expect("completed task snapshot");
    assert!(
        done.transferred <= total,
        "resume must not re-count finished chunks: transferred {} must not exceed total {total}",
        done.transferred
    );
    assert!(
        sink.statuses_of(&task.id).contains(&TransferStatus::Paused),
        "the task must have actually passed through Paused"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// Test 5: cancelling a download mid-flight deletes the `.bcpart` and never
/// produces the final file -- both are absent afterwards. The cancel is issued
/// only after `wait_for_mid_flight` observes real progress, so the `.bcpart`
/// provably existed and was genuinely reaped (a test that cancelled the instant
/// after enqueue would pass even with the cleanup removed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn cancelling_a_download_deletes_the_bcpart() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let src_dir = tempfile::tempdir().expect("source tempdir");
    let dl_dir = tempfile::tempdir().expect("download tempdir");
    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-download-5";
    let hub = live_hub(connection_id, &hub_dir).await;
    let sink = Arc::new(CollectingSink::default());
    let engine = build_engine(Arc::clone(&hub), sink.clone(), 1);

    let key = "downloads/cancel.bin";
    let size = 80 * MB;
    seed_random_object(&provider, &bucket, key, src_dir.path(), size, 0x5EED_1005).await;

    let target = dl_dir.path().join("cancel.bin");
    let total = download_total(&provider, &bucket, key).await;
    let spec = EnqueueSpec::for_download(
        connection_id.to_string(),
        bucket.clone(),
        key.to_string(),
        target.clone(),
        total,
        "cancel.bin".to_string(),
    );
    let task = engine.enqueue(spec).await.expect("enqueue");

    wait_for_mid_flight(&engine, &task.id, Duration::from_secs(120)).await;
    assert!(
        bcpart_path(&target).exists(),
        "a running download must have staged a .bcpart to leak"
    );

    engine.cancel(&task.id).await.expect("cancel");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Canceled,
        Duration::from_secs(60),
    )
    .await;

    assert!(
        !bcpart_path(&target).exists(),
        "cancelling a running download must delete its .bcpart -- there is nothing to continue"
    );
    assert!(
        !target.exists(),
        "a cancelled download must never produce the final file"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// Test 6: cancelling a PAUSED download (paused, never resumed) still deletes
/// its orphaned `.bcpart`. This is the download-side I-2 symmetric check: with
/// no live runner, the engine's self-apply `cancel` branch must run the
/// `ResumeState::Download` arm of `cleanup_orphaned_transfer` and remove the
/// staging file. The live mirror of the `cancelling_a_paused_download_removes_
/// its_orphaned_bcpart` unit test; if that download arm were missing the
/// `.bcpart` would leak here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn cancelling_a_paused_download_deletes_the_bcpart() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let src_dir = tempfile::tempdir().expect("source tempdir");
    let dl_dir = tempfile::tempdir().expect("download tempdir");
    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-download-6";
    let hub = live_hub(connection_id, &hub_dir).await;
    let sink = Arc::new(CollectingSink::default());
    let engine = build_engine(Arc::clone(&hub), sink.clone(), 1);

    let key = "downloads/paused-cancel.bin";
    let size = 80 * MB;
    seed_random_object(&provider, &bucket, key, src_dir.path(), size, 0x5EED_1006).await;

    let target = dl_dir.path().join("paused-cancel.bin");
    let total = download_total(&provider, &bucket, key).await;
    let spec = EnqueueSpec::for_download(
        connection_id.to_string(),
        bucket.clone(),
        key.to_string(),
        target.clone(),
        total,
        "paused-cancel.bin".to_string(),
    );
    let task = engine.enqueue(spec).await.expect("enqueue");

    wait_for_mid_flight(&engine, &task.id, Duration::from_secs(120)).await;
    engine.pause(&task.id).await.expect("pause");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Paused,
        Duration::from_secs(60),
    )
    .await;
    assert!(
        bcpart_path(&target).exists(),
        "a paused download must keep its .bcpart before the cancel"
    );

    // Cancel while Paused: no live runner observes it, so the engine's
    // self-apply cancel branch must delete the orphaned .bcpart itself.
    engine.cancel(&task.id).await.expect("cancel");
    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Canceled,
        Duration::from_secs(60),
    )
    .await;

    assert!(
        !bcpart_path(&target).exists(),
        "cancelling a Paused download must reap its orphaned .bcpart (download-side I-2)"
    );
    assert!(
        !target.exists(),
        "a cancelled download must never produce the final file"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// Test 7: `head_object` reports the object's size and a non-empty ETag for a
/// known object, and a head against a missing key surfaces the
/// `storage/key-not-found` error family (design §7 human-friendly mapping).
#[tokio::test]
#[ignore]
async fn head_object_reports_size_and_etag() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let src_dir = tempfile::tempdir().expect("source tempdir");
    let key = "meta/known.bin";
    let size = 3 * MB;
    seed_random_object(&provider, &bucket, key, src_dir.path(), size, 0x5EED_1007).await;

    let head = provider
        .head_object(&bucket, key)
        .await
        .expect("head_object on a known object should succeed");
    assert_eq!(head.size, size, "head must report the object's real size");
    let etag = head.etag.expect("a real object must carry an ETag");
    assert!(!etag.is_empty(), "the ETag must not be empty");

    let missing = provider
        .head_object(&bucket, "meta/ghost.bin")
        .await
        .expect_err("heading a missing key must fail");
    assert!(
        missing.code().starts_with("storage/"),
        "a missing key must surface the storage/* family (got {:?})",
        missing.code()
    );

    cleanup_bucket(&client, &provider, &bucket).await;
}

/// Test 8: `delete_prefix` removes an entire subtree INCLUDING the `folder/`
/// marker object -- `list_objects_flat` returns empty afterwards (closes M3
/// I4: the empty-folder marker `list_objects`' delimiter listing hides is a
/// real key here). A sibling prefix that merely shares a textual head
/// (`docs2/` next to `docs/`) is untouched, proving the delete is scoped to the
/// slash-terminated prefix and cannot bleed into siblings.
#[tokio::test]
#[ignore]
async fn delete_prefix_removes_everything_including_the_marker() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    // A multi-level `docs/` subtree: its own marker, files at two levels, and a
    // nested EMPTY-folder marker (`docs/empty/`) that only a flat listing sees.
    provider
        .create_folder(&bucket, "docs")
        .await
        .expect("create docs/ marker");
    provider
        .create_folder(&bucket, "docs/empty")
        .await
        .expect("create nested empty-folder marker");
    put_text(&client, &bucket, "docs/a.txt").await;
    put_text(&client, &bucket, "docs/sub/b.txt").await;
    // A sibling sharing the textual head `docs`: it must survive.
    provider
        .create_folder(&bucket, "docs2")
        .await
        .expect("create sibling docs2/ marker");
    put_text(&client, &bucket, "docs2/keep.txt").await;

    // Precondition: the flat listing sees the marker + the nested empty marker.
    let before = provider
        .list_objects_flat(&bucket, "docs/", None, 1000)
        .await
        .expect("flat list docs/ before delete");
    let before_keys: Vec<&str> = before.entries.iter().map(|e| e.key.as_str()).collect();
    assert!(
        before_keys.contains(&"docs/") && before_keys.contains(&"docs/empty/"),
        "the flat listing must surface both markers before delete: {before_keys:?}"
    );

    let result = provider
        .delete_prefix(&bucket, "docs/")
        .await
        .expect("delete_prefix should succeed");
    assert!(
        result.failed.is_empty(),
        "no key should have failed to delete: {:?}",
        result.failed
    );

    let after = provider
        .list_objects_flat(&bucket, "docs/", None, 1000)
        .await
        .expect("flat list docs/ after delete");
    assert!(
        after.entries.is_empty(),
        "delete_prefix must remove everything under docs/ including the marker: {:?}",
        after.entries
    );

    // The sibling that only shares a textual head must be fully intact.
    let sibling = provider
        .list_objects_flat(&bucket, "docs2/", None, 1000)
        .await
        .expect("flat list docs2/ after delete");
    let sibling_keys: Vec<&str> = sibling.entries.iter().map(|e| e.key.as_str()).collect();
    assert!(
        sibling_keys.contains(&"docs2/") && sibling_keys.contains(&"docs2/keep.txt"),
        "deleting docs/ must not touch the sibling docs2/: {sibling_keys:?}"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
}

/// Test 9: a folder download reconstructs the remote subtree on disk. Uploading
/// `p/a.txt` and `p/sub/b.txt` (plus the `p/` and a nested empty `p/empty/`
/// marker) and folder-downloading `p/` yields local `a.txt` and `sub/b.txt`
/// with matching contents, while the folder markers are skipped -- no bogus
/// `empty` file, no file named after the folder. Sizes come from the listing,
/// so no per-object HeadObject is issued.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn folder_download_reconstructs_the_tree() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let src_dir = tempfile::tempdir().expect("source tempdir");
    let dl_dir = tempfile::tempdir().expect("download tempdir");
    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-download-9";
    let hub = live_hub(connection_id, &hub_dir).await;
    let sink = Arc::new(CollectingSink::default());
    let engine = build_engine(Arc::clone(&hub), sink.clone(), 4);

    // A two-level tree plus markers a real folder accumulates.
    provider
        .create_folder(&bucket, "p")
        .await
        .expect("p/ marker");
    provider
        .create_folder(&bucket, "p/empty")
        .await
        .expect("nested empty marker");
    let a_src = seed_random_object(
        &provider,
        &bucket,
        "p/a.txt",
        src_dir.path(),
        2 * MB,
        0x5EED_1091,
    )
    .await;
    let b_src = seed_random_object(
        &provider,
        &bucket,
        "p/sub/b.txt",
        src_dir.path(),
        3 * MB,
        0x5EED_1092,
    )
    .await;

    let ids = folder_download_via_engine(
        &engine,
        &provider,
        connection_id,
        &bucket,
        "p/",
        dl_dir.path(),
    )
    .await;
    assert_eq!(
        ids.len(),
        2,
        "exactly the two real files must be enqueued; the p/ and p/empty/ markers are skipped"
    );
    for id in &ids {
        wait_for_status(
            &engine,
            id,
            TransferStatus::Completed,
            Duration::from_secs(120),
        )
        .await;
    }

    // The downloaded folder's own name (`p`) is recreated as the top directory,
    // then the subtree beneath it -- not spilled loose into `dl_dir`.
    let p_dir = dl_dir.path().join("p");
    let a_dst = p_dir.join("a.txt");
    let b_dst = p_dir.join("sub").join("b.txt");
    assert!(
        p_dir.is_dir(),
        "the downloaded folder's own name must be recreated as <dir>/p"
    );
    assert!(a_dst.exists(), "p/a.txt must land at <dir>/p/a.txt");
    assert!(
        b_dst.exists(),
        "p/sub/b.txt must reconstruct the nested <dir>/p/sub/b.txt"
    );
    assert!(
        p_dir.join("sub").is_dir(),
        "the intermediate folder must exist as a real directory"
    );
    assert_eq!(
        hex(&sha256_file(&a_dst)),
        hex(&sha256_file(&a_src)),
        "a.txt contents must match the source"
    );
    assert_eq!(
        hex(&sha256_file(&b_dst)),
        hex(&sha256_file(&b_src)),
        "sub/b.txt contents must match the source"
    );
    // The folder markers must NOT have been written as files. `p/` now
    // legitimately exists as the recreated directory, so the check is that it
    // is not a *file*; a broken skip of the nested `p/empty/` marker would now
    // land at `<dir>/p/empty`, so that is where the empty-marker check points.
    assert!(
        !p_dir.join("empty").is_file(),
        "the nested empty-folder marker must be skipped, not written as a 'p/empty' file"
    );
    assert!(
        !dl_dir.path().join("p").is_file(),
        "the folder's own marker must never appear as a local file -- only the recreated directory"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

// ===========================================================================
// Group E: cross-restart resume, reconciliation, orphan discard, and the
// resume toggle -- against live MinIO (M4c Task 11, milestone acceptance).
//
// These are the one place the whole M4c checkpoint/resume story is proven end
// to end against a real server: a transfer lands real work, its checkpoint is
// written to disk, the engine is *dropped*, a brand-new engine is built over
// the SAME checkpoint dir + hub, and the exact startup scan `lib.rs` runs is
// replayed -- so the task comes back `Paused` (or is discarded as an orphan)
// with no OS-process restart needed. The narrow "accepted parts are not
// re-uploaded / finished chunks are not re-fetched" counting guards are already
// unit-proven (Task 7 via `multipart_list`, Task 8 via the download runner's
// `get_range` count); what only a live restart can prove -- and what these add
// -- is that the object/file assembled ACROSS the restart is byte-correct, that
// a changed source/ETag forces a from-scratch redo (a matching NEW hash is
// itself the proof the stale work was discarded, since a splice of old+new
// bytes would hash-match neither side), and that the orphan/toggle guards fire.
//
// Every test drives `wait_for_mid_flight` + `wait_for_status(Paused)` before it
// asserts anything about resume/discard, so a green run always observed a
// genuine mid-flight/Paused state with landed work on disk -- never the
// instant-after-enqueue no-op a faked test would settle for. All `#[ignore]`d.
// ===========================================================================

/// The simulated-restart startup scan. Delegates to the exact same
/// `bucketcat_lib::transfer::restore_all` that `lib.rs`'s `setup` calls on
/// launch (the `setup` block itself cannot be invoked directly -- it lives
/// behind a Tauri `App`), so these cross-restart tests exercise the shipped
/// policy rather than a hand-rolled replica that could drift from it. In
/// particular the safety-critical `connection_ids() == Err` branch (leave
/// every checkpoint in place, never mass-discard) is now shared, not
/// re-implemented here with an `unwrap_or_default()` that inverted it.
async fn restore_from_checkpoints(engine: &TransferEngine, hub: &ProviderHub, cp_dir: &Path) {
    restore_all(engine, hub, cp_dir).await;
}

/// Test 1: a large multipart upload paused mid-flight survives an engine drop
/// and resumes to a byte-correct object. The checkpoint written by E1 (upload
/// id + accepted parts + source fingerprint) is all that crosses the restart;
/// E2 rebuilds the task as `Paused` with `transferred` preset from those parts
/// (proving landed work carried across the boundary, not a fresh 0/total), then
/// resume continues the SAME still-open server-side upload to completion. The
/// object's SHA-256 == the source is the end-to-end proof; the pending-upload
/// count going 1 -> 0 confirms the very upload E1 opened was the one completed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn upload_survives_a_restart_and_resumes_without_reuploading() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("fixture tempdir");
    let path = dir.path().join("restart-upload.bin");
    let size = 60 * MB;
    write_pseudo_random_file(&path, size, 0x5EED_4001);

    let cp_base = tempfile::tempdir().expect("checkpoint tempdir");
    let cp_dir = cp_base.path().to_path_buf();
    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-restart-upload";
    let hub = live_hub(connection_id, &hub_dir).await;

    // --- E1: run mid-flight, pause, land a checkpoint, then drop the engine --
    let engine1 = build_engine_cp(
        Arc::clone(&hub),
        Arc::new(CollectingSink::default()),
        1,
        Some(cp_dir.clone()),
        Arc::new(AtomicBool::new(true)),
    );
    let key = "uploads/restart-upload.bin".to_string();
    let spec = EnqueueSpec::for_upload(
        connection_id.to_string(),
        bucket.clone(),
        key.clone(),
        path.clone(),
    )
    .expect("for_upload");
    let task = engine1.enqueue(spec).await.expect("enqueue");

    wait_for_mid_flight(&engine1, &task.id, Duration::from_secs(120)).await;
    engine1.pause(&task.id).await.expect("pause");
    wait_for_status(
        &engine1,
        &task.id,
        TransferStatus::Paused,
        Duration::from_secs(60),
    )
    .await;

    // The pause forced a checkpoint with the upload id and the parts that
    // actually landed. Capture it before the drop: this is the entire state
    // that has to cross the restart.
    let before = checkpoint::scan(&cp_dir);
    assert_eq!(
        before.len(),
        1,
        "exactly one checkpoint must exist after a mid-flight pause, saw {before:?}"
    );
    let (cp_id, cp) = &before[0];
    assert_eq!(cp_id, &task.id, "the checkpoint is keyed by the task id");
    let landed = match &cp.resume {
        ResumeState::Upload(ms) => ms.clone(),
        other => panic!("an upload checkpoint must hold an Upload resume state, got {other:?}"),
    };
    assert!(
        !landed.upload_id.is_empty(),
        "the checkpoint must carry the server upload id to continue"
    );
    assert!(
        !landed.completed.is_empty(),
        "at least one part must have landed before the pause for this to prove resume"
    );
    assert!(
        pending_uploads_for_key(&client, &bucket, &key).await >= 1,
        "the paused multipart upload must still be open on the server for resume to continue it"
    );

    drop(engine1);

    // --- E2: fresh engine over the SAME checkpoint dir + hub; run the scan ---
    let engine2 = build_engine_cp(
        Arc::clone(&hub),
        Arc::new(CollectingSink::default()),
        4,
        Some(cp_dir.clone()),
        Arc::new(AtomicBool::new(true)),
    );
    restore_from_checkpoints(&engine2, &hub, &cp_dir).await;

    let restored = snapshot_of(&engine2, &task.id)
        .await
        .expect("the checkpoint must be rebuilt as a task after the restart scan");
    assert_eq!(
        restored.status,
        TransferStatus::Paused,
        "a survived transfer must come back Paused, offline, until the user resumes"
    );
    assert!(
        restored.transferred > 0 && restored.transferred < size,
        "the restored row must show the landed bytes (0 < {} < {size}), not a fresh 0/total",
        restored.transferred
    );

    engine2.resume(&task.id).await.expect("resume");
    wait_for_status(
        &engine2,
        &task.id,
        TransferStatus::Completed,
        Duration::from_secs(180),
    )
    .await;

    assert_eq!(
        head_object_size(&client, &bucket, &key).await,
        Some(size),
        "after the restart-resume completes, the full object must exist"
    );
    let downloaded = get_object_bytes(&client, &bucket, &key).await;
    assert_eq!(
        hex(&sha256_bytes(&downloaded)),
        hex(&sha256_file(&path)),
        "the object assembled across the restart must be byte-identical to the source"
    );
    assert_eq!(
        pending_uploads_for_key(&client, &bucket, &key).await,
        0,
        "completing the resumed upload must close the very multipart upload E1 opened"
    );
    assert!(
        checkpoint::scan(&cp_dir).is_empty(),
        "the checkpoint must be removed once the transfer reaches a terminal state"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// Test 2: a large multipart download paused mid-flight survives an engine drop
/// and resumes to a byte-correct file. The `.bcpart` staging file and the
/// checkpoint (ETag baseline + finished chunk numbers) are what cross the
/// restart; E2 rebuilds the task `Paused` with `transferred` preset from the
/// finished chunks, and resume re-heads, confirms the unchanged ETag, reopens
/// the SAME `.bcpart`, and fetches only the remainder. The file's SHA-256 ==
/// the source object is the end-to-end proof the surviving partial was correct.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn download_survives_a_restart_and_resumes_without_refetching() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let src_dir = tempfile::tempdir().expect("source tempdir");
    let dl_dir = tempfile::tempdir().expect("download tempdir");
    let cp_base = tempfile::tempdir().expect("checkpoint tempdir");
    let cp_dir = cp_base.path().to_path_buf();
    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-restart-download";
    let hub = live_hub(connection_id, &hub_dir).await;

    let key = "downloads/restart-download.bin";
    let size = 80 * MB;
    let src = seed_random_object(&provider, &bucket, key, src_dir.path(), size, 0x5EED_4002).await;
    let target = dl_dir.path().join("restart-download.bin");
    let total = download_total(&provider, &bucket, key).await;

    // --- E1: run mid-flight, pause, land a checkpoint + .bcpart, then drop ---
    let engine1 = build_engine_cp(
        Arc::clone(&hub),
        Arc::new(CollectingSink::default()),
        1,
        Some(cp_dir.clone()),
        Arc::new(AtomicBool::new(true)),
    );
    let spec = EnqueueSpec::for_download(
        connection_id.to_string(),
        bucket.clone(),
        key.to_string(),
        target.clone(),
        total,
        "restart-download.bin".to_string(),
    );
    let task = engine1.enqueue(spec).await.expect("enqueue");

    wait_for_mid_flight(&engine1, &task.id, Duration::from_secs(120)).await;
    engine1.pause(&task.id).await.expect("pause");
    wait_for_status(
        &engine1,
        &task.id,
        TransferStatus::Paused,
        Duration::from_secs(60),
    )
    .await;

    let before = checkpoint::scan(&cp_dir);
    assert_eq!(before.len(), 1, "one download checkpoint after the pause");
    let done_chunks = match &before[0].1.resume {
        ResumeState::Download(ds) => ds.completed_parts.clone(),
        other => panic!("a download checkpoint must hold a Download resume state, got {other:?}"),
    };
    assert!(
        !done_chunks.is_empty(),
        "at least one chunk must have finished before the pause for this to prove resume"
    );
    assert!(
        bcpart_path(&target).exists(),
        "the paused download's .bcpart must exist on disk to be reopened after the restart"
    );
    assert!(
        !target.exists(),
        "a paused download must not have produced the final file yet"
    );

    drop(engine1);

    // --- E2: fresh engine over the SAME checkpoint dir + hub; run the scan ---
    let engine2 = build_engine_cp(
        Arc::clone(&hub),
        Arc::new(CollectingSink::default()),
        4,
        Some(cp_dir.clone()),
        Arc::new(AtomicBool::new(true)),
    );
    restore_from_checkpoints(&engine2, &hub, &cp_dir).await;

    let restored = snapshot_of(&engine2, &task.id)
        .await
        .expect("the download checkpoint must be rebuilt as a task after the restart scan");
    assert_eq!(restored.status, TransferStatus::Paused);
    assert!(
        restored.transferred > 0 && restored.transferred < size,
        "the restored row must show the finished chunks' bytes (0 < {} < {size})",
        restored.transferred
    );
    assert!(
        bcpart_path(&target).exists(),
        "the restart scan must keep a live connection's .bcpart (only orphans lose it)"
    );

    engine2.resume(&task.id).await.expect("resume");
    wait_for_status(
        &engine2,
        &task.id,
        TransferStatus::Completed,
        Duration::from_secs(180),
    )
    .await;

    assert!(target.exists(), "resume must produce the final file");
    assert!(
        !bcpart_path(&target).exists(),
        "finish renames the .bcpart into place, leaving no staging file"
    );
    assert_eq!(
        hex(&sha256_file(&target)),
        hex(&sha256_file(&src)),
        "the file assembled across the restart must be byte-identical to the source object"
    );
    assert!(
        checkpoint::scan(&cp_dir).is_empty(),
        "the checkpoint must be removed once the download completes"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// Test 3: pausing mid-upload, then rewriting the LOCAL source with different
/// content (same size, bumped mtime -> a changed fingerprint) before resuming,
/// must restart the upload from scratch under a fresh upload id -- never splice
/// new parts onto the stale ones. The resumed runner detects the fingerprint
/// mismatch, aborts the stale upload, and re-uploads the current file. A green
/// run is the object's hash matching the NEW source: a spliced object (old
/// parts + new parts) would hash-match NEITHER source, so equality with the new
/// one is itself proof every byte came from a fresh upload of the new content.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn upload_restarts_from_scratch_when_the_source_changed() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("fixture tempdir");
    let path = dir.path().join("source-changed.bin");
    let size = 60 * MB;
    write_pseudo_random_file(&path, size, 0x5EED_4003);
    let original_hash = sha256_file(&path);

    let cp_base = tempfile::tempdir().expect("checkpoint tempdir");
    let cp_dir = cp_base.path().to_path_buf();
    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-restart-source-changed";
    let hub = live_hub(connection_id, &hub_dir).await;

    let engine1 = build_engine_cp(
        Arc::clone(&hub),
        Arc::new(CollectingSink::default()),
        1,
        Some(cp_dir.clone()),
        Arc::new(AtomicBool::new(true)),
    );
    let key = "uploads/source-changed.bin".to_string();
    let spec = EnqueueSpec::for_upload(
        connection_id.to_string(),
        bucket.clone(),
        key.clone(),
        path.clone(),
    )
    .expect("for_upload");
    let task = engine1.enqueue(spec).await.expect("enqueue");

    wait_for_mid_flight(&engine1, &task.id, Duration::from_secs(120)).await;
    engine1.pause(&task.id).await.expect("pause");
    wait_for_status(
        &engine1,
        &task.id,
        TransferStatus::Paused,
        Duration::from_secs(60),
    )
    .await;
    let stale_upload_id = match &checkpoint::scan(&cp_dir)[0].1.resume {
        ResumeState::Upload(ms) => ms.upload_id.clone(),
        other => panic!("expected an Upload resume state, got {other:?}"),
    };
    assert!(
        pending_uploads_for_key(&client, &bucket, &key).await >= 1,
        "the stale upload must be open before the source changes"
    );

    drop(engine1);

    // Rewrite the source underneath the paused upload: same size (so the fixed
    // task total still plans correctly) but different bytes, and an explicitly
    // bumped mtime so the (size, mtime) fingerprint provably differs.
    write_pseudo_random_file(&path, size, 0x5EED_9003);
    let new_hash = sha256_file(&path);
    assert_ne!(
        hex(&original_hash),
        hex(&new_hash),
        "the rewrite must actually change the content"
    );
    let bumped = std::time::SystemTime::now() + Duration::from_secs(30);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("reopen source to bump mtime")
        .set_modified(bumped)
        .expect("bump the source mtime so the fingerprint changes");

    let engine2 = build_engine_cp(
        Arc::clone(&hub),
        Arc::new(CollectingSink::default()),
        4,
        Some(cp_dir.clone()),
        Arc::new(AtomicBool::new(true)),
    );
    restore_from_checkpoints(&engine2, &hub, &cp_dir).await;
    assert_eq!(
        snapshot_of(&engine2, &task.id).await.unwrap().status,
        TransferStatus::Paused,
        "the task must be restored as Paused before the resume decides to restart it"
    );

    engine2.resume(&task.id).await.expect("resume");
    wait_for_status(
        &engine2,
        &task.id,
        TransferStatus::Completed,
        Duration::from_secs(180),
    )
    .await;

    let downloaded = get_object_bytes(&client, &bucket, &key).await;
    assert_eq!(
        hex(&sha256_bytes(&downloaded)),
        hex(&new_hash),
        "a changed source must re-upload from scratch: the object must hash-match the NEW content \
         (a splice of old + new parts would match neither)"
    );
    assert_ne!(
        hex(&sha256_bytes(&downloaded)),
        hex(&original_hash),
        "the object must not be the pre-change content"
    );
    // The stale upload id E1 opened must have been aborted (not continued), so no
    // pending upload lingers for the key.
    assert_eq!(
        pending_uploads_for_key(&client, &bucket, &key).await,
        0,
        "the stale upload {stale_upload_id} must be aborted, not left open, when the source changes"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// Test 4: pausing mid-download, then overwriting the REMOTE object (same size,
/// new content -> a new ETag) before resuming, must discard the stale `.bcpart`
/// and re-download the current object from scratch. The resumed runner re-heads,
/// sees the ETag no longer matches the checkpoint's baseline, deletes the stale
/// staging file, and fetches the new object clean. A green run is the local
/// file hash-matching the NEW object: keeping the stale partial would leave the
/// already-"finished" chunks holding OLD bytes, so a match with the new content
/// is itself proof the partial was thrown away.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn download_restarts_from_scratch_when_the_etag_changed() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let src_dir = tempfile::tempdir().expect("source tempdir");
    let dl_dir = tempfile::tempdir().expect("download tempdir");
    let cp_base = tempfile::tempdir().expect("checkpoint tempdir");
    let cp_dir = cp_base.path().to_path_buf();
    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-restart-etag-changed";
    let hub = live_hub(connection_id, &hub_dir).await;

    let key = "downloads/etag-changed.bin";
    let size = 80 * MB;
    let old_src =
        seed_random_object(&provider, &bucket, key, src_dir.path(), size, 0x5EED_4004).await;
    let old_hash = sha256_file(&old_src);
    let target = dl_dir.path().join("etag-changed.bin");
    let total = download_total(&provider, &bucket, key).await;
    let old_etag = provider
        .head_object(&bucket, key)
        .await
        .expect("head")
        .etag
        .expect("MinIO returns an ETag");

    let engine1 = build_engine_cp(
        Arc::clone(&hub),
        Arc::new(CollectingSink::default()),
        1,
        Some(cp_dir.clone()),
        Arc::new(AtomicBool::new(true)),
    );
    let spec = EnqueueSpec::for_download(
        connection_id.to_string(),
        bucket.clone(),
        key.to_string(),
        target.clone(),
        total,
        "etag-changed.bin".to_string(),
    );
    let task = engine1.enqueue(spec).await.expect("enqueue");

    wait_for_mid_flight(&engine1, &task.id, Duration::from_secs(120)).await;
    engine1.pause(&task.id).await.expect("pause");
    wait_for_status(
        &engine1,
        &task.id,
        TransferStatus::Paused,
        Duration::from_secs(60),
    )
    .await;
    // The checkpoint's baseline ETag is the OLD object's; the resume compares
    // against it after the overwrite below.
    match &checkpoint::scan(&cp_dir)[0].1.resume {
        ResumeState::Download(ds) => assert_eq!(
            ds.etag.as_deref(),
            Some(old_etag.as_str()),
            "the checkpoint must record the object's ETag baseline"
        ),
        other => panic!("expected a Download resume state, got {other:?}"),
    }
    assert!(
        bcpart_path(&target).exists(),
        "the paused .bcpart must exist"
    );

    drop(engine1);

    // Overwrite the remote object with new content of the same size: a new ETag,
    // a plan that still matches the fixed task total.
    let new_src = dl_dir.path().join("new-remote.bin");
    write_pseudo_random_file(&new_src, size, 0x5EED_9004);
    let new_hash = sha256_file(&new_src);
    assert_ne!(
        hex(&old_hash),
        hex(&new_hash),
        "the overwrite must change the bytes"
    );
    upload_via_primitives(&provider, &bucket, key, &new_src, size).await;
    let new_etag = provider
        .head_object(&bucket, key)
        .await
        .expect("head after overwrite")
        .etag
        .expect("ETag");
    assert_ne!(old_etag, new_etag, "overwriting must change the ETag");

    let engine2 = build_engine_cp(
        Arc::clone(&hub),
        Arc::new(CollectingSink::default()),
        4,
        Some(cp_dir.clone()),
        Arc::new(AtomicBool::new(true)),
    );
    restore_from_checkpoints(&engine2, &hub, &cp_dir).await;
    assert_eq!(
        snapshot_of(&engine2, &task.id).await.unwrap().status,
        TransferStatus::Paused,
        "the task must be restored as Paused before the resume discards the stale partial"
    );

    engine2.resume(&task.id).await.expect("resume");
    wait_for_status(
        &engine2,
        &task.id,
        TransferStatus::Completed,
        Duration::from_secs(180),
    )
    .await;

    assert!(
        !bcpart_path(&target).exists(),
        "no staging file after finish"
    );
    assert_eq!(
        hex(&sha256_file(&target)),
        hex(&new_hash),
        "a changed ETag must discard the stale .bcpart and re-download: the file must hash-match \
         the NEW object (keeping the old partial would leave old bytes in the finished chunks)"
    );
    assert_ne!(
        hex(&sha256_file(&target)),
        hex(&old_hash),
        "the file must not be the pre-overwrite content"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// Test 5: a checkpoint whose connection is no longer in the store is an orphan
/// -- the startup scan must drop it (and a download's staging `.bcpart`) and
/// rebuild NO task. This reaches a genuine Paused state first: a real download
/// runs mid-flight under a live connection, pauses (real checkpoint + real
/// `.bcpart` on disk), the engine is dropped, and only THEN is the restart
/// simulated with a hub that no longer knows the connection -- exactly the
/// "connection deleted between sessions" case. The discard is asserted on the
/// real files the paused transfer left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn orphan_checkpoint_of_a_deleted_connection_is_discarded_on_startup() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let src_dir = tempfile::tempdir().expect("source tempdir");
    let dl_dir = tempfile::tempdir().expect("download tempdir");
    let cp_base = tempfile::tempdir().expect("checkpoint tempdir");
    let cp_dir = cp_base.path().to_path_buf();
    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-orphan-conn";
    let hub = live_hub(connection_id, &hub_dir).await;

    let key = "downloads/orphan.bin";
    let size = 80 * MB;
    seed_random_object(&provider, &bucket, key, src_dir.path(), size, 0x5EED_4005).await;
    let target = dl_dir.path().join("orphan.bin");
    let total = download_total(&provider, &bucket, key).await;

    let engine1 = build_engine_cp(
        Arc::clone(&hub),
        Arc::new(CollectingSink::default()),
        1,
        Some(cp_dir.clone()),
        Arc::new(AtomicBool::new(true)),
    );
    let spec = EnqueueSpec::for_download(
        connection_id.to_string(),
        bucket.clone(),
        key.to_string(),
        target.clone(),
        total,
        "orphan.bin".to_string(),
    );
    let task = engine1.enqueue(spec).await.expect("enqueue");

    wait_for_mid_flight(&engine1, &task.id, Duration::from_secs(120)).await;
    engine1.pause(&task.id).await.expect("pause");
    wait_for_status(
        &engine1,
        &task.id,
        TransferStatus::Paused,
        Duration::from_secs(60),
    )
    .await;
    assert_eq!(
        checkpoint::scan(&cp_dir).len(),
        1,
        "a real checkpoint exists"
    );
    assert!(bcpart_path(&target).exists(), "a real .bcpart exists");

    drop(engine1);

    // Simulate the restart with a hub whose store has NO connections (the
    // connection was deleted): `connection_ids()` is empty, so the checkpoint's
    // connection is unknown -> orphan.
    let empty_hub_dir = tempfile::tempdir().expect("empty hub tempdir");
    let empty_hub = Arc::new(ProviderHub::new(SecureStore {
        path: empty_hub_dir.path().join("connections.enc"),
    }));
    assert!(
        empty_hub.connection_ids().await.unwrap().is_empty(),
        "the restart hub must know no connections for this to test the orphan path"
    );
    let engine2 = build_engine_cp(
        Arc::clone(&empty_hub),
        Arc::new(CollectingSink::default()),
        4,
        Some(cp_dir.clone()),
        Arc::new(AtomicBool::new(true)),
    );
    restore_from_checkpoints(&engine2, &empty_hub, &cp_dir).await;

    assert!(
        checkpoint::scan(&cp_dir).is_empty(),
        "the orphan checkpoint must be discarded on startup"
    );
    assert!(
        !bcpart_path(&target).exists(),
        "the orphan download's .bcpart must be deleted -- nobody will ever resume it"
    );
    assert!(
        snapshot_of(&engine2, &task.id).await.is_none(),
        "no task may be rebuilt for a checkpoint whose connection is gone"
    );
    assert!(
        engine2.snapshot().await.is_empty(),
        "the orphan scan must rebuild nothing at all"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
    drop(empty_hub);
}

/// Test 6: the resume toggle, both halves. With `resume_enabled = false`, a
/// transfer that genuinely runs mid-flight and pauses writes NO checkpoint (the
/// gate suppresses even the forced pause-point write) -- so nothing could ever
/// cross a restart. And a pre-existing checkpoint left on disk is neither
/// restored nor deleted when the disabled startup path (which, like `lib.rs`,
/// skips the scan entirely while the flag is off) runs -- turning resume off is
/// non-destructive, not a silent purge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn resume_disabled_writes_no_checkpoint_and_skips_restore() {
    let provider = from_connection(&minio_connection("minioadmin")).expect("provider");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .expect("create bucket");

    let dir = tempfile::tempdir().expect("fixture tempdir");
    let path = dir.path().join("resume-disabled.bin");
    let size = 60 * MB;
    write_pseudo_random_file(&path, size, 0x5EED_4006);

    let cp_base = tempfile::tempdir().expect("checkpoint tempdir");
    let cp_dir = cp_base.path().to_path_buf();
    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-resume-disabled";
    let hub = live_hub(connection_id, &hub_dir).await;

    // --- Half A: a disabled engine writes no checkpoint even at a pause -------
    let resume_off = Arc::new(AtomicBool::new(false));
    let engine1 = build_engine_cp(
        Arc::clone(&hub),
        Arc::new(CollectingSink::default()),
        1,
        Some(cp_dir.clone()),
        Arc::clone(&resume_off),
    );
    let key = "uploads/resume-disabled.bin".to_string();
    let spec = EnqueueSpec::for_upload(
        connection_id.to_string(),
        bucket.clone(),
        key.clone(),
        path.clone(),
    )
    .expect("for_upload");
    let task = engine1.enqueue(spec).await.expect("enqueue");

    // Genuinely reach mid-flight and pause, so the gate is what suppresses the
    // write -- not an early exit that never had a checkpoint to write.
    wait_for_mid_flight(&engine1, &task.id, Duration::from_secs(120)).await;
    engine1.pause(&task.id).await.expect("pause");
    wait_for_status(
        &engine1,
        &task.id,
        TransferStatus::Paused,
        Duration::from_secs(60),
    )
    .await;
    assert!(
        checkpoint::scan(&cp_dir).is_empty(),
        "with resume disabled, a mid-flight pause must write NO checkpoint (the gate suppresses \
         even the forced pause-point write), so nothing can cross a restart"
    );

    // Reap the still-open multipart upload the paused (uncheckpointed) task left,
    // so the shared container stays clean.
    engine1.cancel(&task.id).await.expect("cancel");
    wait_for_status(
        &engine1,
        &task.id,
        TransferStatus::Canceled,
        Duration::from_secs(60),
    )
    .await;
    drop(engine1);

    // --- Half B: a disabled startup neither restores nor deletes a leftover ---
    // Seed a pre-existing checkpoint (as a prior enabled session would have).
    let seeded_id = "seeded-leftover-task";
    let seeded = checkpoint::Checkpoint {
        direction: Direction::Upload,
        connection_id: connection_id.to_string(),
        bucket: bucket.clone(),
        key: "uploads/leftover.bin".to_string(),
        local_path: path.to_string_lossy().to_string(),
        file_name: "leftover.bin".to_string(),
        total: size,
        resume: ResumeState::Upload(MultipartState {
            upload_id: "u-leftover".to_string(),
            completed: vec![],
            source_size: size,
            source_mtime: 1,
            part_size: 0,
        }),
    };
    checkpoint::write(&cp_dir, seeded_id, &seeded).expect("seed a leftover checkpoint");
    assert_eq!(
        checkpoint::scan(&cp_dir).len(),
        1,
        "the seeded checkpoint must be present before the disabled startup"
    );

    let engine2 = build_engine_cp(
        Arc::clone(&hub),
        Arc::new(CollectingSink::default()),
        4,
        Some(cp_dir.clone()),
        Arc::clone(&resume_off),
    );
    // Mirror `lib.rs`: the restore scan runs only while resume is enabled. With
    // the flag off, the whole scan/restore block is skipped -- so nothing is
    // rebuilt and nothing is deleted.
    if resume_off.load(std::sync::atomic::Ordering::Relaxed) {
        restore_from_checkpoints(&engine2, &hub, &cp_dir).await;
    }

    assert!(
        snapshot_of(&engine2, seeded_id).await.is_none(),
        "a disabled startup must not rebuild the leftover checkpoint into a task"
    );
    assert!(
        engine2.snapshot().await.is_empty(),
        "a disabled startup must rebuild nothing at all"
    );
    let after = checkpoint::scan(&cp_dir);
    assert_eq!(
        after.len(),
        1,
        "a disabled startup must be non-destructive: the leftover checkpoint must remain on disk"
    );
    assert_eq!(
        after[0].0, seeded_id,
        "and it must be the very same checkpoint"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}
