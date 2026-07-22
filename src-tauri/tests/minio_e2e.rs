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

use bucketcat_lib::provider::{from_connection, Provider};
use bucketcat_lib::store::{Connection, SecureStore};

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
