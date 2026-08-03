//! Tencent Cloud COS end-to-end integration tests (opt-in, LIVE).
//!
//! **These tests hit a real Tencent COS endpoint.** Like `oss_e2e.rs`,
//! `qiniu_e2e.rs` and `rainyun_e2e.rs` (and unlike `minio_e2e.rs` /
//! `rustfs_e2e.rs`, which drive disposable local containers), the target
//! account is a real, paid one whose buckets may hold data that matters.
//! Every test in this file is `#[ignore]`d, so a plain `cargo test` never
//! touches the network, never reads a single one of the environment variables
//! below, and never sees a credential -- this suite is opt-in, run explicitly,
//! never part of the hermetic default suite.
//!
//! ## What this suite proves about COS specifically
//!
//! Everything below was probed directly against a live account on 2026-07-31
//! -- one bucket in `ap-beijing`, created that same day, so squarely inside
//! the post-2024-01-01 rules -- **before any implementation code was
//! written**. These tests are what keep those findings from silently rotting.
//!
//! COS is the first provider BucketCat had to pull out of **two** default
//! branches, and one of the two contradicted the pre-implementation
//! expectation outright.
//!
//! - **Virtual-hosted addressing is mandatory, not preferred.** Tencent's
//!   "存储桶域名使用安全管理通知" (in force since 2024-01-01) retires
//!   path-style for buckets created on or after that date. Live: the same
//!   `ListObjectsV2` that succeeds virtual-hosted comes back
//!   `PathStyleDomainForbidden` -- "The bucket you are attempting to access
//!   must be addressed using COS virtual-styled domain." That is why
//!   `provider::s3::uses_path_style` returns `false` for `"cos"`, joining
//!   Aliyun OSS and real AWS. [`path_style_addressing_is_rejected`] is the
//!   load-bearing sentinel: without it, moving COS back into the default
//!   branch would break every post-cutoff bucket with no unit test noticing.
//!
//! - **Multi-Object Delete does NOT work -- and this is where the guess was
//!   wrong.** COS was expected to keep the batch path, the way Qiniu (the
//!   other Chinese provider here whose S3 surface is otherwise AWS-faithful)
//!   does. A real `aws-sdk-s3` `DeleteObjects` -- carrying
//!   `x-amz-checksum-crc32` and no `Content-MD5`, which is the only shape the
//!   SDK can send -- came back `400 InvalidRequest: Missing required header
//!   for this request: Content-MD5`, verbatim what Aliyun OSS and Rainyun ROS
//!   return. So `supports_batch_delete` excludes `"cos"` and every delete
//!   goes one key per request. Note this could only be found by sending the
//!   SDK's own request: a hand-rolled `curl` probe is exactly what produced
//!   the wrong answer for Rainyun, whose suite then failed 4 of 6 on its
//!   first live run.
//!
//! - **A COS connection is one region, full stop.** `ListBuckets` against
//!   `cos.ap-beijing.myqcloud.com` returned the account's single bucket;
//!   `ap-guangzhou`, `ap-shanghai`, `ap-nanjing`, `ap-chengdu` and
//!   `ap-hongkong` each returned zero. Only `service.cos.myqcloud.com` is
//!   account-wide. Tencent's own third-party-app guide says the same:
//!   "在应用中只能在服务地址指定的地域创建或选择存储桶". So COS gets no
//!   `RegionRouting` -- unlike OSS and Qiniu, whose account-wide
//!   `ListBuckets` hands the object plane buckets their own endpoint cannot
//!   serve. There is nothing to route between here.
//!
//! - **The endpoint needs no rewrite.** `cos.<region>.myqcloud.com` already
//!   *is* the S3-compatible host; unlike Aliyun's native `oss-*` form, there
//!   is no `s3.` prefix to add. `s3_compat_endpoint`'s non-OSS passthrough is
//!   correct as-is.
//!
//! - **The credential-scope region is not validated.** Signing for
//!   `us-east-1` against the `ap-beijing` host still returns 200
//!   ([`a_mismatched_region_is_not_rejected`] proves it live). The app stores
//!   the real region id anyway: the UI reads better, and `cos-regions.ts`
//!   resolves the endpoint back to a region by that id.
//!
//! - **`aws-chunked` must not be sent.** Covered by the pre-existing,
//!   non-COS-specific rule in `provider::s3::build_client` that any non-AWS
//!   endpoint gets `RequestChecksumCalculation::WhenRequired`.
//!   [`multipart_upload_round_trip`] is the only test that can catch a
//!   regression there: a unit test on a config value cannot prove wire
//!   behavior, and it is the streamed, multi-part upload path that actually
//!   trips over it.
//!
//! ## Two COS limitations BucketCat cannot work around
//!
//! Both come from [`对象存储域名合规问题`][compliance] and apply to buckets
//! created on or after 2024-01-01, on the **default** bucket domain -- which
//! is the only domain BucketCat has. A custom domain lifts both, but
//! configuring one is a console-side action outside this app.
//!
//! 1. **Every object is served `Content-Disposition: attachment` plus
//!    `x-amz-force-download: true`.** Confirmed live on both a `.png` and a
//!    `.txt`. The status is still `200` and the body is intact, so this does
//!    *not* affect in-app preview: `details-panel.tsx` renders images/video
//!    through `<img>`/`<video>` (subresource loads ignore
//!    `Content-Disposition`) and text through `fetch(...).text()` (which
//!    never looks at it). What it does change is a **shared** presigned link:
//!    a recipient opening it in a browser gets a download, not an inline
//!    view. [`presigned_get_works`] pins both halves of that.
//!
//! 2. **`.apk` / `.ipa` objects cannot be downloaded at all.** Not merely
//!    blocked for anonymous browsers -- an authenticated SDK `GetObject`
//!    gets `403 DownloadForbidden`: "The APK/IPA file is not allowed to be
//!    distributed in a public network using COS default domain, please use
//!    custom domain instead." This is a real functional gap for COS
//!    connections, and [`apk_download_is_forbidden_on_the_default_domain`]
//!    exists to record it as tested behavior rather than folklore -- if
//!    Tencent ever lifts it, that test fails and the docs get corrected.
//!
//! [compliance]: https://cloud.tencent.com/document/product/436/102489
//!
//! ## Safety rules (identical to `qiniu_e2e.rs` / `rainyun_e2e.rs`)
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
//! You need a **dedicated, disposable COS bucket** you are comfortable having
//! a `bucketcat-e2e/` prefix written to and deleted from repeatedly. Do
//! **not** point this at a bucket holding anything you care about -- nothing
//! here creates or deletes the bucket itself, but writes and deletes happen
//! for real under that prefix.
//!
//! The bucket name must be COS's full `<BucketName-APPID>` form (e.g.
//! `examplebucket-1250000000`); the short name shown in some console views is
//! not addressable. The endpoint's region **must** match the bucket's, since
//! a COS regional endpoint cannot see buckets elsewhere.
//!
//! Credentials are a Tencent Cloud **SecretId** / **SecretKey** pair, created
//! in CAM (not in the COS console): sub-user keys at
//! <https://console.cloud.tencent.com/cam>, main-account keys at
//! <https://console.cloud.tencent.com/cam/capi>. Prefer a sub-user scoped to
//! this one bucket.
//!
//! ```bash
//! export BUCKETCAT_COS_ENDPOINT="https://cos.ap-beijing.myqcloud.com"
//! export BUCKETCAT_COS_REGION="ap-beijing"
//! export BUCKETCAT_COS_BUCKET="examplebucket-1250000000"
//! export BUCKETCAT_COS_AK="..."   # SecretId
//! export BUCKETCAT_COS_SK="..."   # SecretKey
//! cargo test --test cos_e2e -- --ignored
//! ```
//!
//! Any variable left unset makes the test `panic!` immediately with a message
//! naming the missing variable, rather than silently defaulting to anything --
//! there is no safe default for a real cloud account.

use std::path::Path;
use std::sync::Arc;

use aws_sdk_s3::config::{
    Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use sha2::{Digest, Sha256};

use bucketcat_lib::error::{AppError, AppResult};
use bucketcat_lib::provider::s3::{supports_batch_delete, uses_path_style};
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
            "missing required environment variable `{name}` -- see cos_e2e.rs's module doc \
             comment for the full list of required variables and how to run this suite"
        )
    })
}

/// The dedicated sandbox bucket these tests operate against. **No default,
/// ever** -- see this file's module doc comment and the safety rules it
/// documents. Nothing in this file creates or deletes it.
fn cos_bucket() -> String {
    required_env("BUCKETCAT_COS_BUCKET")
}

/// A `Connection` pointed at the live COS endpoint from the environment.
///
/// `provider` is `"cos"`, which `provider::s3` branches on in exactly two
/// places -- `uses_path_style` (virtual-hosted, forced) and
/// `supports_batch_delete` (`false`, so deletes go one key per request).
/// Everywhere else it rides the generic non-AWS path unchanged:
/// `s3_compat_endpoint`'s non-OSS passthrough (endpoint used verbatim), no
/// `RegionRouting`, and the non-AWS `RequestChecksumCalculation::WhenRequired`
/// rule (no `aws-chunked`).
fn cos_connection() -> Connection {
    Connection {
        id: "e2e-cos".to_string(),
        provider: "cos".to_string(),
        name: "e2e cos".to_string(),
        endpoint: required_env("BUCKETCAT_COS_ENDPOINT"),
        region: required_env("BUCKETCAT_COS_REGION"),
        access_key_id: required_env("BUCKETCAT_COS_AK"),
        secret_access_key: required_env("BUCKETCAT_COS_SK"),
        default_bucket: None,
        api_token: None,
    }
}

/// A raw `aws_sdk_s3::Client` with `force_path_style` under this test's
/// control, used only by [`path_style_addressing_is_rejected`].
///
/// This deliberately does **not** go through `provider::s3::build_client`:
/// the whole point is to send the request shape BucketCat no longer sends, to
/// prove COS rejects it. Everything else mirrors `build_client` exactly so
/// path style is the only variable.
fn raw_client(path_style: bool) -> aws_sdk_s3::Client {
    let credentials = Credentials::new(
        required_env("BUCKETCAT_COS_AK"),
        required_env("BUCKETCAT_COS_SK"),
        None,
        None,
        "bucketcat-e2e",
    );
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .endpoint_url(required_env("BUCKETCAT_COS_ENDPOINT"))
        .region(Region::new(required_env("BUCKETCAT_COS_REGION")))
        .credentials_provider(credentials)
        .force_path_style(path_style)
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
        .build();
    aws_sdk_s3::Client::from_conf(conf)
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
             manual cleanup of this prefix in the COS console may be needed"
        );
    }
}

// --- fixture / hashing helpers (mirrors qiniu_e2e.rs) ------------------------

/// Writes `size` bytes of deterministic LCG pseudo-random content to `path`.
///
/// Seeded (so the source hash is reproducible run to run) and, crucially,
/// non-compressible -- a file of repeated bytes would let a per-part offset
/// bug go unnoticed because every offset reads the same value. Knuth's MMIX
/// LCG constants give that without dragging a `rand` dev-dependency into the
/// test build. Seeds here live in their own `0x0BBB_` namespace, distinct
/// from `oss_e2e.rs`'s `0x0555_`, `rainyun_e2e.rs`'s `0x0777_`,
/// `qiniu_e2e.rs`'s `0x0999_` and `minio_e2e.rs`'s `0x5EED_`.
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

// --- anonymous HTTP fetch (for the presigned-URL tests) ---------------------

/// One response header this suite cares about, lowercased name + value.
type HeaderPair = (String, String);

/// Fetches `url` with **no credentials of any kind** and returns
/// `(status, headers, body)`.
///
/// Built on the in-process `hyper` + `hyper-rustls` client already in this
/// crate's dependency graph rather than shelling out to `curl`. That is a
/// deliberate safety choice, not a style preference: callers pass a
/// **presigned URL whose query string carries a live signature**, and a
/// subprocess argument is visible to every other process on the machine via
/// `ps`. Keeping the request in-process means the URL never leaves this
/// address space.
///
/// The `url` itself is never included in any error string this returns --
/// `http::uri::InvalidUri` renders only the kind of syntax problem, and
/// `hyper_util::client::legacy::Error`'s `Display` renders only its kind plus
/// cause, never the request URI.
///
/// Unlike `qiniu_e2e.rs`'s equivalent this also returns the response headers:
/// COS's forced `Content-Disposition: attachment` is a documented behavior
/// difference this suite has to assert on, not just tolerate.
async fn fetch_anonymously(url: &str) -> Result<(u16, Vec<HeaderPair>, Vec<u8>), String> {
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
    let headers: Vec<HeaderPair> = response
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_ascii_lowercase(),
                v.to_str().unwrap_or("<non-ascii>").to_string(),
            )
        })
        .collect();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("reading the anonymous GET's response body failed: {e}"))?
        .to_bytes()
        .to_vec();

    Ok((status, headers, body))
}

/// Looks a header up by (already lowercased) name.
fn header<'a>(headers: &'a [HeaderPair], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

// --- 1: connection / listing -------------------------------------------------

/// `test_connection` succeeds against the live endpoint. The cheapest proof
/// that the endpoint, the SecretId/SecretKey pair and SigV4 signing all line
/// up before any other test bothers moving bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn test_connection_succeeds() {
    let provider =
        from_connection(&cos_connection()).expect("a valid COS connection profile should build");
    provider
        .test_connection()
        .await
        .expect("test_connection should succeed against the live COS endpoint");
}

/// `ListBuckets` works, and the configured sandbox bucket is among what it
/// returns.
///
/// A COS regional endpoint only reports buckets **in that region** (see this
/// file's module doc). So this doubles as the check that the configured
/// `BUCKETCAT_COS_ENDPOINT` and `BUCKETCAT_COS_BUCKET` actually belong
/// together -- if they don't, the bucket simply will not be in the list, and
/// the message below says so rather than leaving a later, more confusing
/// failure to explain it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn list_buckets_is_scoped_to_the_endpoints_region() {
    let bucket = cos_bucket();
    let provider =
        from_connection(&cos_connection()).expect("a valid COS connection profile should build");

    let buckets = provider
        .list_buckets()
        .await
        .expect("list_buckets should succeed against the live COS endpoint");

    assert!(
        buckets.iter().any(|b| b.name == bucket),
        "the configured sandbox bucket was not in this endpoint's ListBuckets response. A COS \
         regional endpoint reports only the buckets in ITS OWN region, so this almost certainly \
         means BUCKETCAT_COS_ENDPOINT names a different region than the bucket lives in. Buckets \
         this endpoint can see: {:?}",
        buckets.iter().map(|b| &b.name).collect::<Vec<_>>()
    );
}

/// `list_objects` works -- and the virtual-hosted addressing sentinel.
///
/// The assertion guards the branch, not the call: if someone moved COS back
/// into `uses_path_style`'s default `true` branch, this suite would fail all
/// over the place, but nothing would say *why*. This does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn list_objects_on_the_test_bucket_succeeds() {
    let conn = cos_connection();
    let bucket = cos_bucket();

    assert!(
        !uses_path_style(&conn.provider, &conn.endpoint),
        "BucketCat must address COS virtual-hosted. `uses_path_style(\"{}\", ..)` returned true, \
         which means someone dropped COS back into the default path-style branch. Tencent \
         retired path-style for buckets created on or after 2024-01-01, so that change does not \
         degrade COS -- it breaks it outright with `PathStyleDomainForbidden`. See \
         `path_style_addressing_is_rejected` and this file's module doc comment",
        conn.provider
    );

    let provider = from_connection(&conn).expect("a valid COS connection profile should build");

    provider.list_objects(&bucket, "", None, 10).await.expect(
        "list_objects against the live COS sandbox bucket should succeed -- a failure here means \
         the addressing style, the endpoint, the credentials, or SigV4 signing is wrong",
    );
}

/// **COS rejects path-style addressing outright.** The reason
/// `uses_path_style` has a `"cos"` branch at all.
///
/// This sends the request shape BucketCat deliberately no longer sends, so it
/// has to build its own client ([`raw_client`]) rather than go through
/// `from_connection`. Both shapes are exercised against the same bucket in the
/// same run, so the only variable is `force_path_style`.
///
/// Buckets created **before** 2024-01-01 still accept path-style. If this
/// test ever fails with the path-style call succeeding, check the sandbox
/// bucket's creation date before concluding Tencent reversed the policy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn path_style_addressing_is_rejected() {
    let bucket = cos_bucket();

    raw_client(false)
        .list_objects_v2()
        .bucket(&bucket)
        .max_keys(1)
        .send()
        .await
        .expect("virtual-hosted addressing must work -- this is the shape BucketCat sends");

    let err = raw_client(true)
        .list_objects_v2()
        .bucket(&bucket)
        .max_keys(1)
        .send()
        .await
        .expect_err(
            "path-style addressing must be rejected by COS. If it succeeded, confirm the sandbox \
             bucket was created on or after 2024-01-01 (pre-cutoff buckets are grandfathered in) \
             before concluding the policy changed",
        );

    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("PathStyleDomainForbidden") || rendered.contains("virtual-styled"),
        "expected COS's `PathStyleDomainForbidden` rejection, got a different failure -- which \
         means this test is no longer proving what it claims to: {rendered}"
    );
}

/// The credential-scope region is not validated: signing for `us-east-1`
/// against a `ap-*` host still works.
///
/// This is why the wizard can store a real region id without that choice
/// being load-bearing for authentication -- but the **host** very much does
/// matter, which `list_buckets_is_scoped_to_the_endpoints_region` covers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn a_mismatched_region_is_not_rejected() {
    let mut conn = cos_connection();
    conn.region = "us-east-1".to_string();
    let bucket = cos_bucket();

    let provider =
        from_connection(&conn).expect("a COS connection with an odd region should still build");
    provider.list_objects(&bucket, "", None, 1).await.expect(
        "COS does not validate the SigV4 credential-scope region. If this now fails with \
         `SignatureDoesNotMatch` or an region-mismatch error, COS started enforcing it and the \
         region field became load-bearing -- update this file's module doc comment",
    );
}

// --- 2: small object round trip ---------------------------------------------

/// Small-object round trip: put -> head (size) -> get_range (whole object) ->
/// SHA-256 match -> delete -> proof it's gone -> prefix cleanup. The baseline
/// data-plane proof, independent of the multipart / `aws-chunked` concern the
/// next test targets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn small_object_round_trip() {
    let bucket = cos_bucket();
    let provider =
        from_connection(&cos_connection()).expect("a valid COS connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}small.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("small.bin");
    let size = 48 * 1024;
    write_pseudo_random_file(&path, size, 0x0BBB_0001);
    let source_hash = hex(&sha256_bytes(
        &std::fs::read(&path).expect("reading the fixture back should succeed"),
    ));

    provider
        .put_object_from_file(&bucket, &key, &path, size)
        .await
        .expect("put_object_from_file should succeed against live COS");

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

// --- 3: multipart / the aws-chunked sentinel --------------------------------

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
    let bucket = cos_bucket();
    let provider =
        from_connection(&cos_connection()).expect("a valid COS connection profile should build");
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
    write_pseudo_random_file(&path, total, 0x0BBB_0002);
    let source_hash = hex(&sha256_bytes(
        &std::fs::read(&path).expect("reading the fixture back should succeed"),
    ));

    let upload_id = provider
        .multipart_init(&bucket, &key)
        .await
        .expect("multipart_init should succeed against live COS");

    let mut parts = Vec::new();
    for (number, offset, length) in [(1i32, 0u64, first), (2, first, last)] {
        match provider
            .upload_part_from_file(&bucket, &key, &upload_id, number, &path, offset, length)
            .await
        {
            Ok(etag) => parts.push(UploadedPart {
                number,
                etag,
                size: length,
            }),
            Err(e) => {
                let _ = provider.multipart_abort(&bucket, &key, &upload_id).await;
                cleanup_prefix(&provider, &bucket, &prefix).await;
                panic!(
                    "upload_part_from_file (part {number}) failed against live COS: {e}. A \
                     `400` mentioning `aws-chunked` or `x-amz-content-sha256` here is THE \
                     failure this test exists to catch -- it would mean the non-AWS \
                     `RequestChecksumCalculation::WhenRequired` rule in \
                     `provider::s3::build_client` stopped applying"
                );
            }
        }
    }

    if let Err(e) = provider
        .multipart_complete(&bucket, &key, &upload_id, &parts)
        .await
    {
        let _ = provider.multipart_abort(&bucket, &key, &upload_id).await;
        cleanup_prefix(&provider, &bucket, &prefix).await;
        panic!("multipart_complete failed against live COS: {e}");
    }

    let head = provider
        .head_object(&bucket, &key)
        .await
        .expect("head_object should succeed after multipart_complete");
    assert_eq!(
        head.size, total,
        "the assembled object's size does not match the source"
    );

    let downloaded = get_range_bytes(&provider, &bucket, &key, 0, total)
        .await
        .expect("get_range over the whole assembled object should succeed");
    assert_eq!(
        hex(&sha256_bytes(&downloaded)),
        source_hash,
        "the multipart-assembled object's SHA-256 does not match the source -- a mismatch here \
         means parts were assembled out of order or a per-part offset is wrong"
    );

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 4: delete -- the batch path is NOT available ---------------------------

/// **COS cannot use Multi-Object Delete, and `delete_objects` must still
/// work.**
///
/// This is the inverse of `qiniu_e2e.rs`'s `batch_delete_uses_the_multi_object_path`:
/// there the assertion guards that the fast path stays available, here it
/// guards that BucketCat keeps taking the slow one. Both halves are checked:
///
/// 1. `supports_batch_delete("cos")` is `false`, so `delete_objects`
///    dispatches to the per-key fallback.
/// 2. Sending the batch request anyway (via [`raw_client`], bypassing the
///    provider) really is rejected -- so the fallback is a necessity, not a
///    superstition someone can "clean up" later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn batch_delete_is_unavailable_and_deletes_fall_back_to_single_requests() {
    let conn = cos_connection();
    let bucket = cos_bucket();

    assert!(
        !supports_batch_delete(&conn.provider),
        "COS hard-requires `Content-MD5` on Multi-Object Delete, which aws-sdk-s3 has no way to \
         send. `supports_batch_delete(\"{}\")` returned true, which would route deletes into a \
         batch request COS rejects with `400 InvalidRequest ... Content-MD5`",
        conn.provider
    );

    let provider = from_connection(&conn).expect("a valid COS connection profile should build");
    let prefix = test_prefix();

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tiny.bin");
    let size = 512;
    write_pseudo_random_file(&path, size, 0x0BBB_0003);

    let keys: Vec<String> = (0..3).map(|i| format!("{prefix}batch-{i}.bin")).collect();
    for key in &keys {
        provider
            .put_object_from_file(&bucket, key, &path, size)
            .await
            .expect("put_object_from_file should succeed against live COS");
    }

    // (2) The batch endpoint really does reject the SDK's request shape.
    // Probed here, on objects this test already owns, rather than trusted
    // from a comment. `DeleteObjects` is sent through the raw client so the
    // provider's own fallback doesn't mask it.
    let batch_err = raw_client(false)
        .delete_objects()
        .bucket(&bucket)
        .delete(
            aws_sdk_s3::types::Delete::builder()
                .set_objects(Some(
                    keys.iter()
                        .map(|k| {
                            aws_sdk_s3::types::ObjectIdentifier::builder()
                                .key(k)
                                .build()
                                .expect("a key is all ObjectIdentifier requires")
                        })
                        .collect(),
                ))
                .build()
                .expect("a non-empty object list is all Delete requires"),
        )
        .send()
        .await
        .expect_err(
            "COS must reject Multi-Object Delete. If it now succeeds, COS started accepting the \
             `x-amz-checksum-*` family aws-sdk-s3 sends, and `supports_batch_delete` should drop \
             its `\"cos\"` exclusion -- along with this file's module doc comment",
        );
    let rendered = format!("{batch_err:?}");
    assert!(
        rendered.contains("Content-MD5"),
        "expected COS's `Missing required header for this request: Content-MD5` rejection, got a \
         different failure -- this test is no longer proving what it claims to: {rendered}"
    );

    // (1) And the provider's fallback deletes all three anyway.
    let result = provider
        .delete_objects(&bucket, &keys)
        .await
        .expect("delete_objects should succeed via the per-key fallback");
    assert_eq!(
        result.succeeded,
        keys.len() as u32,
        "every key should have been deleted; failures: {:?}",
        result.failed
    );
    assert!(
        result.failed.is_empty(),
        "no key should have failed: {:?}",
        result.failed
    );

    for key in &keys {
        let err = provider
            .head_object(&bucket, key)
            .await
            .expect_err("every deleted key must be gone");
        assert_eq!(
            err.code(),
            "storage/key-not-found",
            "unexpected error for {key}: {err}"
        );
    }

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

/// `delete_prefix` removes every object under a prefix, including the
/// zero-byte folder marker -- over the per-key fallback, since COS has no
/// batch path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn delete_prefix_removes_every_object_under_it() {
    let bucket = cos_bucket();
    let provider =
        from_connection(&cos_connection()).expect("a valid COS connection profile should build");
    let prefix = test_prefix();
    let folder = format!("{prefix}nested/");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tiny.bin");
    let size = 256;
    write_pseudo_random_file(&path, size, 0x0BBB_0004);

    provider
        .create_folder(&bucket, &folder)
        .await
        .expect("create_folder should succeed against live COS");
    for i in 0..3 {
        provider
            .put_object_from_file(&bucket, &format!("{folder}f{i}.bin"), &path, size)
            .await
            .expect("put_object_from_file should succeed against live COS");
    }

    let result = provider
        .delete_prefix(&bucket, &folder)
        .await
        .expect("delete_prefix should succeed against live COS");
    assert!(
        result.failed.is_empty(),
        "no key should have failed: {:?}",
        result.failed
    );
    assert!(
        result.succeeded >= 4,
        "expected the 3 objects plus the folder marker to be deleted, got {}",
        result.succeeded
    );

    let remaining = provider
        .list_objects_flat(&bucket, &folder, None, 10)
        .await
        .expect("list_objects_flat should succeed");
    assert!(
        remaining.entries.is_empty(),
        "delete_prefix left {} object(s) behind",
        remaining.entries.len()
    );

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 5: rename ---------------------------------------------------------------

/// `rename_object` works: object stores have no native rename, so this is
/// `CopyObject` followed by `DeleteObject`, and `x-amz-copy-source` has to be
/// percent-encoded by hand (the SDK does not do it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn rename_object_moves_the_object() {
    let bucket = cos_bucket();
    let provider =
        from_connection(&cos_connection()).expect("a valid COS connection profile should build");
    let prefix = test_prefix();
    // A space and a non-ASCII segment on purpose: those are exactly what an
    // unencoded `x-amz-copy-source` breaks on.
    let from_key = format!("{prefix}原始 文件.bin");
    let to_key = format!("{prefix}renamed 文件.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("rename.bin");
    let size = 2 * 1024;
    write_pseudo_random_file(&path, size, 0x0BBB_0005);

    provider
        .put_object_from_file(&bucket, &from_key, &path, size)
        .await
        .expect("put_object_from_file should succeed against live COS");

    provider
        .rename_object(&bucket, &from_key, &to_key)
        .await
        .expect("rename_object should succeed against live COS");

    provider
        .head_object(&bucket, &to_key)
        .await
        .expect("the renamed object should exist at its new key");
    let err = provider
        .head_object(&bucket, &from_key)
        .await
        .expect_err("the source key must be gone after a rename");
    assert_eq!(
        err.code(),
        "storage/key-not-found",
        "unexpected error: {err}"
    );

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 6: presigned URLs and the two default-domain limitations ---------------

/// `presign_get` produces a URL a credential-less client can fetch -- **and**
/// COS's forced-download headers ride along with it.
///
/// Both halves matter and are asserted together on purpose. The 200 + hash
/// match is the same proof every other suite makes. The header assertions
/// pin the limitation documented in this file's module doc: a shared link
/// downloads rather than previews in a recipient's browser. In-app preview is
/// unaffected (subresource loads and `fetch()` both ignore
/// `Content-Disposition`), which is exactly why this has to be asserted here
/// rather than "noticed" later.
///
/// If Tencent lifts the restriction (or the bucket predates 2024-01-01), the
/// header assertions fail loudly and the module doc gets corrected -- that is
/// the intent, not a maintenance burden.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn presigned_get_works() {
    let bucket = cos_bucket();
    let provider =
        from_connection(&cos_connection()).expect("a valid COS connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}shared.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("shared.bin");
    let size = 8 * 1024;
    write_pseudo_random_file(&path, size, 0x0BBB_0006);
    let source = std::fs::read(&path).expect("reading the fixture back should succeed");

    provider
        .put_object_from_file(&bucket, &key, &path, size)
        .await
        .expect("put_object_from_file should succeed against live COS");

    let url = provider
        .presign_get(&bucket, &key, 300)
        .await
        .expect("presign_get should produce a URL");

    let outcome = fetch_anonymously(&url).await;
    // Cleanup first: the assertions below must not be able to strand objects.
    cleanup_prefix(&provider, &bucket, &prefix).await;

    let (status, headers, body) = outcome.expect("the anonymous fetch should have completed");
    assert_eq!(
        status, 200,
        "the presigned URL should be fetchable without credentials; got HTTP {status}. A 403 \
         means COS rejected the SigV4 query-string signature"
    );
    assert_eq!(
        hex(&sha256_bytes(&body)),
        hex(&sha256_bytes(&source)),
        "the anonymously fetched body does not match the uploaded object"
    );

    assert_eq!(
        header(&headers, "content-disposition"),
        Some("attachment"),
        "COS's default bucket domain forces `Content-Disposition: attachment` on every object in \
         a post-2024-01-01 bucket. If that header is gone, the limitation documented in this \
         file's module doc comment no longer holds and should be removed from it"
    );
    assert_eq!(
        header(&headers, "x-amz-force-download"),
        Some("true"),
        "COS's default bucket domain sets `x-amz-force-download: true` alongside the attachment \
         disposition. Its disappearance is the same signal as the assertion above"
    );
}

/// **`.apk` / `.ipa` objects cannot be downloaded from a COS default bucket
/// domain -- not even with valid credentials.**
///
/// This test asserts a *limitation*, which is unusual, so the reasoning is
/// worth stating: BucketCat can upload such an object fine and will list it
/// like any other, but every read of it fails. Recording that as a test means
/// (a) the behavior is proven rather than assumed, and (b) if Tencent ever
/// lifts it, this test fails and whoever sees the failure is pointed straight
/// at the docs that need correcting.
///
/// A custom bucket domain lifts the restriction, but configuring one is a
/// console-side action outside this app.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn apk_download_is_forbidden_on_the_default_domain() {
    let bucket = cos_bucket();
    let provider =
        from_connection(&cos_connection()).expect("a valid COS connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}sample.apk");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sample.apk");
    let size = 1024;
    write_pseudo_random_file(&path, size, 0x0BBB_0007);

    // Uploading is fine -- the restriction is read-side only.
    provider
        .put_object_from_file(&bucket, &key, &path, size)
        .await
        .expect("uploading an .apk should succeed; COS only restricts reading it back");

    // ... and so is listing it, which is why the gap is invisible until a
    // user actually tries to download.
    provider
        .head_object(&bucket, &key)
        .await
        .expect("head_object on an .apk should succeed; only the object read is forbidden");

    let err = get_range_bytes(&provider, &bucket, &key, 0, size)
        .await
        .expect_err(
            "COS must refuse to serve an .apk from the default bucket domain. If this now \
             succeeds, Tencent lifted the restriction (or the sandbox bucket predates \
             2024-01-01) and this file's module doc comment plus the user-facing docs should \
             drop the limitation",
        );
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("DownloadForbidden") || rendered.contains("access-denied"),
        "expected COS's `DownloadForbidden` rejection, got a different failure -- this test is no \
         longer proving what it claims to: {rendered}"
    );

    cleanup_prefix(&provider, &bucket, &prefix).await;
}
