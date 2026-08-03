//! RustFS end-to-end integration tests (opt-in).
//!
//! These drive the real [`bucketcat_lib::provider::S3Provider`] and the real
//! [`bucketcat_lib::transfer::TransferEngine`] against a live
//! [RustFS](https://github.com/rustfs/rustfs) container. Every test is
//! `#[ignore]`d, so a plain `cargo test` never touches Docker or the network.
//!
//! ## Why this suite exists when RustFS needs no provider code
//!
//! RustFS lands on **every** default in `provider::s3`: `uses_path_style`
//! returns true for it (it is neither a real-AWS endpoint nor `"oss"`),
//! `s3_compat_endpoint` passes its endpoint through untouched, it gets no
//! `RegionRouting`, and `supports_batch_delete` leaves it on the fast path.
//! Not one line of `provider::s3` mentions `"rustfs"`. That is the *intended*
//! outcome -- RustFS documents wanting exactly what those defaults produce --
//! but it also means nothing in the crate would notice if a default moved out
//! from under it. The four `rustfs_*` assertions in `provider::s3`'s
//! `mod tests` pin the defaults; this file is what proves the defaults are the
//! *right* ones against a real server.
//!
//! ## Running these tests
//!
//! 1. Start Docker (if its daemon isn't already running) and launch RustFS:
//!
//!    ```bash
//!    open -a Docker   # macOS, only if the daemon isn't already up
//!    mkdir -p /tmp/rustfs-e2e/data /tmp/rustfs-e2e/logs
//!    chmod -R 777 /tmp/rustfs-e2e
//!    docker run -d --name bucketcat-rustfs \
//!      -p 9010:9000 -p 9011:9001 \
//!      -v /tmp/rustfs-e2e/data:/data -v /tmp/rustfs-e2e/logs:/logs \
//!      rustfs/rustfs:latest
//!    ```
//!
//!    The image runs as the non-root user `10001:10001`, so a bind-mounted
//!    host directory it cannot write to makes the container exit on boot --
//!    hence the `chmod`.
//!
//!    Host ports 9010/9011 rather than RustFS's native 9000/9001 are
//!    deliberate: MinIO uses the same two, and `tests/minio_e2e.rs` defaults to
//!    `http://localhost:9000`. Publishing elsewhere lets both containers stay
//!    up, so the two suites never have to be run one-at-a-time.
//!
//! 2. Run the suite:
//!
//!    ```bash
//!    cargo test --test rustfs_e2e -- --ignored
//!    ```
//!
//! 3. Tear the container down when finished (the image can stay cached):
//!
//!    ```bash
//!    docker rm -f bucketcat-rustfs
//!    ```
//!
//! `BUCKETCAT_RUSTFS_ENDPOINT` overrides the endpoint (defaults to
//! `http://localhost:9010`); `BUCKETCAT_RUSTFS_ACCESS_KEY` /
//! `BUCKETCAT_RUSTFS_SECRET_KEY` override the credentials, which default to
//! RustFS's own documented first-boot pair (`rustfsadmin`/`rustfsadmin`).
//! Nothing here is a real secret.
//!
//! ## A checksum bug this app happens to be immune to
//!
//! RustFS at or below `1.0.0-beta.8` fails multipart uploads from any client
//! that sends AWS's default flexible checksums, with
//! `500 InternalError: Io error: checksum mismatch: CRC64NVME`
//! ([rustfs#3506](https://github.com/rustfs/rustfs/issues/3506), fixed by
//! [rustfs#3508](https://github.com/rustfs/rustfs/pull/3508) in beta.9). That
//! hits aws-cli >=2.23, boto3 >=1.36 and aws-sdk-go-v2 out of the box.
//!
//! BucketCat never sees it. `provider::s3::build_client` forces
//! `RequestChecksumCalculation::WhenRequired` on every non-AWS endpoint -- a
//! decision made for R2/older-MinIO/Dell-ECS `NotImplemented` errors and for
//! OSS rejecting `aws-chunked` outright, long before RustFS existed. That
//! setting is precisely the workaround the RustFS issue recommends, so this
//! suite passes even against a pre-beta.9 container. Worth knowing before
//! anyone "simplifies" that branch back to the SDK default.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use aws_sdk_s3::primitives::ByteStream;
use sha2::{Digest, Sha256};

use bucketcat_lib::provider::{from_connection, Provider, ProviderHub, S3Provider, UploadedPart};
use bucketcat_lib::store::{Connection, SecureStore};
use bucketcat_lib::transfer::{
    checkpoint, plan_upload_with, restore_all, spawn_aggregator, DispatchRunner, DownloadRunner,
    EngineConfig, EnqueueSpec, ProgressPayload, ProgressSink, ResumeState, TransferEngine,
    TransferSink, TransferStatus, TransferTaskDto, TransferTuning, UploadPlan, UploadRunner,
};

/// 1 MiB, the unit these fixtures are sized in.
const MB: u64 = 1024 * 1024;

// --- connection configuration ----------------------------------------------

/// The endpoint these tests target. Defaults to the host port the module docs'
/// `docker run` publishes, not RustFS's native 9000, so a MinIO container can
/// stay up alongside.
fn endpoint() -> String {
    std::env::var("BUCKETCAT_RUSTFS_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:9010".to_string())
}

/// RustFS's documented first-boot credentials. Its own docs call these
/// "for first-boot convenience only" and tell you to replace them before
/// exposing the server -- fine for a throwaway loopback container, and the
/// reason these are hardcoded rather than treated as secrets.
fn access_key() -> String {
    std::env::var("BUCKETCAT_RUSTFS_ACCESS_KEY").unwrap_or_else(|_| "rustfsadmin".to_string())
}

fn secret_key() -> String {
    std::env::var("BUCKETCAT_RUSTFS_SECRET_KEY").unwrap_or_else(|_| "rustfsadmin".to_string())
}

/// A `Connection` pointed at the live RustFS container, with the given secret
/// (the documented default, or deliberately wrong, depending on the test).
fn rustfs_connection(secret: &str) -> Connection {
    Connection {
        id: "e2e-rustfs".to_string(),
        provider: "rustfs".to_string(),
        name: "e2e rustfs".to_string(),
        endpoint: endpoint(),
        region: "us-east-1".to_string(),
        access_key_id: access_key(),
        secret_access_key: secret.to_string(),
        default_bucket: None,
        api_token: None,
    }
}

/// A bucket name unique to this run, so repeated runs -- and a run overlapping
/// a leftover bucket from a previous crash -- never collide. S3 bucket names
/// must be lowercase; a UUID's hex-and-hyphen form already is.
fn unique_bucket_name() -> String {
    format!("bucketcat-e2e-{}", uuid::Uuid::new_v4())
}

// --- raw-SDK helpers (test seeding and readback only) -----------------------

/// A raw SDK client for seeding and verification, mirroring what
/// `from_connection` builds for RustFS: path-style, static credentials, and
/// the same `WhenRequired` checksum posture `build_client` applies to every
/// non-AWS endpoint (see the module docs -- the SDK default would break
/// multipart against a pre-beta.9 server).
fn raw_seed_client() -> aws_sdk_s3::Client {
    let credentials =
        aws_sdk_s3::config::Credentials::new(access_key(), secret_key(), None, None, "e2e-seed");
    let config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .endpoint_url(endpoint())
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .credentials_provider(credentials)
        .force_path_style(true)
        .request_checksum_calculation(aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(aws_sdk_s3::config::ResponseChecksumValidation::WhenRequired)
        .build();
    aws_sdk_s3::Client::from_conf(config)
}

/// Seeds one small text object.
async fn put_text(client: &aws_sdk_s3::Client, bucket: &str, key: &str) {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from_static(b"bucketcat-e2e"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("seeding put_object {key} should succeed: {e}"));
}

/// Reads a whole object back as raw bytes (test-sized only).
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
/// not exist.
async fn head_object_size(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Option<u64> {
    match client.head_object().bucket(bucket).key(key).send().await {
        Ok(out) => Some(out.content_length().unwrap_or(0).max(0) as u64),
        Err(_) => None,
    }
}

/// How many *pending* (never completed/aborted) multipart uploads the server
/// still holds for `key`.
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

/// Best-effort teardown: removes every object then the bucket, so repeated
/// runs never collide and the container stays clean.
async fn drain_and_delete_bucket(client: &aws_sdk_s3::Client, provider: &S3Provider, bucket: &str) {
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

/// Teardown that first reaps lingering multipart uploads -- an in-progress
/// upload can otherwise block `delete_bucket` and leak fragments -- then
/// drains and deletes the bucket.
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

/// Creates a bucket and returns it alongside a provider + raw client, the
/// three things almost every test below opens with.
async fn fresh_bucket() -> (S3Provider, aws_sdk_s3::Client, String) {
    let provider =
        from_connection(&rustfs_connection(&secret_key())).expect("a valid RustFS profile");
    let client = raw_seed_client();
    let bucket = unique_bucket_name();
    provider
        .create_bucket(&bucket)
        .await
        .unwrap_or_else(|e| panic!("create_bucket {bucket} should succeed: {e}"));
    (provider, client, bucket)
}

/// Collects every page of a listing under `prefix`, asserting the pager
/// terminates and no page exceeds `max_keys`.
async fn list_all_pages(
    provider: &S3Provider,
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

// --- fixtures & hashing -----------------------------------------------------

/// Writes `size` bytes of deterministic LCG pseudo-random content to `path`.
///
/// Seeded (so the source hash is reproducible) and, crucially,
/// non-compressible -- a file of repeated bytes would let a per-part offset bug
/// go unnoticed because every offset reads the same value. Knuth's MMIX LCG
/// constants give that without a `rand` dev-dependency.
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

/// Fetches a URL with no credentials at all, via `curl`. Used to prove a
/// presigned URL carries its own authorization.
fn fetch_anonymously(url: &str, out: &Path) {
    let status = std::process::Command::new("curl")
        .args(["-sSfL", "-o"])
        .arg(out)
        .arg(url)
        .status()
        .expect("curl should be available to fetch a presigned URL");
    assert!(
        status.success(),
        "an unauthenticated GET of the presigned URL must succeed, curl exited {status}"
    );
}

// ===========================================================================
// Group 1: connection and error families
// ===========================================================================

#[tokio::test]
#[ignore]
async fn from_connection_and_test_connection_succeed_against_live_rustfs() {
    let provider =
        from_connection(&rustfs_connection(&secret_key())).expect("a valid RustFS profile");
    provider
        .test_connection()
        .await
        .expect("test_connection should succeed against a live, correctly-credentialed RustFS");
}

/// A wrong secret must land in the `auth/*` family, not `internal`.
///
/// This is the same trap MinIO sprang: `aws-sdk-s3`'s generated `ListBuckets`
/// error enum does not model `SignatureDoesNotMatch`, so the SDK collapses it
/// into `Unhandled` and the keyword sniff in `crate::error` does not recognize
/// the phrase either -- landing on `AppError::Internal`. `normalize_s3_error`'s
/// third tier recovers it by reading the raw S3 error code off the pre-erasure
/// operation error. Asserting it here proves RustFS returns the same code on
/// the wire that the recovery tier keys on.
#[tokio::test]
#[ignore]
async fn wrong_secret_surfaces_expected_error_family() {
    let provider = from_connection(&rustfs_connection("definitely-the-wrong-secret"))
        .expect("a valid RustFS profile");

    let err = provider
        .test_connection()
        .await
        .expect_err("test_connection with a wrong secret key must not succeed");

    let code = err.code();
    assert_eq!(
        code, "auth/invalid-credentials",
        "expected a wrong secret against live RustFS to surface `auth/invalid-credentials` \
         (recovered from the raw S3 error code) -- got `{code}`, which means RustFS's wire \
         response or the SDK's error modeling changed"
    );
}

#[tokio::test]
#[ignore]
async fn unreachable_endpoint_surfaces_network_error() {
    let mut conn = rustfs_connection(&secret_key());
    conn.endpoint = "http://localhost:19999".to_string();
    let provider = from_connection(&conn).expect("a valid connection profile should build");

    let err = provider
        .test_connection()
        .await
        .expect_err("test_connection against an unreachable endpoint must not succeed");

    let code = err.code();
    assert!(
        code.starts_with("network/"),
        "expected a network/* error code for an unreachable endpoint, got `{code}`"
    );
}

// ===========================================================================
// Group 2: bucket CRUD
//
// RustFS is a throwaway local container, so unlike the sandbox-bucket suites
// (`r2_e2e`, `b2_e2e`, `qiniu_e2e`, `oss_e2e`) this one may create and destroy
// buckets freely -- the same latitude `minio_e2e` has.
// ===========================================================================

#[tokio::test]
#[ignore]
async fn create_list_delete_bucket_round_trip() {
    let provider =
        from_connection(&rustfs_connection(&secret_key())).expect("a valid RustFS profile");
    let bucket = unique_bucket_name();

    provider
        .create_bucket(&bucket)
        .await
        .expect("create_bucket should succeed against live RustFS");

    let buckets = provider
        .list_buckets()
        .await
        .expect("list_buckets should succeed after create_bucket");
    assert!(
        buckets.iter().any(|b| b.name == bucket),
        "list_buckets should contain `{bucket}`, saw: {buckets:?}"
    );

    provider
        .delete_bucket(&bucket)
        .await
        .expect("delete_bucket should succeed against live RustFS");

    let after = provider
        .list_buckets()
        .await
        .expect("list_buckets should succeed after delete_bucket");
    assert!(
        !after.iter().any(|b| b.name == bucket),
        "list_buckets should no longer contain `{bucket}`, saw: {after:?}"
    );
}

// ===========================================================================
// Group 3: listing -- paging and folder semantics
// ===========================================================================

#[tokio::test]
#[ignore]
async fn paged_listing_spans_multiple_pages_without_duplicates() {
    let (provider, client, bucket) = fresh_bucket().await;

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

#[tokio::test]
#[ignore]
async fn delimiter_listing_surfaces_folders_and_children() {
    let (provider, client, bucket) = fresh_bucket().await;

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

// ===========================================================================
// Group 4: batch delete -- the single most important test in this file
//
// `supports_batch_delete` excludes only `"oss"` and `"rainyun"`, so RustFS
// keeps S3's Multi-Object Delete (`POST /?delete`). That is a *default*, not a
// verified decision, and the failure mode is silent: OSS and Rainyun both
// hard-require the legacy `Content-MD5` header, which `aws-sdk-s3` has no way
// to send, so batch delete against them fails outright.
//
// `b2_e2e.rs` records the discipline this follows: the pre-implementation probe
// for Rainyun was hand-rolled, looked fine, and was demolished by the first live
// run (4 of 6 tests failed). So the check below goes through the real
// `S3Provider::delete_objects`, driving the same `aws-sdk-s3` request shape
// production sends -- not a hand-built HTTP call that can differ in exactly the
// header under test.
// ===========================================================================

/// Batch delete through the real SDK path succeeds against RustFS, which is
/// what keeps `supports_batch_delete("rustfs")` on its default `true`.
///
/// The seeded object count is deliberately above one: a single-key batch could
/// be served by a server that only implements the singular `DeleteObject`, and
/// would not prove the multi-object endpoint exists at all.
#[tokio::test]
#[ignore]
async fn batch_delete_uses_the_multi_object_path() {
    let (provider, client, bucket) = fresh_bucket().await;

    let keys: Vec<String> = (0..5).map(|i| format!("batch/obj-{i}.txt")).collect();
    for k in &keys {
        put_text(&client, &bucket, k).await;
    }

    let result = provider.delete_objects(&bucket, &keys).await.expect(
        "DeleteObjects must succeed against RustFS -- if this fails with a missing-checksum \
             or Content-MD5 complaint, RustFS belongs in `supports_batch_delete`'s exclusion \
             list alongside OSS and Rainyun, and this test should assert that instead",
    );

    assert_eq!(
        result.succeeded as usize,
        keys.len(),
        "every key must be deleted"
    );
    assert!(
        result.failed.is_empty(),
        "no per-key failures expected: {:?}",
        result.failed
    );

    let (rest, _) = list_all_pages(&provider, &bucket, "", 100).await;
    assert!(
        rest.is_empty(),
        "the batch must have removed everything: {rest:?}"
    );

    drain_and_delete_bucket(&client, &provider, &bucket).await;
}

/// S3 semantics: deleting a key that does not exist is itself a success
/// (idempotent delete), so a nonexistent key does NOT produce a per-key
/// failure -- `succeeded` counts it.
#[tokio::test]
#[ignore]
async fn batch_delete_counts_missing_keys_as_success_and_removes_the_rest() {
    let (provider, client, bucket) = fresh_bucket().await;

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

// ===========================================================================
// Group 5: rename (server-side copy + delete)
// ===========================================================================

#[tokio::test]
#[ignore]
async fn rename_object_round_trip() {
    let (provider, client, bucket) = fresh_bucket().await;

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
    let (provider, client, bucket) = fresh_bucket().await;

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

// ===========================================================================
// Group 6: folder markers
// ===========================================================================

#[tokio::test]
#[ignore]
async fn created_folder_is_visible_as_prefix_and_empty_inside() {
    let (provider, client, bucket) = fresh_bucket().await;

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

// ===========================================================================
// Group 7: upload primitives -- the narrowest proof each byte range lands
// ===========================================================================

/// Uploads `path` via the provider transfer primitives, following
/// `plan_upload_with` (under the default tuning) exactly as `UploadRunner`
/// does.
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

#[tokio::test]
#[ignore]
async fn upload_small_file_round_trips() {
    let (provider, client, bucket) = fresh_bucket().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("small.bin");
    let size = MB;
    write_pseudo_random_file(&path, size, 0x5EED_0001);

    let key = "uploads/small.bin";
    provider
        .put_object_from_file(&bucket, key, &path, size)
        .await
        .expect("single-stream put_object_from_file should succeed against live RustFS");

    let (all, _) = list_all_pages(&provider, &bucket, "uploads/", 100).await;
    let entry = all
        .iter()
        .find(|e| e.key == key)
        .unwrap_or_else(|| panic!("uploaded object {key} must appear in the listing: {all:?}"));
    assert_eq!(entry.size, Some(size));

    cleanup_bucket(&client, &provider, &bucket).await;
}

/// ~40MB of seeded, non-compressible content uploaded part by part, then read
/// back with a SHA-256 equality check. This is where the module docs' checksum
/// note is actually exercised: an SDK left on the default
/// `WhenSupported` posture is what triggers RustFS <= beta.8's
/// `checksum mismatch: CRC64NVME` on exactly this path.
#[tokio::test]
#[ignore]
async fn upload_multipart_file_round_trips_with_matching_hash() {
    let (provider, client, bucket) = fresh_bucket().await;

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
/// `multipart_complete` sorts before sending. S3 answers an unsorted part list
/// with `InvalidPartOrder`; that sort has no unit-level regression protection
/// on the real send path.
#[tokio::test]
#[ignore]
async fn parts_uploaded_out_of_order_still_complete() {
    let (provider, client, bucket) = fresh_bucket().await;

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
/// part is sent: `Length::Exact` doubles as an integrity check.
#[tokio::test]
#[ignore]
async fn a_shrinking_file_fails_before_uploading_a_short_part() {
    let (provider, client, bucket) = fresh_bucket().await;

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
            &bucket, key, &upload_id, p.number, &path, p.offset, p.length,
        )
        .await
        .expect_err("uploading a part that no longer fits the shrunk file must fail");
    assert_eq!(
        err.code(),
        "local/file-io",
        "Length::Exact must catch the shrink as a local file-io error rather than sending a \
         short part"
    );

    provider
        .multipart_abort(&bucket, key, &upload_id)
        .await
        .expect("abort the orphaned upload");
    cleanup_bucket(&client, &provider, &bucket).await;
}

/// Aborting a multipart upload reaps its server-side fragments: a further
/// `upload_part` against the aborted id is rejected with `NoSuchUpload`,
/// proving the fragments are gone rather than merely orphaned.
#[tokio::test]
#[ignore]
async fn aborting_a_multipart_upload_removes_the_fragments() {
    let (provider, client, bucket) = fresh_bucket().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("abort.bin");
    let size = MB;
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

    provider
        .upload_part_from_file(&bucket, key, &upload_id, 2, &path, 0, size)
        .await
        .expect_err("uploading to an aborted upload id must fail; the fragments are gone");

    let (all, _) = list_all_pages(&provider, &bucket, "", 100).await;
    assert!(
        all.is_empty(),
        "an aborted multipart upload leaves no object: {all:?}"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
}

/// `multipart_list` (ListParts) reports exactly the parts the server has
/// accepted for an in-progress upload -- the authoritative "already done" set
/// cross-restart resume consults instead of trusting a local checkpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn multipart_list_returns_the_accepted_parts() {
    let (provider, client, bucket) = fresh_bucket().await;
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
                &bucket, key, &upload_id, p.number, &path, p.offset, p.length,
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
    cleanup_bucket(&client, &provider, &bucket).await;
}

/// A 0-byte file lands as a real 0-byte object. The single-stream path
/// (`plan_upload_with(0, ..) == Single { length: 0 }`) must handle an empty
/// body -- folder-marker-adjacent and easy to get wrong.
#[tokio::test]
#[ignore]
async fn zero_byte_file_round_trips() {
    let (provider, client, bucket) = fresh_bucket().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("empty.bin");
    std::fs::write(&path, b"").expect("write empty fixture");
    assert_eq!(
        plan_upload_with(0, &TransferTuning::default()),
        UploadPlan::Single { length: 0 }
    );

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

    cleanup_bucket(&client, &provider, &bucket).await;
}

/// The two files straddling the multipart threshold each round-trip with a
/// hash check: exactly `TransferTuning::default().upload_threshold`
/// (multipart) and one byte below it (single stream). That boundary selects
/// between two entirely different code paths, so both sides are proven.
#[tokio::test]
#[ignore]
async fn threshold_boundary_files_each_land() {
    let (provider, client, bucket) = fresh_bucket().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let threshold = TransferTuning::default().upload_threshold;

    let at_path = dir.path().join("at-threshold.bin");
    write_pseudo_random_file(&at_path, threshold, 0x5EED_0801);
    assert!(matches!(
        plan_upload_with(threshold, &TransferTuning::default()),
        UploadPlan::Multipart { .. }
    ));
    assert_round_trip_hash(
        &provider,
        &client,
        &bucket,
        "uploads/at-threshold.bin",
        &at_path,
        threshold,
    )
    .await;

    let below_path = dir.path().join("below-threshold.bin");
    write_pseudo_random_file(&below_path, threshold - 1, 0x5EED_0802);
    assert!(matches!(
        plan_upload_with(threshold - 1, &TransferTuning::default()),
        UploadPlan::Single { .. }
    ));
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

// ===========================================================================
// Group 8: ProviderHub against a live server
// ===========================================================================

/// A client obtained through `ProviderHub::provider()` performs real S3 work,
/// a second lookup is served from cache, and `mutate` genuinely invalidates
/// that cache against the live server rather than only inside the in-memory map.
#[tokio::test]
#[ignore]
async fn provider_hub_round_trip_against_live_rustfs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hub = ProviderHub::new(SecureStore {
        path: dir.path().join("connections.enc"),
    });
    let id = "e2e-hub-rustfs".to_string();

    hub.mutate(|list| {
        list.push(Connection {
            id: id.clone(),
            ..rustfs_connection(&secret_key())
        });
        Ok(())
    })
    .await
    .expect("mutate should persist the connection");

    let provider = hub
        .provider(&id)
        .await
        .expect("provider() should build a client for the just-persisted connection");
    provider
        .list_buckets()
        .await
        .expect("list_buckets through a hub-provided client should succeed");

    let provider_again = hub.provider(&id).await.expect("second lookup");
    assert!(
        Arc::ptr_eq(&provider, &provider_again),
        "the second provider() lookup must reuse the cached client, not rebuild it"
    );

    hub.mutate(|list| {
        list[0].secret_access_key = "definitely-the-wrong-secret".to_string();
        Ok(())
    })
    .await
    .expect("mutate should persist the rotated secret");

    let rotated = hub
        .provider(&id)
        .await
        .expect("provider() should rebuild a client after invalidation");
    assert!(
        !Arc::ptr_eq(&provider, &rotated),
        "invalidation must have produced a new client, not reused the pre-rotation one"
    );
    let err = rotated
        .list_buckets()
        .await
        .expect_err("list_buckets with a rotated-to-wrong secret must fail");
    assert_eq!(err.code(), "auth/invalid-credentials");
}

// ===========================================================================
// Group 9: presigned URLs
// ===========================================================================

/// A presigned GET URL works with no credentials attached: uploaded object ->
/// `presign_get` -> unauthenticated `curl` -> SHA-256 match against the source.
///
/// RustFS caps presigned expiry at 604800s (7 days) and rejects malformed or
/// out-of-range values with `AuthorizationQueryParametersError`
/// ([rustfs#5368](https://github.com/rustfs/rustfs/pull/5368), beta.12); the
/// hour used here is far inside that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn presigned_get_works() {
    let (provider, client, bucket) = fresh_bucket().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("share.bin");
    write_pseudo_random_file(&path, 2 * MB, 0x5EED_6001);
    provider
        .put_object_from_file(&bucket, "share.bin", &path, 2 * MB)
        .await
        .expect("put");

    let url = provider
        .presign_get(&bucket, "share.bin", 3600)
        .await
        .expect("presign");

    let out = dir.path().join("fetched.bin");
    fetch_anonymously(&url, &out);

    assert_eq!(
        hex(&sha256_file(&out)),
        hex(&sha256_file(&path)),
        "the bytes fetched through the presigned URL must match the source exactly"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
}

// ===========================================================================
// Group 10: RustFS-specific -- is the configured region actually enforced?
// ===========================================================================

/// RustFS does not reject a request whose SigV4 credential scope names a
/// region other than the server's configured one.
///
/// This is asserted because it was a *source-level inference*, not something
/// RustFS documents. It delegates signature verification to
/// [`s3s`](https://github.com/s3s-project/s3s), whose `ops` module treats the
/// region parsed out of the credential scope as authoritative ("it was verified
/// by the signature") and never compares it against `RUSTFS_REGION`. If that
/// ever changes, BucketCat's RustFS connections would start failing for anyone
/// whose saved region does not match the server's -- a confusing failure that
/// only this test would explain.
///
/// It also documents why RustFS needs no `RegionFlavor`: with the region
/// unenforced there is nothing to route between, so `from_connection` leaving
/// `routing: None` is correct rather than merely untested.
#[tokio::test]
#[ignore]
async fn a_mismatched_region_is_not_rejected() {
    let mut conn = rustfs_connection(&secret_key());
    conn.region = "ap-southeast-1".to_string();
    let provider = from_connection(&conn).expect("a valid RustFS profile");

    provider.list_buckets().await.expect(
        "RustFS must accept a credential scope naming a region other than its own -- if this \
         now fails, RustFS started enforcing `RUSTFS_REGION` and the wizard can no longer \
         prefill a fixed `us-east-1`",
    );
}

// ===========================================================================
// Group 11: the full engine -- TransferEngine + runners + a real ProviderHub
//
// Everything above drives `from_connection`/`Provider` directly. These drive
// the production stack: a real `TransferEngine` over a real `DispatchRunner`
// and a real `ProviderHub` backed by a `SecureStore`. No bypass constructor.
// ===========================================================================

/// Collects every state transition the engine emits, keyed by task id.
#[derive(Default)]
struct CollectingSink {
    seen: std::sync::Mutex<Vec<(String, TransferStatus)>>,
}

impl TransferSink for CollectingSink {
    fn state_changed(&self, task: &TransferTaskDto) {
        self.seen
            .lock()
            .unwrap()
            .push((task.id.clone(), task.status));
    }
}

/// Discards progress batches: these tests observe progress through
/// `engine.snapshot()`, not the throttled IPC batches. The aggregator still has
/// to exist so the engine's progress channel is a real, open channel.
struct NullProgressSink;

impl ProgressSink for NullProgressSink {
    fn flush(&self, _batch: Vec<ProgressPayload>) {}
}

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
        EngineConfig {
            max_tasks: 3,
            max_parts,
        },
        checkpoint_dir,
        resume_enabled,
    )
}

fn build_engine(
    hub: Arc<ProviderHub>,
    sink: Arc<dyn TransferSink>,
    max_parts: usize,
) -> TransferEngine {
    build_engine_cp(hub, sink, max_parts, None, Arc::new(AtomicBool::new(true)))
}

/// A `ProviderHub` over a `SecureStore` in `dir`, with the live RustFS
/// connection persisted through `mutate` (the production path).
async fn live_hub(connection_id: &str, dir: &tempfile::TempDir) -> Arc<ProviderHub> {
    let hub = Arc::new(ProviderHub::new(SecureStore {
        path: dir.path().join("connections.enc"),
    }));
    hub.mutate(|list| {
        list.push(Connection {
            id: connection_id.to_string(),
            ..rustfs_connection(&secret_key())
        });
        Ok(())
    })
    .await
    .expect("persisting the RustFS connection into the hub should succeed");
    hub
}

async fn snapshot_of(engine: &TransferEngine, id: &str) -> Option<TransferTaskDto> {
    engine.snapshot().await.into_iter().find(|t| t.id == id)
}

/// Polls until the task reaches `want`, failing fast (with the error code) if
/// it instead settles into a different terminal state -- a bare timeout costs
/// the next debugger an hour.
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
            if matches!(
                t.status,
                TransferStatus::Completed | TransferStatus::Canceled | TransferStatus::Failed
            ) {
                panic!(
                    "task {id} reached {:?} (error_code={:?}) while waiting for {want:?}",
                    t.status, t.error_code
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out after {budget:?} waiting for {id} to reach {want:?}; last seen: {last:?}");
}

/// Waits until the task is observably `Running` with at least one byte
/// reported -- the only safe moment to pause "mid-flight". Acting the instant
/// after enqueue can land before any part has started, proving nothing.
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
                    "task {id} reached {:?} (error_code={:?}) before any mid-flight progress",
                    t.status, t.error_code
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!(
        "timed out after {budget:?} waiting for {id} to be Running with progress; last: {last:?}"
    );
}

/// The full stack: a ~60MB file enqueued into a real engine runs to
/// `Completed`, and only then does the object exist -- proven by a `HeadObject`
/// size check and a byte-for-byte SHA-256 comparison.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn engine_uploads_a_large_file_end_to_end() {
    let (provider, client, bucket) = fresh_bucket().await;

    let dir = tempfile::tempdir().expect("fixture tempdir");
    let path = dir.path().join("engine-large.bin");
    let size = 60 * MB;
    write_pseudo_random_file(&path, size, 0x5EED_0009);

    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-engine-rustfs";
    let hub = live_hub(connection_id, &hub_dir).await;
    let engine = build_engine(Arc::clone(&hub), Arc::new(CollectingSink::default()), 4);

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
        "HeadObject after Completed must show the full object size"
    );
    assert_eq!(
        hex(&sha256_bytes(
            &get_object_bytes(&client, &bucket, &key).await
        )),
        hex(&sha256_file(&path)),
        "the object the engine assembled must be byte-identical to the source"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// Pausing mid-flight then resuming finishes the upload without losing or
/// duplicating a byte. While paused no complete object exists; after resume the
/// object hash-matches the source.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn pausing_then_resuming_finishes_the_upload() {
    let (provider, client, bucket) = fresh_bucket().await;

    let dir = tempfile::tempdir().expect("fixture tempdir");
    let path = dir.path().join("engine-pause.bin");
    let size = 60 * MB;
    write_pseudo_random_file(&path, size, 0x5EED_0010);

    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-pause-rustfs";
    let hub = live_hub(connection_id, &hub_dir).await;
    // One part at a time: widens the pause window, keeps the timing deterministic.
    let engine = build_engine(Arc::clone(&hub), Arc::new(CollectingSink::default()), 1);

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

    assert_eq!(
        head_object_size(&client, &bucket, &key).await,
        None,
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
    assert_eq!(
        hex(&sha256_bytes(
            &get_object_bytes(&client, &bucket, &key).await
        )),
        hex(&sha256_file(&path)),
        "a paused-then-resumed upload must be byte-identical to the source"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}

/// A large multipart upload paused mid-flight survives an engine drop and
/// resumes to a byte-correct object.
///
/// The checkpoint written by E1 (upload id + accepted parts + source
/// fingerprint) is all that crosses the restart; E2 rebuilds the task as
/// `Paused` with `transferred` preset from those parts, then resume continues
/// the SAME still-open server-side upload. The pending-upload count going
/// 1 -> 0 confirms the very upload E1 opened was the one completed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn upload_survives_a_restart_and_resumes_without_reuploading() {
    let (provider, client, bucket) = fresh_bucket().await;

    let dir = tempfile::tempdir().expect("fixture tempdir");
    let path = dir.path().join("restart-upload.bin");
    let size = 60 * MB;
    write_pseudo_random_file(&path, size, 0x5EED_0011);

    let cp_base = tempfile::tempdir().expect("checkpoint tempdir");
    let cp_dir = cp_base.path().to_path_buf();
    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-restart-rustfs";
    let hub = live_hub(connection_id, &hub_dir).await;

    // --- E1: run mid-flight, pause, and let the checkpoint land -------------
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
        "the paused multipart upload must still be open on the server"
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
    restore_all(&engine2, &hub, &cp_dir).await;

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
        hex(&sha256_bytes(
            &get_object_bytes(&client, &bucket, &key).await
        )),
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

/// The download direction, end to end: a multipart-sized object fetched through
/// the real engine lands byte-identical on disk. Proves `DownloadRunner`'s
/// ranged GETs land at the right offsets against RustFS.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn engine_downloads_a_large_object_matching_hash() {
    let (provider, client, bucket) = fresh_bucket().await;

    let src_dir = tempfile::tempdir().expect("source tempdir");
    let dl_dir = tempfile::tempdir().expect("download tempdir");
    let path = src_dir.path().join("engine-download.bin");
    // Above the download planner's own threshold (64MB, independent of the
    // upload threshold since the M6 split): this must exercise DownloadRunner's
    // multi-chunk path, not just its single-stream one.
    let size = 80 * MB;
    write_pseudo_random_file(&path, size, 0x5EED_0012);

    let key = "downloads/engine-download.bin";
    upload_via_primitives(&provider, &bucket, key, &path, size).await;

    let hub_dir = tempfile::tempdir().expect("hub tempdir");
    let connection_id = "e2e-download-rustfs";
    let hub = live_hub(connection_id, &hub_dir).await;
    let engine = build_engine(Arc::clone(&hub), Arc::new(CollectingSink::default()), 4);

    let target = dl_dir.path().join("engine-download.bin");
    let spec = EnqueueSpec::for_download(
        connection_id.to_string(),
        bucket.clone(),
        key.to_string(),
        target.clone(),
        size,
        "engine-download.bin".to_string(),
    );
    let task = engine.enqueue(spec).await.expect("enqueue");

    wait_for_status(
        &engine,
        &task.id,
        TransferStatus::Completed,
        Duration::from_secs(180),
    )
    .await;

    assert_eq!(
        std::fs::metadata(&target)
            .expect("downloaded file must exist")
            .len(),
        size,
        "the downloaded file must be exactly the object's size"
    );
    assert_eq!(
        hex(&sha256_file(&target)),
        hex(&sha256_file(&path)),
        "every ranged chunk must have landed at its correct offset"
    );

    cleanup_bucket(&client, &provider, &bucket).await;
    drop(hub);
}
