//! Aliyun OSS end-to-end integration tests (opt-in, LIVE).
//!
//! **These tests hit a real Aliyun OSS endpoint.** Unlike `minio_e2e.rs`
//! (a disposable local container), the target account may hold **production
//! data in other buckets**, and every call here costs real money (both
//! requests and egress). Every test in this file is `#[ignore]`d, so a
//! plain `cargo test` never touches the network or any Aliyun credential --
//! this suite is opt-in, run explicitly, never part of the hermetic default
//! suite.
//!
//! ## Why this file exists
//!
//! BucketCat talks to every provider through one `aws-sdk-s3` client. Aliyun
//! OSS is reached via its S3-compatible endpoint, which needed three
//! OSS-specific accommodations in `provider::s3` (see that module's doc
//! comments for the full rationale):
//!
//! - `uses_path_style` returns `false` for `provider == "oss"` -- OSS
//!   accepts only virtual-hosted addressing and rejects path-style requests
//!   outright.
//! - `s3_compat_endpoint` prefixes an OSS endpoint's host with `s3.`
//!   (`oss-cn-beijing.aliyuncs.com` -> `s3.oss-cn-beijing.aliyuncs.com`),
//!   idempotently.
//! - `with_scheme` prefixes a schemeless endpoint with `https://`.
//! - Pre-existing, non-OSS-specific logic sets
//!   `RequestChecksumCalculation::WhenRequired` for any non-AWS endpoint, so
//!   the SDK never sends `aws-chunked` transfer encoding -- which OSS's
//!   S3-compatible endpoint does not support and rejects with
//!   `InvalidArgument`.
//!
//! Those three behaviors were, until this file, covered only by unit tests
//! against synthetic endpoint strings -- never proven against a real OSS
//! server. This suite closes that gap, and in particular
//! `multipart_upload_round_trip` (below) is the one test that would catch a
//! regression back to `aws-chunked` (a purely unit-tested config value can't
//! prove the wire behavior it's meant to prevent).
//!
//! ## ⚠️ Hard safety rules this file obeys
//!
//! - **No bucket lifecycle calls, ever.** This file must never call
//!   `create_bucket` or `delete_bucket`. It operates against exactly one
//!   pre-existing, dedicated sandbox bucket that a human already created and
//!   owns.
//! - **No hardcoded bucket name.** The target bucket comes *only* from
//!   `BUCKETCAT_OSS_BUCKET`, which has no default. This repository is
//!   public; a hardcoded real bucket name must never appear here.
//! - **A single, unique, throwaway key prefix per test.** Every test that
//!   writes data calls [`test_prefix`] to get its own
//!   `bucketcat-e2e/{uuid}/` prefix, so concurrent runs (and repeated runs)
//!   never collide, and cleanup can never reach outside that prefix.
//! - **Cleanup is prefix-scoped and self-checking.** [`cleanup_prefix`]
//!   refuses (via `assert!`) to run against anything that isn't a real,
//!   non-empty `bucketcat-e2e/` prefix, so a coding mistake that produced an
//!   empty or wrong prefix can never cascade into deleting unrelated
//!   objects.
//! - **Credentials never touch disk or a log line.** They're read from
//!   environment variables at process start and handed straight to the SDK;
//!   nothing in this file prints, formats, or persists them. (The presigned
//!   URL in [`presigned_get_works`] carries a live signature and also must
//!   never be logged -- it isn't.)
//!
//! ## Running these tests
//!
//! You need a **dedicated, disposable OSS bucket** that you're comfortable
//! having a `bucketcat-e2e/` prefix written to and deleted from repeatedly.
//! Do **not** point this at a bucket that holds anything you care about --
//! nothing here creates or deletes the bucket itself, but writes/deletes
//! happen for real under that prefix.
//!
//! Set all five required environment variables, then run with `--ignored`:
//!
//! ```bash
//! export BUCKETCAT_OSS_ENDPOINT="https://oss-cn-beijing.aliyuncs.com"  # NATIVE form;
//!                                                                       # the code rewrites
//!                                                                       # this to the `s3.`
//!                                                                       # compat form itself
//! export BUCKETCAT_OSS_REGION="cn-beijing"
//! export BUCKETCAT_OSS_BUCKET="your-disposable-sandbox-bucket"
//! export BUCKETCAT_OSS_AK="..."
//! export BUCKETCAT_OSS_SK="..."
//! cargo test --test oss_e2e -- --ignored
//! ```
//!
//! Any variable left unset makes every test `panic!` immediately with a
//! message naming the missing variable, rather than silently defaulting to
//! anything -- there is no safe default for a real cloud account.
//!
//! `BUCKETCAT_OSS_ENDPOINT` is deliberately documented as the endpoint's
//! *native* form (no `s3.` prefix): `s3_compat_endpoint` doing that rewrite
//! is exactly the behavior [`list_objects_on_the_test_bucket_succeeds`]
//! proves, mirroring what a real user pastes from the Aliyun console.

use bucketcat_lib::provider::{from_connection, Provider, S3Provider, UploadedPart};
use bucketcat_lib::store::Connection;
use sha2::{Digest, Sha256};
use std::path::Path;

/// 1 MiB, the unit the multipart fixture is sized in.
const MB: u64 = 1024 * 1024;

// --- env / connection helpers ------------------------------------------------

/// Reads a required environment variable, panicking with a clear pointer to
/// this file's module doc comment when it's missing -- there is no safe
/// default for any of these against a real cloud account.
fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!(
            "missing required environment variable `{name}` -- see oss_e2e.rs's module doc \
             comment for the full list of required variables and how to run this suite"
        )
    })
}

/// The dedicated sandbox bucket these tests operate against. **No
/// default, ever** -- see this file's module doc comment and the safety
/// rules it documents.
fn oss_bucket() -> String {
    required_env("BUCKETCAT_OSS_BUCKET")
}

/// A `Connection` pointed at the live OSS endpoint from the environment.
/// `provider` is `"oss"`, which is what activates `uses_path_style`'s
/// virtual-hosted addressing, `s3_compat_endpoint`'s `s3.` rewrite, and
/// `with_scheme`'s `https://` default inside `provider::s3::from_connection`
/// -- see this file's module doc comment.
fn oss_connection() -> Connection {
    Connection {
        id: "e2e-oss".to_string(),
        provider: "oss".to_string(),
        name: "e2e oss".to_string(),
        endpoint: required_env("BUCKETCAT_OSS_ENDPOINT"),
        region: required_env("BUCKETCAT_OSS_REGION"),
        access_key_id: required_env("BUCKETCAT_OSS_AK"),
        secret_access_key: required_env("BUCKETCAT_OSS_SK"),
        default_bucket: None,
    }
}

/// A fresh `bucketcat-e2e/{uuid}/` prefix, unique per call -- every writing
/// test gets its own so concurrent/repeated runs never collide, and
/// [`cleanup_prefix`] never has to reason about shared state.
fn test_prefix() -> String {
    format!("bucketcat-e2e/{}/", uuid::Uuid::new_v4())
}

/// Deletes everything under `prefix` via [`Provider::delete_prefix`].
///
/// Refuses to run at all against anything that isn't a real, non-empty
/// `bucketcat-e2e/` prefix -- this is the last line of defense against a
/// coding mistake (an empty or mistyped prefix) turning cleanup into
/// "delete the whole bucket". Failures are logged, not panicked on: a
/// cleanup failure must never mask (or overwrite the exit status of) the
/// actual test assertion that ran before it.
async fn cleanup_prefix(provider: &S3Provider, bucket: &str, prefix: &str) {
    assert!(
        prefix.starts_with("bucketcat-e2e/") && prefix.len() > "bucketcat-e2e/".len(),
        "refusing to clean up a prefix that is not a real, non-empty `bucketcat-e2e/{{uuid}}/` \
         prefix: `{prefix}` -- this guard exists specifically to prevent an empty/mistyped \
         prefix from ever reaching delete_prefix"
    );
    if let Err(e) = provider.delete_prefix(bucket, prefix).await {
        eprintln!(
            "WARNING: cleanup_prefix failed to delete `{prefix}` in bucket `{bucket}`: {e} -- \
             manual cleanup of this prefix in the OSS console may be needed"
        );
    }
}

// --- fixture / hashing helpers (mirrors minio_e2e.rs) ------------------------

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

// --- 1: read-only smoke test -------------------------------------------------

/// The cheapest possible proof that virtual-hosted addressing, the `s3.`
/// endpoint prefix, and the region are all simultaneously correct: a bare
/// `ListObjectsV2` against the sandbox bucket's root. Path-style addressing
/// (which OSS rejects), a missing `s3.` prefix, or a wrong region would each
/// independently fail this call, so a green result here rules out all three
/// at once before the writing tests below even run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn list_objects_on_the_test_bucket_succeeds() {
    let conn = oss_connection();
    let bucket = oss_bucket();
    let provider = from_connection(&conn).expect("a valid OSS connection profile should build");

    provider.list_objects(&bucket, "", None, 10).await.expect(
        "list_objects against the live OSS sandbox bucket should succeed -- a failure here \
         means virtual-hosted addressing, the `s3.` endpoint prefix, or the region is wrong; \
         see this file's module doc comment",
    );
}

// --- 2: account-level ListBuckets over the S3-compatible endpoint ------------

/// Account-level `ListBuckets` **works** through OSS's S3-compatible
/// endpoint, contradicting Aliyun's published compatibility list.
///
/// Aliyun's "OSS 兼容的 S3 API" page enumerates the supported S3 operations
/// as Bucket + Object + Multipart only, and account-level
/// `ListBuckets`/`GetService` is absent from it. BucketCat's design
/// therefore planned a native-API (OSS4-HMAC-SHA256) call just to list
/// buckets. **Verified empirically against a real account on 2026-07-25:
/// `ListBuckets` succeeds over the S3-compatible endpoint**, so that native
/// call is not needed. This test pins the behavior we actually depend on --
/// if Aliyun ever removes it, this fails loudly and the native fallback
/// becomes necessary again.
///
/// Known limitation (the reason the native call still has *some* value):
/// the S3 response carries only name + creation date, **not** each bucket's
/// region. OSS does not redirect cross-region requests -- it answers
/// `NoSuchBucket` -- so on an account whose buckets span regions, buckets
/// outside the connection's configured region will list fine but fail on
/// access. OSS's *native* `ListBuckets` returns a `Location` per bucket and
/// would let the UI resolve that automatically.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn s3_compat_list_buckets_works() {
    let conn = oss_connection();
    let provider = from_connection(&conn).expect("a valid OSS connection profile should build");

    let buckets = provider.list_buckets().await.expect(
        "OSS's S3-compatible endpoint should serve account-level ListBuckets (verified \
         2026-07-25 against a real account, despite its absence from Aliyun's published \
         S3 compatibility list); if this now fails, OSS's behavior has changed and the \
         native-API ListBuckets fallback from the design doc becomes necessary",
    );

    // The configured sandbox bucket must be in its own account listing --
    // proves we read a real account-level response, not an empty stub.
    let bucket = oss_bucket();
    assert!(
        buckets.iter().any(|b| b.name == bucket),
        "the configured test bucket `{bucket}` should appear in its own account's bucket \
         listing, got: {buckets:?}"
    );
}

// --- 3: small object round trip ----------------------------------------------

/// Small (tens-of-KB) object round trip: put -> head (size) -> get_range
/// (whole object) -> SHA-256 match -> delete -> prefix cleanup. The
/// baseline data-plane proof, independent of the multipart / `aws-chunked`
/// concern the next test targets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn small_object_round_trip() {
    let conn = oss_connection();
    let bucket = oss_bucket();
    let provider = from_connection(&conn).expect("a valid OSS connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}small.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("small.bin");
    let size = 48 * 1024; // a few tens of KB
    write_pseudo_random_file(&path, size, 0x0555_0001);

    provider
        .put_object_from_file(&bucket, &key, &path, size)
        .await
        .expect("put_object_from_file should succeed against live OSS");

    let head = provider
        .head_object(&bucket, &key)
        .await
        .expect("head_object should succeed right after put_object_from_file");
    assert_eq!(
        head.size, size,
        "head_object's reported size must match the uploaded file's size"
    );

    let downloaded = provider
        .get_range(&bucket, &key, 0, size)
        .await
        .expect("get_range should succeed reading the whole object back");
    assert_eq!(
        hex(&sha256_bytes(&downloaded)),
        hex(&sha256_file(&path)),
        "round-tripped bytes must match the source file's SHA-256"
    );

    provider
        .delete_objects(&bucket, std::slice::from_ref(&key))
        .await
        .expect("delete_objects should succeed against live OSS");

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 4: multipart round trip (the aws-chunked risk) --------------------------

/// The most important test in this file: a ~12MB multipart upload split
/// into 2 parts of ~6MB each (each above S3/OSS's 5MB minimum part size for
/// every part but the last).
///
/// This specifically targets the `aws-chunked` risk `provider::s3`'s doc
/// comments call out: OSS's S3-compatible endpoint does not support
/// `aws-chunked` transfer encoding, and if the SDK's checksum-calculation
/// config ever regresses back to sending it for a non-AWS endpoint, the
/// symptom is exactly `upload_part_from_file` failing with `InvalidArgument`
/// -- which is why the panic message on that failure calls out that
/// specific possibility. A green run here proves multipart upload works
/// end-to-end against real OSS: init -> 2 parts -> `multipart_list` sees
/// both -> complete -> head (size) -> get_range (whole object) -> SHA-256
/// match -> delete -> prefix cleanup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn multipart_upload_round_trip() {
    let conn = oss_connection();
    let bucket = oss_bucket();
    let provider = from_connection(&conn).expect("a valid OSS connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}multipart.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("multipart.bin");
    let part_size = 6 * MB;
    let total = part_size * 2; // ~12MB, two ~6MB parts.
    write_pseudo_random_file(&path, total, 0x0555_0002);

    let upload_id = provider
        .multipart_init(&bucket, &key)
        .await
        .expect("multipart_init should succeed against live OSS");

    let mut done: Vec<UploadedPart> = Vec::with_capacity(2);
    for (number, offset) in [(1i32, 0u64), (2i32, part_size)] {
        let etag = provider
            .upload_part_from_file(&bucket, &key, &upload_id, number, &path, offset, part_size)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "upload_part_from_file (part {number}) should succeed against live OSS -- \
                     a failure here, especially `InvalidArgument`, most likely means the \
                     client sent `aws-chunked` transfer encoding, which OSS's S3-compatible \
                     endpoint does not support (see this file's and provider::s3's module doc \
                     comments): {e}"
                )
            });
        done.push(UploadedPart {
            number,
            etag,
            size: part_size,
        });
    }

    let listed = provider
        .multipart_list(&bucket, &key, &upload_id)
        .await
        .expect("multipart_list should succeed against live OSS");
    assert_eq!(
        listed.len(),
        2,
        "the server must report exactly the 2 parts uploaded, got: {listed:?}"
    );

    provider
        .multipart_complete(&bucket, &key, &upload_id, &done)
        .await
        .expect("multipart_complete should succeed against live OSS");

    let head = provider
        .head_object(&bucket, &key)
        .await
        .expect("head_object should succeed after multipart_complete");
    assert_eq!(
        head.size, total,
        "the completed object's size must equal the source file's total size"
    );

    let downloaded = provider
        .get_range(&bucket, &key, 0, total)
        .await
        .expect("get_range should succeed reading the whole object back");
    assert_eq!(
        hex(&sha256_bytes(&downloaded)),
        hex(&sha256_file(&path)),
        "round-tripped bytes must match the source file's SHA-256 -- a mismatch would mean a \
         part landed at the wrong offset"
    );

    provider
        .delete_objects(&bucket, std::slice::from_ref(&key))
        .await
        .expect("delete_objects should succeed against live OSS");

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 5: presigned GET (resolves an open design question) --------------------

/// Resolves an open design-doc question: does OSS's S3-compatible endpoint
/// honor presigned GET URLs the way `aws-sdk-s3` signs them? This matters
/// directly for BucketCat's Share and preview features.
///
/// Upload a small object, `presign_get` it, then fetch the URL with `curl`
/// carrying **no credentials at all** -- the entire point of a presigned URL
/// is that the signature embedded in the query string is sufficient on its
/// own. A SHA-256 match against the source proves not just that the request
/// succeeded, but that it returned the exact intended bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn presigned_get_works() {
    let conn = oss_connection();
    let bucket = oss_bucket();
    let provider = from_connection(&conn).expect("a valid OSS connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}share.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("share.bin");
    let size = 32 * 1024;
    write_pseudo_random_file(&path, size, 0x0555_0003);

    provider
        .put_object_from_file(&bucket, &key, &path, size)
        .await
        .expect("put_object_from_file should succeed against live OSS");

    let url = provider
        .presign_get(&bucket, &key, 3600)
        .await
        .expect("presign_get should succeed against live OSS");

    // curl the presigned URL WITHOUT any credentials -- the whole point of a
    // presigned URL is that the signature in the query string is enough.
    // The URL itself carries a live signature and must never be logged.
    let out = dir.path().join("fetched.bin");
    let status = std::process::Command::new("curl")
        .args([
            "-fsS",
            "-o",
            out.to_str().expect("tempdir path is utf8"),
            &url,
        ])
        .status()
        .expect("curl should be runnable as a subprocess");
    assert!(
        status.success(),
        "the presigned GET URL must be fetchable via an unauthenticated curl request -- if \
         this fails, the conclusion is that OSS's S3-compatible endpoint does NOT support \
         presigned GET URLs the way `aws-sdk-s3` signs them, which would block the Share and \
         preview features for OSS connections and needs a design-doc follow-up"
    );
    assert_eq!(
        hex(&sha256_file(&out)),
        hex(&sha256_file(&path)),
        "the fetched bytes must match the source file's SHA-256 -- if this fails while curl \
         reported success, OSS accepted the presigned URL but served different content than \
         intended"
    );

    provider
        .delete_objects(&bucket, std::slice::from_ref(&key))
        .await
        .expect("delete_objects should succeed against live OSS");

    cleanup_prefix(&provider, &bucket, &prefix).await;
}
