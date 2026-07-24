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

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use sha2::{Digest, Sha256};

use bucketcat_lib::provider::{from_connection, Provider, ProviderHub, S3Provider, UploadedPart};
use bucketcat_lib::store::{Connection, SecureStore};
use bucketcat_lib::transfer::part::MULTIPART_THRESHOLD;
use bucketcat_lib::transfer::{
    plan_upload, spawn_aggregator, EngineConfig, EnqueueSpec, ProgressPayload, ProgressSink,
    TransferEngine, TransferSink, TransferStatus, TransferTaskDto, UploadPlan, UploadRunner,
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
/// following `plan_upload` exactly as `UploadRunner` does: a single
/// `PutObject` below the threshold, otherwise create + sequential parts +
/// complete. Parts are sent in plan order here (out-of-order is its own test).
async fn upload_via_primitives(
    provider: &S3Provider,
    bucket: &str,
    key: &str,
    path: &Path,
    total: u64,
) {
    match plan_upload(total) {
        UploadPlan::Single { length } => {
            provider
                .put_object_from_file(bucket, key, path, length)
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
                        bucket, key, &upload_id, p.number, path, p.offset, p.length,
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

/// A ~1MB file goes up as a single `PutObject` (below the 16MB threshold) and
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
        .put_object_from_file(&bucket, key, &path, size)
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
/// `plan_upload`, then completed, then read back with a SHA-256 equality
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
        matches!(plan_upload(size), UploadPlan::Multipart { .. }),
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
    let size = 24 * MB; // three 8MB parts at the floor part size.
    write_pseudo_random_file(&path, size, 0x5EED_0003);

    let parts = match plan_upload(size) {
        UploadPlan::Multipart { parts, .. } => parts,
        UploadPlan::Single { .. } => panic!("24MB must plan as multipart"),
    };
    assert_eq!(
        parts.len(),
        3,
        "24MB at the 8MB floor is exactly three parts"
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
                &bucket, key, &upload_id, p.number, &path, p.offset, p.length,
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
    let size = 16 * MB; // two 8MB parts.
    write_pseudo_random_file(&path, size, 0x5EED_0004);
    let parts = match plan_upload(size) {
        UploadPlan::Multipart { parts, .. } => parts,
        UploadPlan::Single { .. } => panic!("16MB must plan as multipart"),
    };

    let key = "uploads/shrinking.bin";
    let upload_id = provider.multipart_init(&bucket, key).await.expect("init");

    // Truncate AFTER planning: part 1 (offset 0, length 8MB) no longer fits.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("reopen fixture for truncation");
    file.set_len(4 * MB).expect("truncate the fixture");
    drop(file);

    let p = parts[0];
    let err = provider
        .upload_part_from_file(
            &bucket, key, &upload_id, p.number, &path, p.offset, p.length,
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
        .upload_part_from_file(&bucket, key, &upload_id, 1, &path, 0, size)
        .await
        .expect("first part should upload");
    provider
        .multipart_abort(&bucket, key, &upload_id)
        .await
        .expect("abort should succeed");

    // The provider path rejects a further part against the aborted id...
    provider
        .upload_part_from_file(&bucket, key, &upload_id, 2, &path, 0, size)
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

/// A 0-byte file lands as a real 0-byte object. The single-stream path
/// (`plan_upload(0) == Single { length: 0 }`) must handle an empty body --
/// this is folder-marker-adjacent and easy to get wrong.
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
    assert_eq!(plan_upload(0), UploadPlan::Single { length: 0 });

    let key = "uploads/empty.bin";
    provider
        .put_object_from_file(&bucket, key, &path, 0)
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
/// hash check: exactly `MULTIPART_THRESHOLD` (multipart) and
/// `MULTIPART_THRESHOLD - 1` (single stream). That boundary selects between
/// two entirely different code paths, so both sides are proven.
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

    // Exactly at the threshold -> multipart.
    let at_path = dir.path().join("at-threshold.bin");
    write_pseudo_random_file(&at_path, MULTIPART_THRESHOLD, 0x5EED_0801);
    assert!(
        matches!(
            plan_upload(MULTIPART_THRESHOLD),
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
        MULTIPART_THRESHOLD,
    )
    .await;

    // One byte below -> single stream.
    let below_path = dir.path().join("below-threshold.bin");
    write_pseudo_random_file(&below_path, MULTIPART_THRESHOLD - 1, 0x5EED_0802);
    assert!(
        matches!(
            plan_upload(MULTIPART_THRESHOLD - 1),
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
        MULTIPART_THRESHOLD - 1,
    )
    .await;

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

/// Builds a real `TransferEngine` with a real `UploadRunner` and a real
/// aggregator. `max_parts` is a knob: the pause/cancel tests set it to 1 to
/// serialize parts, which widens the mid-flight window and makes the timing
/// deterministic without changing any of the abort/resume semantics under test.
fn build_engine(
    hub: Arc<ProviderHub>,
    sink: Arc<dyn TransferSink>,
    max_parts: usize,
) -> TransferEngine {
    let progress_tx = spawn_aggregator(Arc::new(NullProgressSink));
    TransferEngine::new(
        hub,
        Arc::new(UploadRunner),
        sink,
        progress_tx,
        EngineConfig {
            max_tasks: 3,
            max_parts,
        },
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
    // The default part concurrency (EngineConfig::default().max_parts == 4).
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
