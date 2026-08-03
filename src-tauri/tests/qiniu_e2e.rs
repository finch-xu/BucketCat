//! Qiniu Kodo end-to-end integration tests (opt-in, LIVE).
//!
//! **These tests hit a real Qiniu Kodo endpoint.** Like `oss_e2e.rs` and
//! `rainyun_e2e.rs` (and unlike `minio_e2e.rs`, which drives a disposable
//! local container), the target account is a real, paid one whose buckets may
//! hold data that matters. Every test in this file is `#[ignore]`d, so a plain
//! `cargo test` never touches the network, never reads a single one of the
//! environment variables below, and never sees a credential -- this suite is
//! opt-in, run explicitly, never part of the hermetic default suite.
//!
//! ## What this suite proves about Qiniu specifically
//!
//! Qiniu's S3-compatible interface turned out to be the *most* AWS-faithful of
//! the Chinese providers BucketCat targets. Everything below was probed
//! directly against a live account on 2026-07-30 before any code was written;
//! these tests are what keep those findings from silently rotting.
//!
//! - **SigV4, header and query-string, both work.** `aws-sdk-s3` signs SigV4
//!   unconditionally, so nothing has to opt in -- but the presigned-URL test
//!   would fail loudly if that changed.
//! - **The credential-scope region is not validated.** Signing for
//!   `us-east-1` against the `cn-east-1` host still returns 200. The app
//!   stores the real region id anyway (the UI reads better, and the *host*
//!   very much does matter -- see cross-region routing below).
//! - **Path-style AND virtual-hosted both work.** BucketCat stays on
//!   path-style, which `uses_path_style` already gives every non-AWS, non-OSS
//!   provider. [`list_objects_on_the_test_bucket_succeeds`] is the sentinel
//!   against a future change moving Qiniu into the virtual-hosted branch
//!   alongside AWS/OSS: unlike Rainyun (whose gateway hard-fails that shape),
//!   Qiniu would keep *working*, so only an explicit assertion can catch it.
//! - **Multi-Object Delete works.** This is the big divergence from OSS and
//!   Rainyun, both of which hard-require the legacy `Content-MD5` header that
//!   `aws-sdk-s3` cannot send, forcing `supports_batch_delete` to return
//!   `false` for them. Qiniu accepts the `x-amz-checksum-crc32` that
//!   `aws-sdk-s3` actually sends (200), while the same request with no
//!   checksum header at all is rejected with
//!   `400 InvalidRequest: Missing required header for this request:
//!   Content-MD5`. So Qiniu keeps the fast batch path.
//!   [`batch_delete_uses_the_multi_object_path`] is the test that matters
//!   most here: the pre-implementation probe was hand-rolled curl, so this
//!   suite's first run is the first time a *real* `aws-sdk-s3`
//!   `DeleteObjects` (with its `x-amz-sdk-checksum-algorithm` and friends)
//!   has gone at the endpoint. Rainyun's identical assumption is exactly what
//!   its own suite's first run demolished.
//! - **`aws-chunked` must not be sent.** Covered by the pre-existing,
//!   non-Qiniu-specific rule in `provider::s3::build_client` that any non-AWS
//!   endpoint gets `RequestChecksumCalculation::WhenRequired`.
//!   [`multipart_upload_round_trip`] is the only test that can catch a
//!   regression there: a unit test on a config value cannot prove wire
//!   behavior, and it is the streamed, multi-part upload path that actually
//!   trips over it.
//!
//! ## The actual special case: cross-region routing
//!
//! Qiniu's `ListBuckets` is **account-level and global** -- called against
//! any one region's host it reports every bucket the account owns, across all
//! eight regions. But an object-plane request for a bucket that lives
//! elsewhere is rejected outright with `400 IncorrectRegion` ("The bucket
//! region not match with the service region"), not routed internally.
//!
//! That is structurally the same problem Aliyun OSS has, so
//! `provider::s3::RegionRouting` (generalized out of the former
//! OSS-only `OssNativeAuth` for exactly this) handles both. The Qiniu-specific
//! parts:
//!
//! - Regions come from `GetBucketLocation`, one request per bucket, because
//!   `aws-sdk-s3` 1.139 only deserializes AWS's `<BucketRegion>` tag and
//!   therefore silently drops the `<LocationConstraint>` Qiniu *does* put in
//!   its `ListBuckets` response.
//! - `GetBucketLocation` itself is cross-region-callable: a `cn-north-1`
//!   bucket resolves correctly through the `ap-southeast-1` host. That is what
//!   makes the whole scheme work from a connection configured for any region.
//! - `qiniu_endpoint_for_region` rewrites `s3.{from}.` -> `s3.{to}.` in the
//!   host.
//!
//! [`cross_region_bucket_is_routed_automatically`] is the load-bearing test
//! for all of that, and the analogue of `oss_e2e.rs`'s same-named test. It
//! deliberately configures the connection for the *wrong* region and asserts
//! the object plane still works.
//!
//! ## Safety rules (identical to `rainyun_e2e.rs`)
//!
//! - **Nothing creates or deletes a bucket.** Only objects, and only under
//!   this suite's own prefix.
//! - **Every write is prefix-scoped.** Each test that writes data calls
//!   [`test_prefix`] for its own `bucketcat-e2e/{uuid}/` prefix, so concurrent
//!   (and repeated) runs never collide and cleanup can never reach outside
//!   that prefix.
//! - **Cleanup is prefix-scoped and self-checking.** [`cleanup_prefix`]
//!   refuses (via `assert!`) to run against anything that isn't a real,
//!   non-empty `bucketcat-e2e/` prefix. Its reach is real but not total:
//!   `delete_prefix` enumerates via `ListObjectsV2`, which never lists an
//!   **in-progress** multipart upload -- so `cleanup_prefix` alone can never
//!   reach or abort one. [`multipart_upload_round_trip`] is the one test that
//!   can leave one behind, and it guards that window itself: every failure
//!   path in it explicitly calls `multipart_abort` before `cleanup_prefix`
//!   runs.
//! - **Credentials never touch disk or a log line.** They are read from
//!   environment variables at test start and handed straight to the SDK. The
//!   presigned URL in [`presigned_get_works`] carries a live signature and is
//!   treated the same way: never printed, never passed as a subprocess
//!   argument (where `ps` would expose it), never interpolated into an
//!   assertion message. That is why that test fetches with the in-process
//!   `hyper` + `hyper-rustls` client already in this crate's dependency graph
//!   rather than shelling out to `curl`.
//!
//! ## Running these tests
//!
//! You need a **dedicated, disposable Qiniu bucket** you are comfortable
//! having a `bucketcat-e2e/` prefix written to and deleted from repeatedly.
//! Do **not** point this at a bucket holding anything you care about --
//! nothing here creates or deletes the bucket itself, but writes and deletes
//! happen for real under that prefix.
//!
//! Unlike Rainyun (where each region is a separate cluster with its own
//! credentials), **one Qiniu key pair covers the whole account**, and
//! `ListBuckets` is global. So the endpoint's region does *not* have to match
//! the bucket's -- indeed [`cross_region_bucket_is_routed_automatically`]
//! relies on being able to point at the wrong one on purpose.
//!
//! ```bash
//! export BUCKETCAT_QINIU_ENDPOINT="https://s3.cn-north-1.qiniucs.com"
//! export BUCKETCAT_QINIU_REGION="cn-north-1"
//! export BUCKETCAT_QINIU_BUCKET="your-disposable-sandbox-bucket"
//! export BUCKETCAT_QINIU_AK="..."
//! export BUCKETCAT_QINIU_SK="..."
//! cargo test --test qiniu_e2e -- --ignored
//! ```
//!
//! Any variable left unset makes the test `panic!` immediately with a message
//! naming the missing variable, rather than silently defaulting to anything --
//! there is no safe default for a real cloud account.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use sha2::{Digest, Sha256};
use std::path::Path;

use bucketcat_lib::error::{AppError, AppResult};
use bucketcat_lib::provider::s3::{
    qiniu_endpoint_for_region, supports_batch_delete, uses_path_style,
};
use bucketcat_lib::provider::{from_connection, Provider, S3Provider, UploadedPart};
use bucketcat_lib::store::Connection;

/// 1 MiB, the unit the multipart fixture is sized in.
const MB: u64 = 1024 * 1024;

/// Reads `[offset, offset+length)` of `key` into a `Vec`, standing in for the
/// whole-buffer `Provider::get_range` these tests were originally written
/// against -- Task 4 replaced it with the streaming `Provider::open_range`,
/// so this suite drains the returned reader itself.
async fn get_range_bytes(
    provider: &S3Provider,
    bucket: &str,
    key: &str,
    offset: u64,
    length: u64,
) -> AppResult<Vec<u8>> {
    let mut reader = provider.open_range(bucket, key, offset, length).await?;
    let mut buf = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf)
        .await
        .map_err(|e| AppError::Internal {
            message: format!("reading range stream: {e}"),
        })?;
    Ok(buf)
}

// --- env / connection helpers ------------------------------------------------

/// Reads a required environment variable, panicking with a clear pointer to
/// this file's module doc comment when it's missing -- there is no safe
/// default for any of these against a real cloud account.
fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!(
            "missing required environment variable `{name}` -- see qiniu_e2e.rs's module doc \
             comment for the full list of required variables and how to run this suite"
        )
    })
}

/// The dedicated sandbox bucket these tests operate against. **No default,
/// ever** -- see this file's module doc comment and the safety rules it
/// documents. Nothing in this file creates or deletes it.
fn qiniu_bucket() -> String {
    required_env("BUCKETCAT_QINIU_BUCKET")
}

/// A `Connection` pointed at the live Qiniu endpoint from the environment.
///
/// `provider` is `"qiniu"`, which `provider::s3` branches on in exactly two
/// places: `from_connection` captures a `RegionRouting` with
/// `RegionFlavor::Qiniu` (so `client_for` can route per bucket), and
/// `provider_str`/`endpoint_for_region` dispatch off that flavor. Everywhere
/// else it rides the generic non-AWS, non-OSS path unchanged --
/// `uses_path_style`'s "everything else" branch (path-style),
/// `s3_compat_endpoint`'s non-OSS passthrough (endpoint used verbatim),
/// `supports_batch_delete`'s default `true`, and the non-AWS
/// `RequestChecksumCalculation::WhenRequired` rule (no `aws-chunked`).
fn qiniu_connection() -> Connection {
    Connection {
        id: "e2e-qiniu".to_string(),
        provider: "qiniu".to_string(),
        name: "e2e qiniu".to_string(),
        endpoint: required_env("BUCKETCAT_QINIU_ENDPOINT"),
        region: required_env("BUCKETCAT_QINIU_REGION"),
        access_key_id: required_env("BUCKETCAT_QINIU_AK"),
        secret_access_key: required_env("BUCKETCAT_QINIU_SK"),
        default_bucket: None,
        api_token: None,
    }
}

/// The same connection, deliberately pointed at a **different** region's host
/// than the sandbox bucket actually lives in -- the fixture for
/// [`cross_region_bucket_is_routed_automatically`].
///
/// Picks the other region mechanically (`cn-east-1` unless that's already the
/// configured one, in which case `cn-north-1`) so this works regardless of
/// which region the caller configured, and rewrites the endpoint host with the
/// same [`qiniu_endpoint_for_region`] the routing code itself uses.
///
/// Note that only the *host* is wrong: the credentials are account-wide, and
/// the credential-scope region isn't validated at all (see the module doc). So
/// this isolates exactly one variable -- whether the object plane reaches a
/// bucket whose region differs from the connection's.
fn qiniu_connection_pointed_at_another_region() -> Connection {
    let mut conn = qiniu_connection();
    let elsewhere = if conn.region == "cn-east-1" {
        "cn-north-1"
    } else {
        "cn-east-1"
    };
    conn.endpoint = qiniu_endpoint_for_region(&conn.endpoint, &conn.region, elsewhere);
    conn.region = elsewhere.to_string();
    conn
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
/// coding mistake (an empty or mistyped prefix) turning cleanup into "delete
/// the whole bucket". Failures are logged, not panicked on: a cleanup failure
/// must never mask (or overwrite the exit status of) the actual test assertion
/// that ran before it.
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
             manual cleanup of this prefix in the Qiniu console may be needed"
        );
    }
}

// --- fixture / hashing helpers (mirrors rainyun_e2e.rs) ----------------------

/// Writes `size` bytes of deterministic LCG pseudo-random content to `path`.
///
/// Seeded (so the source hash is reproducible run to run) and, crucially,
/// non-compressible -- a file of repeated bytes would let a per-part offset
/// bug go unnoticed because every offset reads the same value. Knuth's MMIX
/// LCG constants give that without dragging a `rand` dev-dependency into the
/// test build. Seeds here live in their own `0x0999_` namespace, distinct from
/// `oss_e2e.rs`'s `0x0555_`, `rainyun_e2e.rs`'s `0x0777_` and
/// `minio_e2e.rs`'s `0x5EED_`.
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// --- anonymous HTTP fetch (for the presigned-URL test) ----------------------

/// Fetches `url` with **no credentials of any kind** and returns
/// `(status, body)`.
///
/// Built on the in-process `hyper` + `hyper-rustls` client already in this
/// crate's dependency graph rather than shelling out to `curl`. That is a
/// deliberate safety choice, not a style preference: the only caller passes a
/// **presigned URL whose query string carries a live signature**, and a
/// subprocess argument is visible to every other process on the machine via
/// `ps`. Keeping the request in-process means the URL never leaves this
/// address space.
///
/// The `url` itself is never included in any error string this returns --
/// `http::uri::InvalidUri` renders only the kind of syntax problem, and
/// `hyper_util::client::legacy::Error`'s `Display` renders only its kind plus
/// cause, never the request URI.
async fn fetch_anonymously(url: &str) -> Result<(u16, Vec<u8>), String> {
    let uri: http::Uri = url
        .parse()
        .map_err(|e| format!("the presigned URL did not parse as a URI: {e}"))?;

    let crypto = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let connector = HttpsConnectorBuilder::new()
        .with_provider_and_native_roots(crypto)
        .map_err(|e| format!("TLS setup for the anonymous fetch failed: {e}"))?
        .https_or_http()
        .enable_http1()
        .build();
    let client = Client::builder(TokioExecutor::new()).build::<_, Empty<Bytes>>(connector);

    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(uri)
        .body(Empty::<Bytes>::new())
        .map_err(|e| format!("building the anonymous GET request failed: {e}"))?;

    let response = client
        .request(request)
        .await
        .map_err(|e| format!("the anonymous GET never got a response: {e}"))?;
    let status = response.status().as_u16();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("reading the anonymous GET's response body failed: {e}"))?
        .to_bytes()
        .to_vec();

    Ok((status, body))
}

// --- 1: global ListBuckets + per-bucket regions ------------------------------

/// `ListBuckets` works and every bucket comes back with a resolved region.
///
/// Two distinct claims, which is why they share a test -- the second is
/// meaningless without the first:
///
/// 1. The S3-compatible account-level `ListBuckets` answers directly (no
///    native-API detour like OSS needs), and the sandbox bucket is in it.
/// 2. `S3Provider::list_buckets_qiniu` filled in each bucket's region via its
///    `GetBucketLocation` fan-out. `region: None` on the sandbox bucket would
///    mean that fan-out silently failed -- which is non-fatal by design (the
///    listing still succeeds) and would therefore go completely unnoticed
///    without this assertion, taking cross-region routing down with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn list_buckets_reports_each_bucket_region() {
    let conn = qiniu_connection();
    let bucket = qiniu_bucket();
    let provider = from_connection(&conn).expect("a valid Qiniu connection profile should build");

    let buckets = provider.list_buckets().await.expect(
        "list_buckets against the live Qiniu endpoint should succeed -- Qiniu answers the \
         S3-compatible account-level ListBuckets directly, so a failure here means the \
         endpoint, the SigV4 credentials, or path-style addressing is wrong; see this file's \
         module doc comment",
    );

    let found = buckets
        .iter()
        .find(|b| b.name == bucket)
        .unwrap_or_else(|| {
            panic!(
                "the configured sandbox bucket `{bucket}` must appear in its own account's \
                 bucket listing. Qiniu's ListBuckets is account-level and GLOBAL (every region's \
                 buckets, from any endpoint), so unlike Rainyun this cannot be a \
                 region-mismatch problem -- check BUCKETCAT_QINIU_BUCKET / _AK / _SK. Saw: \
                 {buckets:?}"
            )
        });

    let region = found.region.as_deref().unwrap_or_else(|| {
        panic!(
            "`{bucket}` came back with no region. Qiniu's own ListBuckets response carries a \
             `<LocationConstraint>` per bucket, but aws-sdk-s3 drops it (it only parses AWS's \
             `<BucketRegion>` tag), so `list_buckets_qiniu` resolves regions with a \
             GetBucketLocation fan-out instead -- a `None` here means that fan-out failed for \
             this bucket. It is deliberately non-fatal to the listing, which is exactly why it \
             needs asserting: without a region, `client_for` cannot route and every \
             cross-region request will fail with `IncorrectRegion`. Run with \
             RUST_LOG=debug to see the underlying error."
        )
    });
    assert!(
        !region.is_empty(),
        "`{bucket}`'s region came back as the empty string. `list_buckets_qiniu` is supposed to \
         normalize an empty `LocationConstraint` to `None` rather than cache `\"\"` (which \
         `client_for` would then try to build a client for) -- seeing `Some(\"\")` here means \
         that normalization broke"
    );
}

// --- 2: read-only smoke test / the path-style regression sentinel -----------

/// The cheapest proof that path-style addressing and SigV4 are simultaneously
/// correct: a bare `ListObjectsV2` against the sandbox bucket's root.
///
/// **This is the path-style regression sentinel** named in the module doc, and
/// it works differently from Rainyun's. Rainyun's gateway hard-fails
/// virtual-hosted requests (`404 Route Not Found`), so any accidental switch
/// shows up as a broken suite. Qiniu accepts *both* shapes -- verified live --
/// so a change moving Qiniu into the virtual-hosted branch alongside AWS/OSS
/// would keep every other test in this file passing. Only the explicit
/// `uses_path_style` assertion below can catch it, which is why it runs first
/// and on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn list_objects_on_the_test_bucket_succeeds() {
    let conn = qiniu_connection();
    let bucket = qiniu_bucket();

    assert!(
        uses_path_style(&conn.provider, &conn.endpoint),
        "BucketCat keeps Qiniu on path-style addressing. `uses_path_style(\"{}\", ..)` returned \
         false, which means someone added Qiniu to the virtual-hosted branch alongside AWS/OSS. \
         Qiniu happens to support both, so nothing else in this suite would have failed -- see \
         this file's module doc comment",
        conn.provider
    );

    let provider = from_connection(&conn).expect("a valid Qiniu connection profile should build");

    provider.list_objects(&bucket, "", None, 10).await.expect(
        "list_objects against the live Qiniu sandbox bucket should succeed -- a failure here \
         means path-style addressing, the endpoint, the credentials, or SigV4 signing is wrong",
    );
}

// --- 3: small object round trip ----------------------------------------------

/// Small-object round trip: put -> head (size) -> get_range (whole object) ->
/// SHA-256 match -> delete -> proof it's gone -> prefix cleanup. The baseline
/// data-plane proof, independent of the multipart / `aws-chunked` concern the
/// next test targets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn small_object_round_trip() {
    let conn = qiniu_connection();
    let bucket = qiniu_bucket();
    let provider = from_connection(&conn).expect("a valid Qiniu connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}small.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("small.bin");
    let size = 48 * 1024;
    write_pseudo_random_file(&path, size, 0x0999_0001);
    let source_hash = hex(&sha256_bytes(
        &std::fs::read(&path).expect("reading the fixture back should succeed"),
    ));

    provider
        .put_object_from_file(&bucket, &key, &path, size, Arc::new(|_| {}))
        .await
        .expect("put_object_from_file should succeed against live Qiniu");

    let head = provider
        .head_object(&bucket, &key)
        .await
        .expect("head_object should succeed right after put_object_from_file");
    assert_eq!(head.size, size, "head_object reported the wrong size");

    let downloaded = get_range_bytes(&provider, &bucket, &key, 0, size)
        .await
        .expect("get_range over the whole object should succeed");
    assert_eq!(
        hex(&sha256_bytes(&downloaded)),
        source_hash,
        "the round-tripped object's SHA-256 does not match the source"
    );

    provider
        .delete_objects(&bucket, std::slice::from_ref(&key))
        .await
        .expect("delete_objects should succeed");
    let err = provider
        .head_object(&bucket, &key)
        .await
        .expect_err("the object must be gone after delete_objects");
    assert_eq!(
        err.code(),
        "storage/key-not-found",
        "unexpected error: {err}"
    );

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 4: multipart / the aws-chunked sentinel ---------------------------------

/// Streamed multipart upload round trip -- and the **only** test that can
/// catch an `aws-chunked` regression.
///
/// `provider::s3::build_client` sets `RequestChecksumCalculation::WhenRequired`
/// for every non-AWS endpoint precisely so the SDK never attaches a streaming
/// checksum trailer (which would force `aws-chunked` transfer encoding). A
/// unit test on that config value proves nothing about the wire; only a real
/// streamed part upload does.
///
/// Every failure path below goes through `multipart_abort` before returning:
/// an in-progress multipart upload is invisible to `ListObjectsV2` and thus to
/// `cleanup_prefix`, and holds quota until aborted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn multipart_upload_round_trip() {
    let conn = qiniu_connection();
    let bucket = qiniu_bucket();
    let provider = from_connection(&conn).expect("a valid Qiniu connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}multipart.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("multipart.bin");
    // 6 MiB + 1 MiB: every part but the last must be >= 5 MiB, so this is the
    // smallest legal two-part upload -- enough to exercise the streamed
    // multipart path without moving more paid bytes than necessary.
    let first = 6 * MB;
    let last = MB;
    let total = first + last;
    write_pseudo_random_file(&path, total, 0x0999_0002);
    let source = std::fs::read(&path).expect("reading the multipart fixture back should succeed");
    let source_hash = hex(&sha256_bytes(&source));

    let upload_id = provider
        .multipart_init(&bucket, &key)
        .await
        .expect("multipart_init should succeed against live Qiniu");

    let outcome: Result<(), String> = async {
        let mut done: Vec<UploadedPart> = Vec::with_capacity(2);
        for (number, offset, length) in [(1i32, 0u64, first), (2i32, first, last)] {
            let etag = provider
                .upload_part_from_file(
                    &bucket,
                    &key,
                    &upload_id,
                    number,
                    &path,
                    offset,
                    length,
                    Arc::new(|_| {}),
                )
                .await
                .map_err(|e| {
                    format!(
                        "upload_part_from_file (part {number}) should succeed against live \
                         Qiniu -- a failure here most likely means the client sent \
                         `aws-chunked` transfer encoding, i.e. the non-AWS \
                         `RequestChecksumCalculation::WhenRequired` rule in \
                         provider::s3::build_client stopped applying: {e}"
                    )
                })?;
            done.push(UploadedPart {
                number,
                etag,
                size: length,
            });
        }

        // `ListParts` is what M4c's cross-restart resume treats as
        // authoritative, so it has to actually report what the server took.
        let listed = provider
            .multipart_list(&bucket, &key, &upload_id)
            .await
            .map_err(|e| format!("multipart_list should succeed before completing: {e}"))?;
        if listed.len() != done.len() {
            return Err(format!(
                "multipart_list reported {} parts, expected {}: {listed:?}",
                listed.len(),
                done.len()
            ));
        }

        provider
            .multipart_complete(&bucket, &key, &upload_id, &done)
            .await
            .map_err(|e| format!("multipart_complete should assemble the object: {e}"))?;

        let head = provider
            .head_object(&bucket, &key)
            .await
            .map_err(|e| format!("head_object after multipart_complete: {e}"))?;
        if head.size != total {
            return Err(format!(
                "the assembled object is {} bytes, expected {total}",
                head.size
            ));
        }

        // Read it back in two ranges rather than one, so a per-part offset
        // mix-up (which a whole-object read could mask if the halves happened
        // to be swapped consistently) shows up as a hash mismatch.
        let mut got = get_range_bytes(&provider, &bucket, &key, 0, first)
            .await
            .map_err(|e| format!("get_range over the first part: {e}"))?;
        got.extend(
            get_range_bytes(&provider, &bucket, &key, first, last)
                .await
                .map_err(|e| format!("get_range over the last part: {e}"))?,
        );
        if hex(&sha256_bytes(&got)) != source_hash {
            return Err("the reassembled object's SHA-256 does not match the source".to_string());
        }
        Ok(())
    }
    .await;

    if let Err(message) = outcome {
        // Abort BEFORE reporting, so a failing assertion never strands
        // server-side fragments.
        if let Err(e) = provider.multipart_abort(&bucket, &key, &upload_id).await {
            eprintln!("WARNING: multipart_abort also failed: {e} -- manual cleanup may be needed");
        }
        cleanup_prefix(&provider, &bucket, &prefix).await;
        panic!("{message}");
    }

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 5: THE batch-delete claim ----------------------------------------------

/// Multi-Object Delete works through `aws-sdk-s3` -- the one finding in this
/// integration that had never been tested with a real SDK request until this
/// suite first ran.
///
/// `supports_batch_delete` returns `true` for Qiniu, which routes
/// `Provider::delete_objects` down the `POST /?delete` batch path. Both other
/// Chinese providers BucketCat supports go the other way: Aliyun OSS and
/// Rainyun ROS hard-require the legacy `Content-MD5` header on that endpoint,
/// `aws-sdk-s3` only ever sends `x-amz-checksum-*` flexible checksums, so both
/// are forced onto the one-DeleteObject-per-key fallback.
///
/// The pre-implementation probe (2026-07-30) established that Qiniu accepts a
/// request carrying `x-amz-checksum-crc32` (200) and rejects one with no
/// checksum header at all (`400 ... Content-MD5`) -- but that probe was
/// hand-rolled curl. A real SDK request additionally carries
/// `x-amz-sdk-checksum-algorithm` and friends. **Rainyun's suite is the
/// cautionary tale**: the same assumption looked fine on paper and was
/// demolished by its first live run.
///
/// Multiple keys on purpose: `S3Provider::delete_objects` dispatches on
/// `self.batch_delete`, not on key count, so a single key would exercise the
/// same code path -- but a multi-key body is the shape a real
/// `DeleteObjects` request has, and it also proves the per-key result
/// aggregation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn batch_delete_uses_the_multi_object_path() {
    let conn = qiniu_connection();
    let bucket = qiniu_bucket();

    assert!(
        supports_batch_delete(&conn.provider),
        "this test exists to prove Multi-Object Delete works for Qiniu. \
         `supports_batch_delete(\"{}\")` returned false, so `delete_objects` would silently take \
         the one-request-per-key fallback and this test would prove nothing -- if that change \
         was deliberate (i.e. a live run DID reject the batch request), delete this test and \
         record the finding in this file's module doc comment instead",
        conn.provider
    );

    let provider = from_connection(&conn).expect("a valid Qiniu connection profile should build");
    let prefix = test_prefix();

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tiny.bin");
    let size = 512;
    write_pseudo_random_file(&path, size, 0x0999_0003);

    let keys: Vec<String> = (0..3).map(|i| format!("{prefix}batch-{i}.bin")).collect();
    for key in &keys {
        provider
            .put_object_from_file(&bucket, key, &path, size, Arc::new(|_| {}))
            .await
            .expect("put_object_from_file should succeed against live Qiniu");
    }

    let result = provider.delete_objects(&bucket, &keys).await.expect(
        "delete_objects should succeed against live Qiniu. A `400 MissingContentMD5` / \
         `InvalidRequest ... Content-MD5` here is THE failure this test was written to catch: it \
         would mean Qiniu, like Aliyun OSS and Rainyun ROS, rejects the flexible checksum \
         aws-sdk-s3 sends on Multi-Object Delete. The fix is one line -- add \"qiniu\" to the \
         exclusion in `provider::s3::supports_batch_delete` -- and this file's module doc comment \
         must be corrected to match",
    );

    assert_eq!(
        result.succeeded,
        keys.len() as u32,
        "every key should have been deleted; failures: {:?}",
        result.failed
    );
    assert!(
        result.failed.is_empty(),
        "unexpected per-key failures: {:?}",
        result.failed
    );

    for key in &keys {
        let err = provider
            .head_object(&bucket, key)
            .await
            .expect_err("the object must be gone after a batch delete");
        assert_eq!(
            err.code(),
            "storage/key-not-found",
            "unexpected error: {err}"
        );
    }

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 6: THE cross-region routing claim --------------------------------------

/// A connection configured for the **wrong** region still reaches the bucket.
///
/// This is the load-bearing test for the whole `RegionRouting` generalization,
/// and the Qiniu analogue of `oss_e2e.rs`'s same-named test. The connection's
/// endpoint host is rewritten to a region the sandbox bucket does *not* live
/// in; without routing, every object-plane call below fails with
/// `400 IncorrectRegion`.
///
/// The chain being proven, in order: `list_buckets` (global, works from any
/// host) fans out `GetBucketLocation` and caches each bucket's real region ->
/// `client_for` sees the bucket's region differs from the connection's ->
/// `qiniu_endpoint_for_region` derives the right host -> `build_client` builds
/// (and caches) a client for it -> the object plane succeeds.
///
/// `list_objects` is called *before* `list_buckets` on purpose: it exercises
/// `client_for`'s **lazy** resolution path (a cold bucket -> region cache,
/// resolved by a single `GetBucketLocation`), which is what a transfer resumed
/// after an app restart hits. The `list_buckets` call afterwards then covers
/// the eager fan-out fill.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn cross_region_bucket_is_routed_automatically() {
    let bucket = qiniu_bucket();
    let configured = qiniu_connection();
    let conn = qiniu_connection_pointed_at_another_region();
    assert_ne!(
        conn.region, configured.region,
        "the fixture must actually point at a different region than the configured one"
    );

    let provider =
        from_connection(&conn).expect("a wrong-region Qiniu connection profile should still build");

    // Cold cache: this only works if `client_for` resolved the bucket's region
    // on its own via `resolve_region`.
    provider.list_objects(&bucket, "", None, 10).await.expect(
        "list_objects must succeed even though this connection is configured for a different \
         region than the bucket lives in -- that is the entire point of \
         `S3Provider::client_for`'s region routing. A `storage/wrong-region` error here means \
         routing did not kick in: either `resolve_region`'s GetBucketLocation failed, or \
         `qiniu_endpoint_for_region` did not rewrite the host",
    );

    // The eager path: `list_buckets` is global, so it must work from the
    // "wrong" region too, and must report the bucket's real region.
    let buckets = provider
        .list_buckets()
        .await
        .expect("Qiniu's ListBuckets is account-global, so it works from any region's host");
    let found = buckets
        .iter()
        .find(|b| b.name == bucket)
        .expect("the sandbox bucket must appear in the global bucket listing");
    assert_eq!(
        found.region.as_deref(),
        Some(configured.region.as_str()),
        "the bucket's reported region should be its real one ({}), not the connection's \
         deliberately-wrong {}",
        configured.region,
        conn.region
    );

    // And a full write/read/delete round trip through the routed client, to
    // prove routing holds for the object plane and not just for listings.
    let prefix = test_prefix();
    let key = format!("{prefix}cross-region.bin");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cross-region.bin");
    let size = 4 * 1024;
    write_pseudo_random_file(&path, size, 0x0999_0004);
    let source_hash = hex(&sha256_bytes(
        &std::fs::read(&path).expect("reading the fixture back should succeed"),
    ));

    provider
        .put_object_from_file(&bucket, &key, &path, size, Arc::new(|_| {}))
        .await
        .expect("put_object_from_file must succeed through the routed client");
    let downloaded = get_range_bytes(&provider, &bucket, &key, 0, size)
        .await
        .expect("get_range must succeed through the routed client");
    assert_eq!(hex(&sha256_bytes(&downloaded)), source_hash);

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 7: presigned GET --------------------------------------------------------

/// A presigned GET URL is fetchable with no credentials -- the Share feature's
/// foundation, and the proof that Qiniu accepts SigV4 **query-string**
/// signing, not just header signing.
///
/// The URL is never printed or passed to a subprocess; see
/// [`fetch_anonymously`].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn presigned_get_works() {
    let conn = qiniu_connection();
    let bucket = qiniu_bucket();
    let provider = from_connection(&conn).expect("a valid Qiniu connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}shared.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("shared.bin");
    let size = 8 * 1024;
    write_pseudo_random_file(&path, size, 0x0999_0005);
    let source = std::fs::read(&path).expect("reading the fixture back should succeed");

    provider
        .put_object_from_file(&bucket, &key, &path, size, Arc::new(|_| {}))
        .await
        .expect("put_object_from_file should succeed against live Qiniu");

    let url = provider
        .presign_get(&bucket, &key, 300)
        .await
        .expect("presign_get should produce a URL");

    let outcome = fetch_anonymously(&url).await;
    // Cleanup first: the assertions below must not be able to strand objects.
    cleanup_prefix(&provider, &bucket, &prefix).await;

    let (status, body) = outcome.expect("the anonymous fetch should have completed");
    assert_eq!(
        status, 200,
        "the presigned URL should be fetchable without credentials; got HTTP {status}. A 403 \
         means Qiniu rejected the SigV4 query-string signature"
    );
    assert_eq!(
        hex(&sha256_bytes(&body)),
        hex(&sha256_bytes(&source)),
        "the anonymously fetched body does not match the uploaded object"
    );
}

// --- 8: rename (copy-then-delete) -------------------------------------------

/// `rename_object` works: object stores have no native rename, so this is
/// `CopyObject` followed by `DeleteObject`, and `x-amz-copy-source` has to be
/// percent-encoded by hand (the SDK does not do it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn rename_object_moves_the_object() {
    let conn = qiniu_connection();
    let bucket = qiniu_bucket();
    let provider = from_connection(&conn).expect("a valid Qiniu connection profile should build");
    let prefix = test_prefix();
    // A space and a non-ASCII segment on purpose: those are exactly what an
    // unencoded `x-amz-copy-source` breaks on.
    let from_key = format!("{prefix}原始 文件.bin");
    let to_key = format!("{prefix}renamed 文件.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("rename.bin");
    let size = 2 * 1024;
    write_pseudo_random_file(&path, size, 0x0999_0006);

    provider
        .put_object_from_file(&bucket, &from_key, &path, size, Arc::new(|_| {}))
        .await
        .expect("put_object_from_file should succeed against live Qiniu");

    provider
        .rename_object(&bucket, &from_key, &to_key)
        .await
        .expect(
            "rename_object should succeed -- a signature failure here usually means \
             `x-amz-copy-source` was not percent-encoded",
        );

    provider
        .head_object(&bucket, &to_key)
        .await
        .expect("the destination key must exist after the rename");
    let err = provider
        .head_object(&bucket, &from_key)
        .await
        .expect_err("the source key must be gone after the rename");
    assert_eq!(
        err.code(),
        "storage/key-not-found",
        "unexpected error: {err}"
    );

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 9: recursive prefix delete ---------------------------------------------

/// `delete_prefix` removes a whole subtree, **including** the zero-byte
/// folder-marker object.
///
/// The marker is the interesting part: `list_objects` (delimiter `/`) rolls
/// `sub/` into a CommonPrefix and filters it out, so only the delimiter-less
/// `list_objects_flat` walk surfaces it as a real key. Verified live that
/// Qiniu has S3-identical semantics for both shapes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn delete_prefix_removes_every_object_under_it() {
    let conn = qiniu_connection();
    let bucket = qiniu_bucket();
    let provider = from_connection(&conn).expect("a valid Qiniu connection profile should build");
    let prefix = test_prefix();

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("leaf.bin");
    let size = 256;
    write_pseudo_random_file(&path, size, 0x0999_0007);

    provider
        .create_folder(&bucket, &format!("{prefix}sub"))
        .await
        .expect("create_folder should write a zero-byte marker");
    for key in [format!("{prefix}a.bin"), format!("{prefix}sub/b.bin")] {
        provider
            .put_object_from_file(&bucket, &key, &path, size, Arc::new(|_| {}))
            .await
            .expect("put_object_from_file should succeed against live Qiniu");
    }

    let flat = provider
        .list_objects_flat(&bucket, &prefix, None, 100)
        .await
        .expect("list_objects_flat should walk the whole subtree");
    assert!(
        flat.entries
            .iter()
            .any(|e| e.key == format!("{prefix}sub/")),
        "list_objects_flat must surface the zero-byte folder marker as a real key; saw: {:?}",
        flat.entries.iter().map(|e| &e.key).collect::<Vec<_>>()
    );

    let result = provider
        .delete_prefix(&bucket, &prefix)
        .await
        .expect("delete_prefix should succeed");
    assert!(
        result.failed.is_empty(),
        "unexpected per-key failures: {:?}",
        result.failed
    );

    let after = provider
        .list_objects_flat(&bucket, &prefix, None, 100)
        .await
        .expect("list_objects_flat should succeed after the delete");
    assert!(
        after.entries.is_empty(),
        "the prefix should be empty after delete_prefix; saw: {:?}",
        after.entries.iter().map(|e| &e.key).collect::<Vec<_>>()
    );
}
