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
//! ## A known error-family mismatch
//!
//! [`wrong_secret_surfaces_expected_error_family`] documents (rather than
//! papers over) a real finding from running this suite against actual
//! MinIO: a wrong secret key does **not** land in `AppError`'s `auth/*`
//! family. It surfaces as `internal` (message `"unhandled error
//! (SignatureDoesNotMatch)"`) instead, because `aws-sdk-s3`'s generated
//! `ListBuckets` error enum doesn't model `SignatureDoesNotMatch` as one of
//! its own variants, so it collapses to `aws_sdk_s3::Error::Unhandled`
//! before `crate::error`'s keyword sniff -- which doesn't recognize that
//! phrase either -- ever gets a chance at an `auth/*` classification. See
//! that test's doc comment for the full chain and why this is a real,
//! reportable UX gap rather than a test-authoring artifact.

use bucketcat_lib::provider::{from_connection, Provider};
use bucketcat_lib::store::{Connection, SecureStore};

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
/// **Finding from running this against live MinIO (RELEASE.2025-09-07,
/// `minio/minio:latest` at time of writing):** a wrong `SecretAccessKey`
/// does **not** surface as `AppError`'s `auth/*` family, which is what the
/// original task brief expected. The actual code observed is
/// `internal`, with `params.message` equal to
/// `"unhandled error (SignatureDoesNotMatch)"`.
///
/// Why: MinIO returns a 403 whose S3 error code is `SignatureDoesNotMatch`.
/// `aws-sdk-s3`'s generated per-operation error enum for `ListBuckets`
/// doesn't model that error code as one of its own variants (unlike
/// `NoSuchBucket`/`BucketAlreadyExists`/`AccessDenied`, which *are*
/// modeled and explicitly matched in `crate::error`'s
/// `From<aws_sdk_s3::Error>` impl), so the SDK collapses it into the
/// catch-all `aws_sdk_s3::Error::Unhandled` variant even though a real,
/// parseable response came back. That lands in `crate::error`'s wildcard
/// `_` arm, whose keyword sniff over the rendered message (`"timed
/// out"`/`"timeout"` => `Timeout`, `"dispatch failure"`/`"dns"`/
/// `"connect"`/`"unreachable"`/`"tls"` => `Unreachable`) finds no match in
/// `"unhandled error (SignatureDoesNotMatch)"`, so it falls through to
/// `AppError::Internal`.
///
/// This is a real, reportable gap, not a test-authoring quirk: a MinIO
/// user who mistypes their secret key currently gets an `internal` /
/// "something went wrong" message instead of the `auth/*` family that
/// condition actually deserves. Fixing it -- e.g. adding a
/// `SignatureDoesNotMatch`/`InvalidAccessKeyId` keyword to the sniff, or a
/// dedicated match arm keyed on the S3 error code rather than the message
/// text -- is follow-up work outside this task's scope; this test's job is
/// to assert the code that *actually* surfaces today (per the task
/// instruction to record a real mismatch rather than paper over it) so a
/// future fix shows up here as an intentional, visible test change instead
/// of a silent behavior drift.
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
        code, "internal",
        "expected the `internal` code this MinIO build's unmodeled \
         `SignatureDoesNotMatch` response actually maps to today (see this test's doc \
         comment for why, and the known auth/* UX gap this represents) -- got `{code}` \
         instead, which means the error mapping chain (or MinIO's behavior) changed and \
         this assertion should be revisited"
    );
    assert!(
        err.params()
            .get("message")
            .is_some_and(|m| m.contains("SignatureDoesNotMatch")),
        "expected the internal error's message to mention SignatureDoesNotMatch, got: {:?}",
        err.params()
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
