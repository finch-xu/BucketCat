//! Rainyun ROS end-to-end integration tests (opt-in, LIVE).
//!
//! **These tests hit a real Rainyun ROS endpoint.** Like `oss_e2e.rs` (and
//! unlike `minio_e2e.rs`, which drives a disposable local container), the
//! target account is a real, paid one whose buckets may hold data that
//! matters. Every test in this file is `#[ignore]`d, so a plain `cargo test`
//! never touches the network, never reads a single one of the environment
//! variables below, and never sees a credential -- this suite is opt-in, run
//! explicitly, never part of the hermetic default suite.
//!
//! ## What this suite proves about Rainyun specifically
//!
//! Rainyun's S3 endpoint ("雨云对象存储 ROS") is a MinIO cluster behind an
//! APISIX gateway. The original design premise of this integration was zero
//! backend change, and four facts -- all verified live on 2026-07-25 by
//! probing the real endpoint -- confirm Rainyun needs none of them to be
//! addressed, listed, uploaded to, or presigned against:
//!
//! Batch delete is the one exception: this suite's first live run (also
//! 2026-07-25, same probing session) is what caught it, and `provider::s3`
//! now has exactly one line of Rainyun-specific code as a result -- see
//! "The one actual special case" below, after these four.
//!
//! - **Path-style addressing is mandatory.** `bucket.cn-nb1.rains3.com` gets
//!   `404 Route Not Found` from the gateway -- APISIX has no wildcard
//!   bucket-subdomain route, so virtual-hosted addressing never even reaches
//!   MinIO. `uses_path_style` already returns `true` for any non-AWS,
//!   non-OSS provider, so Rainyun is correct by default; the point of
//!   testing it is to catch a *future* change that adds Rainyun to the
//!   virtual-hosted branch alongside AWS and OSS.
//!   [`list_objects_on_the_test_bucket_succeeds`] is that sentinel.
//! - **SigV4 only.** SigV2 is rejected. `aws-sdk-s3` signs SigV4
//!   unconditionally, so nothing here has to opt in -- but every test below
//!   would fail loudly if that ever changed.
//! - **The region is never validated.** Any credential-scope region reaches
//!   the same `InvalidAccessKeyId` stage, and `GetBucketLocation` reports an
//!   empty `LocationConstraint` (i.e. us-east-1). The app stores the Rainyun
//!   region id (`cn-nb1` / `cn-sy1`) anyway, purely so the UI reads sensibly;
//!   that value never has to match anything server-side. Consequently there
//!   is deliberately **no** cross-region routing test here (the OSS suite's
//!   `cross_region_bucket_is_routed_automatically` has no Rainyun analogue):
//!   there is no cross-region behavior to route around, only the separate
//!   clusters described under "Running these tests" below.
//! - **`aws-chunked` must not be sent.** Covered by the pre-existing,
//!   non-Rainyun-specific rule in `provider::s3::build_client` that any
//!   non-AWS endpoint gets `RequestChecksumCalculation::WhenRequired`, so
//!   the SDK never attaches a streaming checksum trailer (which would force
//!   `aws-chunked` transfer encoding). [`multipart_upload_round_trip`] is
//!   the **only** test that can catch a regression there: a unit test on a
//!   config value cannot prove wire behavior, and it is the streamed,
//!   multi-part upload path that actually trips over it.
//!
//! ## The one actual special case: batch delete
//!
//! `provider::s3::supports_batch_delete` returns `false` for `"rainyun"`
//! (case-insensitively), the **only** place `provider::s3` branches on the
//! Rainyun provider string at all. Every other behavior above rides the
//! generic non-AWS/non-OSS path unchanged.
//!
//! This is not something static analysis found -- it's exactly what this
//! suite's first live run against a real endpoint exposed, on 2026-07-25.
//! Before that fix, `supports_batch_delete` returned `true` for Rainyun (it
//! only special-cased OSS), so every call to `Provider::delete_objects`
//! here -- single-key or multi-key alike, since the dispatch in
//! `S3Provider::delete_objects` doesn't look at key count -- went out as a
//! real Multi-Object Delete (`POST /?delete`) body. Rainyun's MinIO rejects
//! that request with `400 MissingContentMD5` unless it carries a legacy
//! `Content-MD5` header; `aws-sdk-s3` only ever sends `x-amz-checksum-*`
//! flexible checksums and has no way to send `Content-MD5`. The live
//! symptom was every delete call failing with
//! `BatchResult { succeeded: 0, failed: [FailedKey { code: "internal" }] }`
//! -- **four of this file's six tests** (every one that deletes anything:
//! [`small_object_round_trip`], [`multipart_upload_round_trip`],
//! [`presigned_get_works`], and [`delete_prefix_removes_every_object_under_it`])
//! failed that way on that first run.
//!
//! The fix folds `"rainyun"` into the same `supports_batch_delete` check
//! Aliyun OSS already used for the identical reason, so `delete_objects`
//! now falls back to one `DeleteObject` per key for Rainyun too -- see the
//! doc comment on `supports_batch_delete` in `provider::s3` for the full
//! three-way probe (`Content-MD5` present / absent / `x-amz-checksum-crc32`
//! present) that confirmed the root cause. Every delete call in this file
//! now exercises that one-by-one fallback rather than the batch endpoint,
//! which Rainyun simply cannot use via `aws-sdk-s3`.
//!
//! ## ⚠️ Hard safety rules this file obeys
//!
//! - **No bucket lifecycle calls, ever.** This file must never call
//!   `create_bucket` or `delete_bucket`, not once. It operates against
//!   exactly one pre-existing, dedicated sandbox bucket that a human already
//!   created and owns. (Rainyun rejects `CreateBucket` with `AccessDenied`
//!   regardless -- buckets are created in its console and bound to a plan's
//!   quota, not over the S3 API -- but that is corroboration, not the
//!   reason: the rule stands on its own and would still stand if Rainyun
//!   allowed it tomorrow.)
//! - **No hardcoded bucket name.** The target bucket comes *only* from
//!   `BUCKETCAT_RAINYUN_BUCKET`, which has no default. This repository is
//!   public; a real bucket name must never appear in it.
//! - **A single, unique, throwaway key prefix per test.** Every test that
//!   writes data calls [`test_prefix`] for its own `bucketcat-e2e/{uuid}/`
//!   prefix, so concurrent (and repeated) runs never collide and cleanup can
//!   never reach outside that prefix.
//! - **Cleanup is prefix-scoped and self-checking.** [`cleanup_prefix`]
//!   refuses (via `assert!`) to run against anything that isn't a real,
//!   non-empty `bucketcat-e2e/` prefix, so a coding mistake that produced an
//!   empty or mistyped prefix can never cascade into deleting unrelated
//!   objects. Its reach is real but not total, though: `delete_prefix`
//!   enumerates via `ListObjectsV2`, which never lists an **in-progress**
//!   multipart upload -- so `cleanup_prefix` alone can never reach or abort
//!   one. [`multipart_upload_round_trip`] is the one test that can leave one
//!   behind (any failure between `multipart_init` and `multipart_complete`),
//!   and it guards that window itself: every failure path in it explicitly
//!   calls `multipart_abort` before `cleanup_prefix` runs. The two together
//!   -- not `cleanup_prefix` alone -- are what keep this file's footprint
//!   genuinely bounded.
//! - **Credentials never touch disk or a log line.** They are read from
//!   environment variables at test start and handed straight to the SDK;
//!   nothing here prints, formats, or persists them. The presigned URL in
//!   [`presigned_get_works`] carries a live signature in its query string
//!   and is treated the same way: it is never printed, never passed as a
//!   subprocess argument (where it would be visible in `ps`), and never
//!   interpolated into an assertion message. That is why that test fetches
//!   with the in-process `hyper` + `hyper-rustls` client already in this
//!   crate's dependency graph rather than shelling out to `curl` the way
//!   `oss_e2e.rs` and `minio_e2e.rs` do.
//!
//! ## Running these tests
//!
//! You need a **dedicated, disposable Rainyun bucket** you are comfortable
//! having a `bucketcat-e2e/` prefix written to and deleted from repeatedly.
//! Do **not** point this at a bucket holding anything you care about --
//! nothing here creates or deletes the bucket itself, but writes and deletes
//! happen for real under that prefix.
//!
//! Note that **each Rainyun region is a SEPARATE cluster with its OWN
//! credentials**: an access key issued in `cn-nb1` returns
//! `InvalidAccessKeyId` against `cn-sy1` (verified live). The endpoint, the
//! bucket and the key/secret below must therefore all belong to the *same*
//! region -- there is no account-level, cross-region view to fall back on.
//!
//! ```bash
//! export BUCKETCAT_RAINYUN_ENDPOINT="https://cn-nb1.rains3.com"
//! export BUCKETCAT_RAINYUN_REGION="cn-nb1"
//! export BUCKETCAT_RAINYUN_BUCKET="your-disposable-sandbox-bucket"
//! export BUCKETCAT_RAINYUN_AK="..."
//! export BUCKETCAT_RAINYUN_SK="..."
//! cargo test --test rainyun_e2e -- --ignored
//! ```
//!
//! Any variable left unset makes the test `panic!` immediately with a
//! message naming the missing variable, rather than silently defaulting to
//! anything -- there is no safe default for a real cloud account.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use sha2::{Digest, Sha256};
use std::path::Path;

use bucketcat_lib::provider::s3::uses_path_style;
use bucketcat_lib::provider::{from_connection, Provider, S3Provider, UploadedPart};
use bucketcat_lib::store::Connection;

/// 1 MiB, the unit the multipart fixture is sized in.
const MB: u64 = 1024 * 1024;

// --- env / connection helpers ------------------------------------------------

/// Reads a required environment variable, panicking with a clear pointer to
/// this file's module doc comment when it's missing -- there is no safe
/// default for any of these against a real cloud account.
fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!(
            "missing required environment variable `{name}` -- see rainyun_e2e.rs's module doc \
             comment for the full list of required variables and how to run this suite"
        )
    })
}

/// The dedicated sandbox bucket these tests operate against. **No default,
/// ever** -- see this file's module doc comment and the safety rules it
/// documents. Nothing in this file creates or deletes it.
fn rainyun_bucket() -> String {
    required_env("BUCKETCAT_RAINYUN_BUCKET")
}

/// A `Connection` pointed at the live Rainyun endpoint from the environment.
///
/// `provider` is `"rainyun"`, which `provider::s3` special-cases in exactly
/// one place: `supports_batch_delete` returns `false` for it, the same
/// fallback Aliyun OSS uses, because Rainyun's MinIO also mandates a legacy
/// `Content-MD5` header on Multi-Object Delete that `aws-sdk-s3` cannot
/// send (confirmed live 2026-07-25; see this file's module doc comment,
/// "The one actual special case"). Everywhere else it falls into the
/// generic path: `uses_path_style`'s "everything else" branch (path-style,
/// which the APISIX gateway requires), `s3_compat_endpoint`'s non-OSS
/// passthrough (the endpoint is used verbatim), and the non-AWS
/// `RequestChecksumCalculation::WhenRequired` branch (no `aws-chunked`).
///
/// `region` is whatever `BUCKETCAT_RAINYUN_REGION` says; Rainyun never
/// validates it (fact 3 in the module doc), but the app stores the real
/// region id so the UI reads sensibly, and so does this fixture.
fn rainyun_connection() -> Connection {
    Connection {
        id: "e2e-rainyun".to_string(),
        provider: "rainyun".to_string(),
        name: "e2e rainyun".to_string(),
        endpoint: required_env("BUCKETCAT_RAINYUN_ENDPOINT"),
        region: required_env("BUCKETCAT_RAINYUN_REGION"),
        access_key_id: required_env("BUCKETCAT_RAINYUN_AK"),
        secret_access_key: required_env("BUCKETCAT_RAINYUN_SK"),
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
             manual cleanup of this prefix in the Rainyun console may be needed"
        );
    }
}

// --- fixture / hashing helpers (mirrors oss_e2e.rs) --------------------------

/// Writes `size` bytes of deterministic LCG pseudo-random content to `path`.
///
/// Seeded (so the source hash is reproducible run to run) and, crucially,
/// non-compressible -- a file of repeated bytes would let a per-part offset
/// bug go unnoticed because every offset reads the same value. Knuth's MMIX
/// LCG constants give that without dragging a `rand` dev-dependency into the
/// test build. Seeds here live in their own `0x0777_` namespace, distinct
/// from `oss_e2e.rs`'s `0x0555_` and `minio_e2e.rs`'s `0x5EED_`.
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

// --- anonymous HTTP fetch (for the presigned-URL test) ----------------------

/// Fetches `url` with **no credentials of any kind** and returns
/// `(status, body)`.
///
/// Built on the `hyper` + `hyper-rustls` client already in this crate's
/// dependency graph (see `provider::oss_admin`, which builds the same one
/// for OSS's native ListBuckets) rather than shelling out to `curl` the way
/// `oss_e2e.rs` and `minio_e2e.rs` do. That is a deliberate safety choice,
/// not a style preference: the only caller passes a **presigned URL whose
/// query string carries a live signature**, and a subprocess argument is
/// visible to every other process on the machine via `ps`. Keeping the
/// request in-process means the URL never leaves this address space.
///
/// The `url` itself is never included in any error string this returns --
/// `http::uri::InvalidUri` renders only the kind of syntax problem, and
/// `hyper_util::client::legacy::Error`'s `Display` renders only its kind
/// plus cause (a connect/IO/protocol error), never the request URI.
///
/// The TLS crypto provider is passed explicitly (`with_provider_and_native_roots`)
/// rather than relying on a process-wide default, so this never depends on
/// -- or races -- `aws-sdk-s3`'s own transport having installed one first.
/// `https_or_http` rather than `https_only` so a plaintext endpoint (a local
/// stand-in, or a future Rainyun HTTP endpoint) is fetchable too; the real
/// Rainyun endpoint is HTTPS.
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

// --- 1: account-level ListBuckets -------------------------------------------

/// Account-level `ListBuckets` works against Rainyun, unlike Aliyun OSS.
///
/// This is worth its own test because OSS forced BucketCat to build a whole
/// second transport for it (`provider::oss_admin`, an OSS4-HMAC-SHA256-signed
/// native call) -- `S3Provider::list_buckets` still branches on that. Rainyun
/// is a plain MinIO cluster: the S3-compatible `ListBuckets` is the real
/// thing, `oss_native` is `None` for this connection, and the sidebar's
/// bucket list therefore needs no Rainyun-specific path at all. A failure
/// here would mean the app cannot even populate a Rainyun connection's
/// bucket list, so it is checked first.
///
/// It also asserts the configured sandbox bucket is *in* that listing, which
/// makes this the cheapest diagnostic for the single most likely
/// misconfiguration of this suite: each Rainyun region is a separate cluster
/// with its own credentials (see the module doc), so an endpoint, bucket and
/// key that don't all belong to the same region produce either an auth
/// failure here or a listing the bucket simply isn't in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn list_buckets_succeeds() {
    let conn = rainyun_connection();
    let bucket = rainyun_bucket();
    let provider = from_connection(&conn).expect("a valid Rainyun connection profile should build");

    let buckets = provider.list_buckets().await.expect(
        "list_buckets against the live Rainyun endpoint should succeed -- unlike Aliyun OSS, \
         Rainyun answers the S3-compatible account-level ListBuckets directly, so a failure \
         here means the endpoint, the SigV4 credentials, or path-style addressing is wrong; \
         see this file's module doc comment",
    );

    assert!(
        buckets.iter().any(|b| b.name == bucket),
        "the configured sandbox bucket `{bucket}` must appear in its own account's bucket \
         listing -- if it doesn't, the most likely cause is that BUCKETCAT_RAINYUN_ENDPOINT / \
         _BUCKET / _AK / _SK do not all belong to the SAME Rainyun region (each region is a \
         separate cluster with its own credentials; see this file's module doc comment). Saw: \
         {buckets:?}"
    );
}

// --- 2: read-only smoke test / the path-style regression sentinel -----------

/// The cheapest possible proof that path-style addressing and SigV4 are
/// simultaneously correct: a bare `ListObjectsV2` against the sandbox
/// bucket's root.
///
/// **This is the path-style regression sentinel** named in the module doc.
/// Rainyun's APISIX gateway has no wildcard bucket-subdomain route, so a
/// virtual-hosted request to `bucket.cn-nb1.rains3.com` gets `404 Route Not
/// Found` -- it never reaches MinIO at all, and the failure looks nothing
/// like a normal S3 error. `uses_path_style` returns `true` for Rainyun today
/// only because it returns `true` for every non-AWS, non-OSS provider; the
/// risk this test exists to cover is a future change adding Rainyun to the
/// virtual-hosted branch (as OSS already is) without anyone realizing the
/// gateway cannot serve that shape.
///
/// The pure `uses_path_style` assertion runs first so that regression is
/// named explicitly and fails instantly, before the network call turns it
/// into a confusing 404; the live call is what proves the *rest* of the
/// chain (SigV4, endpoint, credentials) on top of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn list_objects_on_the_test_bucket_succeeds() {
    let conn = rainyun_connection();
    let bucket = rainyun_bucket();

    assert!(
        uses_path_style(&conn.provider, &conn.endpoint),
        "Rainyun MUST use path-style addressing: its APISIX gateway has no wildcard \
         bucket-subdomain route and answers `bucket.{{region}}.rains3.com` with `404 Route Not \
         Found`. `uses_path_style(\"{}\", ..)` returned false, which means someone added \
         Rainyun to the virtual-hosted branch alongside AWS/OSS -- see this file's module doc \
         comment",
        conn.provider
    );

    let provider = from_connection(&conn).expect("a valid Rainyun connection profile should build");

    provider.list_objects(&bucket, "", None, 10).await.expect(
        "list_objects against the live Rainyun sandbox bucket should succeed -- a failure here \
         means path-style addressing, the endpoint, the region-scoped credentials, or SigV4 \
         signing is wrong; see this file's module doc comment",
    );
}

// --- 3: small object round trip ----------------------------------------------

/// Small (tens-of-KB) object round trip: put -> head (size) -> get_range
/// (whole object) -> SHA-256 match -> delete -> proof it's actually gone ->
/// prefix cleanup. The baseline data-plane proof, independent of the
/// multipart / `aws-chunked` concern the next test targets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn small_object_round_trip() {
    let conn = rainyun_connection();
    let bucket = rainyun_bucket();
    let provider = from_connection(&conn).expect("a valid Rainyun connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}small.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("small.bin");
    let size = 48 * 1024; // a few tens of KB
    write_pseudo_random_file(&path, size, 0x0777_0001);

    provider
        .put_object_from_file(&bucket, &key, &path, size)
        .await
        .expect("put_object_from_file should succeed against live Rainyun");

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

    let deleted = provider
        .delete_objects(&bucket, std::slice::from_ref(&key))
        .await
        .expect("delete_objects should succeed against live Rainyun");
    assert_eq!(
        (deleted.succeeded, deleted.failed.len()),
        (1, 0),
        "delete_objects must report exactly one success and no per-key failures, got: {deleted:?}"
    );

    // Asserting the call's return value is NOT enough. `delete_objects`
    // counts a key as deleted whenever the batch response reports no error
    // for it, so a backend that accepts the request and quietly drops it
    // still looks like success -- exactly the shape of a regression that
    // passed against live OSS while every "deleted" object survived. Assert
    // the object is actually gone.
    let err = provider.head_object(&bucket, &key).await.expect_err(
        "head_object must fail after delete_objects reported success -- if it still succeeds, \
         the delete was acknowledged but did not actually remove the object",
    );
    // `expect_err` alone accepts ANY error -- a transient 500, DNS hiccup, or
    // mid-test credential failure would pass it just as well as a real
    // "gone" response, and be indistinguishable from a successful delete.
    // Assert the specific not-found code instead.
    assert_eq!(
        err.code(),
        "storage/key-not-found",
        "head_object failed after delete, as expected, but with code `{}` instead of the \
         expected `storage/key-not-found` -- this could be a transient error unrelated to the \
         delete rather than proof the object is actually gone: {err}",
        err.code()
    );
    eprintln!("post-delete head_object failed as expected for `{key}`: {err}");

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 4: multipart round trip (the aws-chunked risk) --------------------------

/// The most important test in this file: a 7 MiB multipart upload split into
/// a 6 MiB part and a 1 MiB part (only the *last* part may be under S3's
/// 5 MiB minimum, so this is the smallest legal two-part shape).
///
/// **This is the only test in this repository that can catch an
/// `aws-chunked` regression for Rainyun.** `provider::s3::build_client` sets
/// `RequestChecksumCalculation::WhenRequired` for every non-AWS endpoint
/// precisely so the SDK never attaches a streaming checksum trailer, which
/// would force `aws-chunked` transfer encoding. A unit test can assert that
/// config value, but it cannot prove what goes on the wire -- and it is the
/// streamed multipart upload path, not `PutObject`, that actually trips over
/// it. If that config ever regresses to the SDK's `WhenSupported` default,
/// the symptom is `upload_part_from_file` failing, which is why that
/// failure's message calls the possibility out by name.
///
/// A green run proves the whole chain against real Rainyun: init -> 2 parts
/// -> `multipart_list` sees both -> complete -> head (size) -> a `get_range`
/// per part boundary, each hashed against the corresponding slice of the
/// source (so a part landing at the wrong offset is caught *at that
/// offset*, not just as a whole-object mismatch) -> the concatenation hashed
/// against the whole source -> delete -> proof it's gone -> prefix cleanup.
///
/// Everything between `multipart_init` and the end of the test runs inside a
/// `Result`-returning block rather than using `expect`/`assert_eq!` directly.
/// That is not stylistic: an in-progress multipart upload is invisible to
/// `ListObjectsV2` (so `delete_prefix`/`cleanup_prefix` can never reach or
/// abort one) and its already-uploaded parts keep consuming the bucket's
/// paid quota until explicitly aborted. A mid-test `panic!` unwinding
/// straight past `multipart_abort` would leave exactly that: a
/// console-invisible upload with no cleanup path except the human running
/// this suite noticing and aborting it by hand. Every check below keeps its
/// full strength -- only the failure *plumbing* changed, from `panic!` to
/// `Err(String)`, so it can be caught and turned into an abort first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn multipart_upload_round_trip() {
    let conn = rainyun_connection();
    let bucket = rainyun_bucket();
    let provider = from_connection(&conn).expect("a valid Rainyun connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}multipart.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("multipart.bin");
    // 6 MiB + 1 MiB: every part but the last must be >= 5 MiB, so this is
    // the smallest legal two-part upload -- enough to exercise the streamed
    // multipart path without moving more paid bytes than necessary.
    let first = 6 * MB;
    let last = MB;
    let total = first + last;
    write_pseudo_random_file(&path, total, 0x0777_0002);
    // Read the source once, up front, so the per-part hash comparison inside
    // the abort-guarded block below needs no fallible file IO of its own.
    let source = std::fs::read(&path).expect("reading the multipart fixture back should succeed");
    let source_hash = hex(&sha256_bytes(&source));

    let upload_id = provider
        .multipart_init(&bucket, &key)
        .await
        .expect("multipart_init should succeed against live Rainyun");

    // From here on, ANY failure -- an `Err` return or a failed check -- must
    // go through `multipart_abort` before this test exits. See the doc
    // comment above for why: an in-progress multipart upload is invisible to
    // `delete_prefix` and holds quota until aborted.
    let outcome: Result<(), String> = async {
        let mut done: Vec<UploadedPart> = Vec::with_capacity(2);
        for (number, offset, length) in [(1i32, 0u64, first), (2i32, first, last)] {
            let etag = provider
                .upload_part_from_file(&bucket, &key, &upload_id, number, &path, offset, length)
                .await
                .map_err(|e| {
                    format!(
                        "upload_part_from_file (part {number}) should succeed against live \
                         Rainyun -- a failure here most likely means the client sent \
                         `aws-chunked` transfer encoding, which Rainyun's MinIO-behind-APISIX \
                         endpoint does not accept; that would mean the non-AWS \
                         `RequestChecksumCalculation::WhenRequired` rule in \
                         `provider::s3::build_client` regressed (see this file's and \
                         provider::s3's module doc comments): {e}"
                    )
                })?;
            done.push(UploadedPart {
                number,
                etag,
                size: length,
            });
        }

        let listed = provider
            .multipart_list(&bucket, &key, &upload_id)
            .await
            .map_err(|e| format!("multipart_list should succeed against live Rainyun: {e}"))?;
        if listed.len() != 2 {
            return Err(format!(
                "the server must report exactly the 2 parts uploaded, got: {listed:?}"
            ));
        }

        provider
            .multipart_complete(&bucket, &key, &upload_id, &done)
            .await
            .map_err(|e| format!("multipart_complete should succeed against live Rainyun: {e}"))?;

        let head = provider
            .head_object(&bucket, &key)
            .await
            .map_err(|e| format!("head_object should succeed after multipart_complete: {e}"))?;
        if head.size != total {
            return Err(format!(
                "the completed object's size must equal the source file's total size: got {}, \
                 want {total}",
                head.size
            ));
        }

        // Read the object back one part-range at a time and hash each range
        // against the matching slice of the source. A whole-object hash alone
        // would say "something is wrong"; this says *which* part landed at the
        // wrong offset -- the exact failure mode a multipart upload has and a
        // single-stream one doesn't.
        let mut reassembled: Vec<u8> = Vec::with_capacity(total as usize);
        for (number, offset, length) in [(1i32, 0u64, first), (2i32, first, last)] {
            let segment = provider
                .get_range(&bucket, &key, offset, length)
                .await
                .map_err(|e| {
                    format!("get_range for part {number}'s byte range should succeed: {e}")
                })?;
            if segment.len() as u64 != length {
                return Err(format!(
                    "get_range for part {number} must return exactly {length} bytes, got {}",
                    segment.len()
                ));
            }
            let want = &source[offset as usize..(offset + length) as usize];
            let (got_hash, want_hash) = (hex(&sha256_bytes(&segment)), hex(&sha256_bytes(want)));
            if got_hash != want_hash {
                return Err(format!(
                    "the bytes at part {number}'s range (offset {offset}, length {length}) must \
                     match the source file's same range -- a mismatch means that part landed at \
                     the wrong offset: got {got_hash}, want {want_hash}"
                ));
            }
            reassembled.extend_from_slice(&segment);
        }

        let reassembled_hash = hex(&sha256_bytes(&reassembled));
        if reassembled_hash != source_hash {
            return Err(format!(
                "the concatenated part ranges must hash to the whole source file's SHA-256: got \
                 {reassembled_hash}, want {source_hash}"
            ));
        }

        let deleted = provider
            .delete_objects(&bucket, std::slice::from_ref(&key))
            .await
            .map_err(|e| format!("delete_objects should succeed against live Rainyun: {e}"))?;
        if (deleted.succeeded, deleted.failed.len()) != (1, 0) {
            return Err(format!(
                "delete_objects must report exactly one success and no per-key failures, got: \
                 {deleted:?}"
            ));
        }

        // Asserting the call's return value is NOT enough -- see
        // `small_object_round_trip`'s post-delete assertion for why. And a
        // bare `expect_err` isn't enough either: it accepts ANY error (a
        // transient 500, DNS hiccup, mid-test credential failure), so pin the
        // specific not-found code.
        match provider.head_object(&bucket, &key).await {
            Ok(_) => Err(format!(
                "head_object must fail after delete_objects reported success for `{key}` -- if \
                 it still succeeds, the delete was acknowledged but did not actually remove the \
                 object"
            )),
            Err(e) if e.code() != "storage/key-not-found" => Err(format!(
                "head_object failed after delete, as expected, but with code `{}` instead of the \
                 expected `storage/key-not-found`: {e}",
                e.code()
            )),
            Err(e) => {
                eprintln!("post-delete head_object failed as expected for `{key}`: {e}");
                Ok(())
            }
        }
    }
    .await;

    if let Err(msg) = outcome {
        // Best-effort: if the abort itself also fails, there is nothing more
        // this test can do about it beyond what `cleanup_prefix`'s own
        // WARNING would already surface -- and an abort failure here must not
        // mask the real assertion failure in `msg` below.
        let _ = provider.multipart_abort(&bucket, &key, &upload_id).await;
        cleanup_prefix(&provider, &bucket, &prefix).await;
        panic!("{msg}");
    }
    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 5: presigned GET --------------------------------------------------------

/// Does Rainyun honor presigned GET URLs the way `aws-sdk-s3` signs them?
/// This matters directly for BucketCat's Share and preview features, and it
/// is not a given: the URL has to survive Rainyun's APISIX gateway, which
/// forwards the whole query string (signature included) to MinIO, and it is
/// path-style, so the bucket is part of the path the gateway routes on.
///
/// Upload a small object, `presign_get` it, then fetch the URL carrying **no
/// credentials at all** -- the entire point of a presigned URL is that the
/// signature embedded in the query string is sufficient on its own. A
/// SHA-256 match against the source proves not just that the request
/// succeeded, but that it returned the exact intended bytes.
///
/// The URL carries a live signature and is never printed, never passed as a
/// subprocess argument, and never interpolated into an assertion message --
/// see [`fetch_anonymously`], which exists for exactly that reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn presigned_get_works() {
    let conn = rainyun_connection();
    let bucket = rainyun_bucket();
    let provider = from_connection(&conn).expect("a valid Rainyun connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}share.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("share.bin");
    let size = 32 * 1024;
    write_pseudo_random_file(&path, size, 0x0777_0003);

    provider
        .put_object_from_file(&bucket, &key, &path, size)
        .await
        .expect("put_object_from_file should succeed against live Rainyun");

    let url = provider
        .presign_get(&bucket, &key, 3600)
        .await
        .expect("presign_get should succeed against live Rainyun");

    let fetched = fetch_anonymously(&url).await;
    // The URL is out of scope from here on: nothing below may reference it.
    drop(url);

    let (status, body) = fetched.unwrap_or_else(|e| {
        panic!(
            "the presigned GET URL for `{key}` must be fetchable with no credentials at all: \
             {e} -- if this is a transport failure the conclusion is inconclusive, but if it is \
             a signature/route rejection it means Rainyun does NOT support presigned GET URLs \
             the way `aws-sdk-s3` signs them, which would block the Share and preview features \
             for Rainyun connections and needs a design-doc follow-up"
        )
    });
    assert_eq!(
        status, 200,
        "the unauthenticated presigned GET for `{key}` must return 200; a 403 means Rainyun \
         rejected the SigV4 query-string signature, and a 404 means its APISIX gateway did not \
         route the path-style presigned URL to the bucket at all -- either would block the \
         Share and preview features for Rainyun connections"
    );
    assert_eq!(
        hex(&sha256_bytes(&body)),
        hex(&sha256_file(&path)),
        "the fetched bytes must match the source file's SHA-256 -- if this fails while the \
         request returned 200, Rainyun accepted the presigned URL but served different content \
         than intended"
    );

    let deleted = provider
        .delete_objects(&bucket, std::slice::from_ref(&key))
        .await
        .expect("delete_objects should succeed against live Rainyun");
    assert_eq!(
        (deleted.succeeded, deleted.failed.len()),
        (1, 0),
        "delete_objects must report exactly one success and no per-key failures, got: {deleted:?}"
    );

    // Asserting the call's return value is NOT enough, and a bare
    // `expect_err` accepts any error at all -- see `small_object_round_trip`'s
    // post-delete assertion for the full reasoning.
    let err = provider.head_object(&bucket, &key).await.expect_err(
        "head_object must fail after delete_objects reported success -- if it still succeeds, \
         the delete was acknowledged but did not actually remove the object",
    );
    assert_eq!(
        err.code(),
        "storage/key-not-found",
        "head_object failed after delete, as expected, but with code `{}` instead of the \
         expected `storage/key-not-found` -- this could be a transient error unrelated to the \
         delete rather than proof the object is actually gone: {err}",
        err.code()
    );
    eprintln!("post-delete head_object failed as expected for `{key}`: {err}");

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 6: delete_prefix removes a multi-object nested tree ---------------------

/// Proves `delete_prefix` actually deletes every object under a prefix on
/// Rainyun -- not just that its return value claims to.
///
/// Every other writing test in this file uploads exactly **one** object and
/// deletes it individually via `delete_objects` *before* calling
/// `cleanup_prefix`, so `cleanup_prefix`'s own `delete_prefix` call always
/// runs against an already-empty prefix: 0 keys listed, 0 chunks, no
/// multi-key delete request ever issued. `delete_prefix`'s walk-then-delete
/// logic would otherwise never be exercised with a >1-key fan-out at all.
///
/// This test is also, historically, one of the four that caught the real
/// Rainyun batch-delete bug on this suite's first live run (2026-07-25; see
/// the module doc comment, "The one actual special case"): back then
/// `supports_batch_delete` still returned `true` for Rainyun, so this
/// four-key `delete_prefix` call went out as one real `DeleteObjects`
/// (`POST /?delete`) body -- which Rainyun's MinIO rejected outright
/// (`400 MissingContentMD5`). Now that `supports_batch_delete` returns
/// `false` for Rainyun, this same call instead exercises the one-by-one
/// `DeleteObject` fallback -- four individual requests, not one batch
/// request -- so what this test verifies today is `delete_prefix`'s
/// walk-then-delete *logic* (does it find and remove every key across a
/// multi-depth tree), not the batch endpoint's wire behavior, which Rainyun
/// cannot use via `aws-sdk-s3` at all.
///
/// The tree deliberately contains **four** keys of three different shapes:
/// a top-level object, one nested two levels deep (to exercise the
/// delimiter-less walk), and a zero-byte folder marker created via
/// `create_folder`. The marker matters on its own: the delimiter-`/`
/// `list_objects` rolls it into a CommonPrefix and filters it away, so only
/// the flat walk `delete_prefix` uses can see -- and therefore delete -- it.
/// An in-app empty folder that survives a delete is exactly the bug that
/// hides behind not testing this.
///
/// The objects are never deleted individually here; `delete_prefix` is
/// called directly, and the result is checked twice: the returned
/// `BatchResult`, and -- far more importantly -- a `list_objects_flat` that
/// must come back empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn delete_prefix_removes_every_object_under_it() {
    let conn = rainyun_connection();
    let bucket = rainyun_bucket();
    let provider = from_connection(&conn).expect("a valid Rainyun connection profile should build");
    let prefix = test_prefix();

    let dir = tempfile::tempdir().expect("tempdir");
    let keys = [
        format!("{prefix}a.bin"),
        format!("{prefix}nested/b.bin"),
        format!("{prefix}nested/deep/c.bin"),
    ];
    for (i, key) in keys.iter().enumerate() {
        let path = dir.path().join(format!("obj{i}.bin"));
        let size = 4 * 1024; // a few KB is plenty; this test is about fan-out, not payload size
        write_pseudo_random_file(&path, size, 0x0777_0010 + i as u64);
        provider
            .put_object_from_file(&bucket, key, &path, size)
            .await
            .unwrap_or_else(|e| {
                panic!("put_object_from_file for `{key}` should succeed against live Rainyun: {e}")
            });
    }

    // A zero-byte folder marker, the fourth key: `create_folder` normalizes
    // this to `{prefix}emptydir/`. Only the delimiter-less walk can see it.
    provider
        .create_folder(&bucket, &format!("{prefix}emptydir"))
        .await
        .expect("create_folder should succeed against live Rainyun");

    // Pin the fan-out size BEFORE deleting anything. `delete_prefix` walks
    // exactly this listing, so asserting it here means a surprising success
    // count below is diagnosed as "the walk saw something unexpected"
    // (e.g. the backend synthesizing directory rows for `nested/` and
    // `nested/deep/`, which MinIO does not do) rather than as a delete
    // failure -- and it is diagnosed while the objects are still there to
    // look at.
    let mut before: Vec<String> = provider
        .list_objects_flat(&bucket, &prefix, None, 100)
        .await
        .expect("list_objects_flat should succeed against live Rainyun")
        .entries
        .into_iter()
        .map(|e| e.key)
        .collect();
    before.sort();
    let mut want: Vec<String> = keys.to_vec();
    want.push(format!("{prefix}emptydir/"));
    want.sort();
    assert_eq!(
        before, want,
        "the flat walk `delete_prefix` uses must see exactly the 3 objects plus the zero-byte \
         `emptydir/` folder marker -- the marker in particular is invisible to the delimiter-`/` \
         `list_objects`, so only this walk can delete it"
    );

    // Deliberately NOT deleting the 4 keys individually here -- the whole
    // point of this test is to exercise `delete_prefix`'s own multi-key
    // fan-out, which every other test in this file accidentally skips (see
    // the doc comment above).
    let deleted = provider
        .delete_prefix(&bucket, &prefix)
        .await
        .expect("delete_prefix should succeed against live Rainyun");
    assert_eq!(
        (deleted.succeeded, deleted.failed.len()),
        (4, 0),
        "delete_prefix must report exactly 4 successes (3 objects + 1 folder marker) and no \
         per-key failures, got: {deleted:?}"
    );

    // Asserting the return value is NOT enough -- see this file's other
    // post-delete assertions for why. List the prefix back and confirm the
    // objects are actually gone, not just acknowledged.
    let remaining = provider
        .list_objects_flat(&bucket, &prefix, None, 10)
        .await
        .expect("list_objects_flat should succeed against live Rainyun");
    assert!(
        remaining.entries.is_empty(),
        "delete_prefix reported 4 successes, but list_objects_flat still shows objects under \
         `{prefix}`: {:?}",
        remaining.entries
    );

    cleanup_prefix(&provider, &bucket, &prefix).await;
}
