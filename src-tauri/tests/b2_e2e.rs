//! Backblaze B2 end-to-end integration tests (opt-in, LIVE).
//!
//! **These tests hit a real Backblaze B2 endpoint.** Like `oss_e2e.rs`,
//! `qiniu_e2e.rs`, `rainyun_e2e.rs` and `r2_e2e.rs` (and unlike `minio_e2e.rs`,
//! which drives a disposable local container), the target account is a real,
//! billed one whose buckets may hold data that matters. Every test in this file
//! is `#[ignore]`d, so a plain `cargo test` never touches the network, never
//! reads a single one of the environment variables below, and never sees a
//! credential -- this suite is opt-in, run explicitly, never part of the
//! hermetic default suite.
//!
//! ## What this suite proves about B2 specifically
//!
//! B2's S3-compatible interface needs **no provider branch anywhere in
//! `provider::s3`** -- it lands on every default. That is precisely why these
//! tests exist: "it works because nobody wrote code for it" is invisible to
//! every other test in the crate, so a change to one of those defaults would
//! silently take B2 with it. Everything below was probed directly against a
//! live account on 2026-07-30 before any code was written.
//!
//! - **Path-style AND virtual-hosted both work.** `ListObjectsV2` returned 200
//!   for both shapes on the same bucket. BucketCat stays on path-style, which
//!   `uses_path_style` gives every non-AWS, non-OSS provider.
//!   [`list_objects_on_the_test_bucket_succeeds`] is the sentinel against a
//!   future change moving B2 into the virtual-hosted branch: B2 would keep
//!   *working*, so only an explicit assertion can catch it.
//! - **The credential-scope region is not validated.** Signing for
//!   `us-east-005` against the `us-west-004` host still returns 200. The app
//!   stores the real region id anyway, because the *host* very much does
//!   matter -- see the next point.
//! - **Multi-Object Delete works, but only just.** B2 refuses a `DeleteObjects`
//!   carrying no integrity header at all:
//!   `400 InvalidRequest: Missing required header for this request:
//!   Content-MD5 OR x-amz-checksum-*`. The `OR` is what saves it -- B2 accepts
//!   the modern `x-amz-checksum-*` family that `aws-sdk-s3` still sends under
//!   `RequestChecksumCalculation::WhenRequired` for operations the S3 model
//!   marks as requiring one. Aliyun OSS and Rainyun ROS accept only the legacy
//!   `Content-MD5`, which is why they are excluded from `supports_batch_delete`
//!   and B2 is not. [`batch_delete_uses_the_multi_object_path`] is the test
//!   that matters most here: the pre-implementation probe was hand-rolled
//!   Python, so this suite's first run is the first time a *real* `aws-sdk-s3`
//!   `DeleteObjects` has gone at the endpoint. **Rainyun's suite is the
//!   cautionary tale** -- the same assumption looked fine on paper and was
//!   demolished by its first live run (4 of 6 tests failed).
//! - **`aws-chunked` must not be sent.** Covered by the pre-existing, non-B2
//!   rule in `provider::s3::build_client` that any non-AWS endpoint gets
//!   `RequestChecksumCalculation::WhenRequired`.
//!   [`multipart_upload_round_trip`] is the only test that can catch a
//!   regression there.
//!
//! ## The actual special case: the region is baked into the credential
//!
//! A B2 account lives in exactly one region, fixed when the account is created,
//! and its keys are refused everywhere else. There is nothing to route between
//! (unlike OSS and Qiniu), so B2 deliberately gets **no** `RegionRouting`.
//!
//! What makes this worth a feature rather than a footnote is the *failure
//! mode*: a right key against the wrong region's host answers
//! `403 InvalidAccessKeyId / "The key '004...' is not valid"`. A user reading
//! "the key is not valid" rebuilds their credentials over and over and never
//! learns it was the region. [`a_wrong_region_host_reports_a_misleading_key_error`]
//! pins that behavior, because it is the entire justification for
//! `provider::b2`'s derivation and for `B2Fields` deriving the region instead
//! of asking.
//!
//! [`authorize_account_agrees_with_the_key_id_derivation`] then closes the
//! loop: it proves the offline guess (`b2_region_from_key_id`, built on a
//! convention **Backblaze has never documented**) matches what Backblaze itself
//! reports. If Backblaze ever changes the key-id format, that test is what
//! fails -- loudly, and long before a user hits it.
//!
//! ## Safety rules (identical to `qiniu_e2e.rs`, plus one B2-only rule)
//!
//! - **Nothing creates or deletes a bucket.** Only objects, and only under this
//!   suite's own prefix.
//! - **Every write is prefix-scoped.** Each test that writes data calls
//!   [`test_prefix`] for its own `bucketcat-e2e/{uuid}/` prefix, so concurrent
//!   (and repeated) runs never collide and cleanup can never reach outside that
//!   prefix.
//! - **Cleanup is prefix-scoped and self-checking.** [`cleanup_prefix`] refuses
//!   (via `assert!`) to run against anything that isn't a real, non-empty
//!   `bucketcat-e2e/` prefix. Its reach is real but not total: `delete_prefix`
//!   enumerates via `ListObjectsV2`, which never lists an **in-progress**
//!   multipart upload -- so `cleanup_prefix` alone can never reach or abort
//!   one. [`multipart_upload_round_trip`] guards that window itself: every
//!   failure path in it explicitly calls `multipart_abort` first.
//! - **B2-only: cleanup must purge object *versions*, not just objects.**
//!   Verified 2026-07-31 on the sandbox account: `GetBucketVersioning` reports
//!   `Enabled` and the bucket carries **no lifecycle rules**, so a plain
//!   `DeleteObject` only writes a delete marker and every byte this suite ever
//!   uploaded stays billable forever. (The probe that established this deleted
//!   a key that did not exist and still got back a `DeleteMarkerVersionId`.)
//!   That is correct behavior for BucketCat -- `delete_prefix` does what a user
//!   asked for on a versioned bucket -- but it makes a naive test suite a slow
//!   storage leak, so [`cleanup_prefix`] follows `delete_prefix` with
//!   [`purge_versions`], which walks `ListObjectVersions` and deletes every
//!   version *and* delete marker under the prefix by id. None of the other five
//!   suites needs this.
//! - **Credentials never touch disk or a log line.** They are read from
//!   environment variables at test start and handed straight to the SDK. The
//!   presigned URL in [`presigned_get_works`] carries a live signature and is
//!   treated the same way: never printed, never passed as a subprocess argument
//!   (where `ps` would expose it), never interpolated into an assertion
//!   message. That is why that test fetches with the in-process `hyper` +
//!   `hyper-rustls` client already in this crate's dependency graph rather than
//!   shelling out to `curl`.
//!
//! ## Running these tests
//!
//! You need a **dedicated, disposable B2 bucket** you are comfortable having a
//! `bucketcat-e2e/` prefix written to and deleted from repeatedly. Do **not**
//! point this at a bucket holding anything you care about.
//!
//! The key must be an **application key**, not the master application key:
//! Backblaze does not accept master keys on the S3-compatible API at all
//! (`403 Malformed Access Key Id`), and `b2_admin::authorize_account` rejects
//! one by shape before it even reaches the network.
//!
//! `BUCKETCAT_B2_ENDPOINT` and `BUCKETCAT_B2_REGION` must match the account's
//! own region -- the one shown as the bucket's Endpoint in the Backblaze
//! console, and the one `b2_authorize_account` reports as `s3ApiUrl`. Pointing
//! them anywhere else fails every test with a misleading
//! `InvalidAccessKeyId`, which is exactly what
//! [`a_wrong_region_host_reports_a_misleading_key_error`] demonstrates on
//! purpose.
//!
//! ```bash
//! export BUCKETCAT_B2_ENDPOINT="https://s3.us-west-004.backblazeb2.com"
//! export BUCKETCAT_B2_REGION="us-west-004"
//! export BUCKETCAT_B2_BUCKET="your-disposable-sandbox-bucket"
//! export BUCKETCAT_B2_KEY_ID="..."
//! export BUCKETCAT_B2_APP_KEY="..."
//! cargo test --test b2_e2e -- --ignored
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
use bucketcat_lib::provider::b2::{
    b2_endpoint_for_region, b2_region_from_key_id, parse_s3_api_url,
};
use bucketcat_lib::provider::b2_admin;
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
/// this file's module doc comment when it's missing -- there is no safe default
/// for any of these against a real cloud account.
fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!(
            "missing required environment variable `{name}` -- see b2_e2e.rs's module doc \
             comment for the full list of required variables and how to run this suite"
        )
    })
}

/// The dedicated sandbox bucket these tests operate against. **No default,
/// ever** -- see this file's module doc comment and the safety rules it
/// documents. Nothing in this file creates or deletes it.
fn b2_bucket() -> String {
    required_env("BUCKETCAT_B2_BUCKET")
}

/// A `Connection` pointed at the live B2 endpoint from the environment.
///
/// `provider` is `"b2"`, which `provider::s3` branches on in **exactly zero**
/// places -- B2 rides every generic path unchanged: `uses_path_style`'s
/// "everything else" branch (path-style), `s3_compat_endpoint`'s non-OSS
/// passthrough (endpoint used verbatim), `supports_batch_delete`'s default
/// `true`, no `RegionRouting`, and the non-AWS
/// `RequestChecksumCalculation::WhenRequired` rule (no `aws-chunked`). Those
/// four choices are asserted as unit tests in `provider::s3`'s own `mod tests`
/// and proven on the wire here.
fn b2_connection() -> Connection {
    Connection {
        id: "e2e-b2".to_string(),
        provider: "b2".to_string(),
        name: "e2e b2".to_string(),
        endpoint: required_env("BUCKETCAT_B2_ENDPOINT"),
        region: required_env("BUCKETCAT_B2_REGION"),
        access_key_id: required_env("BUCKETCAT_B2_KEY_ID"),
        secret_access_key: required_env("BUCKETCAT_B2_APP_KEY"),
        default_bucket: None,
        api_token: None,
    }
}

/// A fresh `bucketcat-e2e/{uuid}/` prefix, unique per call -- every writing
/// test gets its own so concurrent/repeated runs never collide, and
/// [`cleanup_prefix`] never has to reason about shared state.
fn test_prefix() -> String {
    format!("bucketcat-e2e/{}/", uuid::Uuid::new_v4())
}

/// A raw `aws_sdk_s3::Client` for the version-purging cleanup only.
///
/// This deliberately duplicates `provider::s3::build_client`'s configuration,
/// which that function's own doc comment warns against -- for production paths.
/// The reason it is acceptable here: `Provider` intentionally exposes no
/// version-aware API (BucketCat browses objects, not version histories), and a
/// test-only cleanup helper must not become a reason to widen the production
/// trait. If the two drift, the worst case is that cleanup stops working and
/// the suite starts leaking storage -- loudly, via the warnings
/// [`purge_versions`] prints -- rather than a user-visible bug.
fn raw_versioning_client(conn: &Connection) -> aws_sdk_s3::Client {
    let credentials = Credentials::new(
        conn.access_key_id.clone(),
        conn.secret_access_key.clone(),
        None,
        None,
        "bucketcat-e2e",
    );
    aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .endpoint_url(&conn.endpoint)
            .region(Region::new(conn.region.clone()))
            .credentials_provider(credentials)
            .force_path_style(true)
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
            .build(),
    )
}

/// Deletes every object **version** and delete marker under `prefix`.
///
/// B2 buckets have file versioning on, and the sandbox account carries no
/// lifecycle rules (verified 2026-07-31), so `delete_prefix` alone only writes
/// delete markers -- every byte this suite uploads would stay billable forever.
/// This is the B2-only cleanup step the other five suites don't need; see the
/// safety rules in this file's module doc.
///
/// Failures are warned about, never panicked on: a cleanup failure must never
/// mask (or overwrite the exit status of) the actual test assertion that ran
/// before it.
async fn purge_versions(conn: &Connection, bucket: &str, prefix: &str) {
    let client = raw_versioning_client(conn);
    let mut key_marker: Option<String> = None;
    let mut version_marker: Option<String> = None;

    loop {
        let mut request = client
            .list_object_versions()
            .bucket(bucket)
            .prefix(prefix)
            .max_keys(1000);
        if let Some(marker) = key_marker.take() {
            request = request.key_marker(marker);
        }
        if let Some(marker) = version_marker.take() {
            request = request.version_id_marker(marker);
        }

        let page = match request.send().await {
            Ok(page) => page,
            Err(e) => {
                eprintln!(
                    "WARNING: ListObjectVersions failed while purging `{prefix}` in bucket \
                     `{bucket}`: {e} -- old versions may be left behind and will keep accruing \
                     storage cost; purge this prefix in the Backblaze console"
                );
                return;
            }
        };

        // Versions and delete markers are two separate lists in the same
        // response, and both hold storage -- deleting only one of them leaves
        // the prefix looking empty while still being billed.
        let doomed: Vec<(String, String)> = page
            .versions()
            .iter()
            .filter_map(|v| Some((v.key()?.to_string(), v.version_id()?.to_string())))
            .chain(
                page.delete_markers()
                    .iter()
                    .filter_map(|m| Some((m.key()?.to_string(), m.version_id()?.to_string()))),
            )
            .collect();

        for (key, version_id) in doomed {
            if let Err(e) = client
                .delete_object()
                .bucket(bucket)
                .key(&key)
                .version_id(&version_id)
                .send()
                .await
            {
                eprintln!("WARNING: failed to purge version `{version_id}` of `{key}`: {e}");
            }
        }

        if !page.is_truncated().unwrap_or(false) {
            return;
        }
        key_marker = page.next_key_marker().map(str::to_string);
        version_marker = page.next_version_id_marker().map(str::to_string);
        // A truncated page with no continuation markers would loop forever;
        // stop instead and let the warning explain what was left behind.
        if key_marker.is_none() && version_marker.is_none() {
            eprintln!(
                "WARNING: ListObjectVersions reported a truncated page with no continuation \
                 marker while purging `{prefix}`; stopping to avoid an infinite loop"
            );
            return;
        }
    }
}

/// Deletes everything under `prefix`, versions included.
///
/// Refuses to run at all against anything that isn't a real, non-empty
/// `bucketcat-e2e/` prefix -- this is the last line of defense against a coding
/// mistake (an empty or mistyped prefix) turning cleanup into "delete the whole
/// bucket". Failures are logged, not panicked on.
async fn cleanup_prefix(provider: &S3Provider, conn: &Connection, bucket: &str, prefix: &str) {
    assert!(
        prefix.starts_with("bucketcat-e2e/") && prefix.len() > "bucketcat-e2e/".len(),
        "refusing to clean up a prefix that is not a real, non-empty `bucketcat-e2e/{{uuid}}/` \
         prefix: `{prefix}` -- this guard exists specifically to prevent an empty/mistyped \
         prefix from ever reaching delete_prefix"
    );
    if let Err(e) = provider.delete_prefix(bucket, prefix).await {
        eprintln!(
            "WARNING: cleanup_prefix failed to delete `{prefix}` in bucket `{bucket}`: {e} -- \
             manual cleanup of this prefix in the Backblaze console may be needed"
        );
    }
    // Always runs, even if the delete above failed: whatever *did* land still
    // has versions to reclaim.
    purge_versions(conn, bucket, prefix).await;
}

// --- fixture / hashing helpers (mirrors qiniu_e2e.rs) ------------------------

/// Writes `size` bytes of deterministic LCG pseudo-random content to `path`.
///
/// Seeded (so the source hash is reproducible run to run) and, crucially,
/// non-compressible -- a file of repeated bytes would let a per-part offset bug
/// go unnoticed because every offset reads the same value. Knuth's MMIX LCG
/// constants give that without dragging a `rand` dev-dependency into the test
/// build. Seeds here live in their own `0x0B22_` namespace, distinct from
/// `oss_e2e.rs`'s `0x0555_`, `rainyun_e2e.rs`'s `0x0777_`, `qiniu_e2e.rs`'s
/// `0x0999_`, `r2_e2e.rs`'s `0x0BBB_` and `minio_e2e.rs`'s `0x5EED_`.
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
/// `ps`. Keeping the request in-process means the URL never leaves this address
/// space.
///
/// The `url` itself is never included in any error string this returns.
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

// --- 1: the region derivation chain, end to end ------------------------------

/// Backblaze's own answer agrees with the offline key-id derivation.
///
/// This is the test that keeps `provider::b2`'s central guess honest. The
/// derivation reads a region out of the key id's three-digit cluster prefix --
/// a convention **Backblaze has never documented** (neither the
/// application-keys guide nor the master-key help page mentions the format).
/// Everything the connection form shows before the user clicks Test rests on
/// it.
///
/// So: ask Backblaze, and assert all three agree -- what
/// `b2_authorize_account` reports, what the offline derivation guessed, and
/// what the suite was configured with. If Backblaze ever changes the key-id
/// format, this fails loudly here rather than silently handing users a wrong
/// endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn authorize_account_agrees_with_the_key_id_derivation() {
    let conn = b2_connection();

    let probe = b2_admin::authorize_account(&conn.access_key_id, &conn.secret_access_key)
        .await
        .expect(
            "b2_authorize_account should succeed with a valid application key. A \
             `auth/invalid-credentials` here means either the pair is wrong or \
             BUCKETCAT_B2_KEY_ID is a MASTER key, which Backblaze does not accept on the \
             S3-compatible API (and which `authorize_account` rejects by shape before the \
             request)",
        );

    let derived = b2_region_from_key_id(&conn.access_key_id).expect(
        "the key id's cluster prefix should resolve to a region. If this is None, Backblaze has \
         either changed the key-id format or opened a cluster missing from `B2_CLUSTERS` -- both \
         are real findings, and both need `provider::b2` and `src/lib/b2-regions.ts` updated \
         together",
    );

    assert_eq!(
        probe.region, derived,
        "the offline key-id derivation disagrees with Backblaze's own s3ApiUrl. Backblaze is the \
         authority here; the undocumented cluster-prefix convention `b2_region_from_key_id` rests \
         on has changed and must be re-derived"
    );

    let (configured_region, configured_endpoint) = parse_s3_api_url(&conn.endpoint).expect(
        "BUCKETCAT_B2_ENDPOINT should be a B2 S3 endpoint of the form \
         https://s3.{region}.backblazeb2.com",
    );
    assert_eq!(
        probe.region, configured_region,
        "this suite is configured for a different region than the account actually lives in -- \
         every other test here would fail with a misleading `InvalidAccessKeyId`. Set \
         BUCKETCAT_B2_ENDPOINT/BUCKETCAT_B2_REGION to `{}` / `{}`",
        probe.endpoint, probe.region
    );
    assert_eq!(probe.endpoint, configured_endpoint);
    assert_eq!(
        conn.region, configured_region,
        "BUCKETCAT_B2_REGION and BUCKETCAT_B2_ENDPOINT disagree with each other"
    );
}

/// A right key against the **wrong** region's host reports a misleading key
/// error -- the whole reason BucketCat derives the region instead of asking.
///
/// This is the inverse of every other test here: it asserts a *failure*, and
/// specifically the shape of it. `403 InvalidAccessKeyId` on a key that is
/// perfectly valid is what sends users off rebuilding credentials for an hour.
/// If Backblaze ever improves this to something honest (a redirect, a
/// `PermanentRedirect`, an actual region error), this test fails -- and that
/// would be good news worth acting on, because the derivation could then be
/// relaxed.
///
/// Read-only: it never writes anything, to any bucket, in any region.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn a_wrong_region_host_reports_a_misleading_key_error() {
    let bucket = b2_bucket();
    let mut conn = b2_connection();

    // Pick a region mechanically that is not the configured one, and build its
    // host by table lookup rather than string surgery.
    let elsewhere = if conn.region == "us-east-005" {
        "us-west-004"
    } else {
        "us-east-005"
    };
    conn.endpoint =
        b2_endpoint_for_region(elsewhere).expect("the fallback region must be in the table");
    conn.region = elsewhere.to_string();

    let provider =
        from_connection(&conn).expect("a wrong-region B2 connection profile should still build");

    let err = provider
        .list_objects(&bucket, "", None, 1)
        .await
        .expect_err(
            "a B2 key is scoped to its own region's host. If this SUCCEEDED, Backblaze has \
             started routing across regions -- a real change that would make the whole region \
             derivation in `provider::b2` unnecessary",
        );

    assert_eq!(
        err.code(),
        "auth/invalid-credentials",
        "B2 answers a wrong-region request with `403 InvalidAccessKeyId`, which \
         `classify_error_code` maps to invalid credentials. Getting something else here means \
         the failure mode changed -- update `provider::b2`'s module doc and `b2-regions.ts`, \
         since both cite this exact behavior as the reason the region is derived. Actual: {err}"
    );
}

// --- 2: ListBuckets + path-style sentinel ------------------------------------

/// `ListBuckets` works and reports the sandbox bucket.
///
/// Unlike OSS (native-API detour) and Qiniu (per-bucket `GetBucketLocation`
/// fan-out), B2's `ListBuckets` needs no special handling at all -- a B2
/// account has one region, so `Bucket::region` is `None` for every entry by
/// design and that is not a failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn list_buckets_reports_the_sandbox_bucket() {
    let conn = b2_connection();
    let bucket = b2_bucket();
    let provider = from_connection(&conn).expect("a valid B2 connection profile should build");

    let buckets = provider
        .list_buckets()
        .await
        .expect("list_buckets should succeed against live B2");

    assert!(
        buckets.iter().any(|b| b.name == bucket),
        "the sandbox bucket `{bucket}` should appear in the listing. If the key is restricted to \
         particular buckets, make sure `{bucket}` is one of them; saw: {:?}",
        buckets.iter().map(|b| &b.name).collect::<Vec<_>>()
    );
}

/// `list_objects` works, and B2 stays on path-style addressing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn list_objects_on_the_test_bucket_succeeds() {
    let conn = b2_connection();
    let bucket = b2_bucket();

    assert!(
        uses_path_style(&conn.provider, &conn.endpoint),
        "BucketCat keeps B2 on path-style addressing. `uses_path_style(\"{}\", ..)` returned \
         false, which means someone added B2 to the virtual-hosted branch alongside AWS/OSS. B2 \
         happens to support both, so nothing else in this suite would have failed -- see this \
         file's module doc comment",
        conn.provider
    );

    let provider = from_connection(&conn).expect("a valid B2 connection profile should build");

    provider.list_objects(&bucket, "", None, 10).await.expect(
        "list_objects against the live B2 sandbox bucket should succeed -- a failure here means \
         path-style addressing, the endpoint, the credentials, or SigV4 signing is wrong",
    );
}

// --- 3: small object round trip ----------------------------------------------

/// Small-object round trip: put -> head (size) -> get_range (whole object) ->
/// SHA-256 match -> delete -> proof it's gone -> prefix cleanup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn small_object_round_trip() {
    let conn = b2_connection();
    let bucket = b2_bucket();
    let provider = from_connection(&conn).expect("a valid B2 connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}small.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("small.bin");
    let size = 48 * 1024;
    write_pseudo_random_file(&path, size, 0x0B22_0001);
    let source_hash = hex(&sha256_bytes(
        &std::fs::read(&path).expect("reading the fixture back should succeed"),
    ));

    provider
        .put_object_from_file(&bucket, &key, &path, size, Arc::new(|_| {}))
        .await
        .expect("put_object_from_file should succeed against live B2");

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

    cleanup_prefix(&provider, &conn, &bucket, &prefix).await;
}

// --- 4: multipart / the aws-chunked sentinel ---------------------------------

/// Streamed multipart upload round trip -- and the **only** test that can catch
/// an `aws-chunked` regression.
///
/// `provider::s3::build_client` sets `RequestChecksumCalculation::WhenRequired`
/// for every non-AWS endpoint precisely so the SDK never attaches a streaming
/// checksum trailer (which would force `aws-chunked` transfer encoding). A unit
/// test on that config value proves nothing about the wire; only a real
/// streamed part upload does.
///
/// Every failure path below goes through `multipart_abort` before returning: an
/// in-progress multipart upload is invisible to `ListObjectsV2` and thus to
/// `cleanup_prefix`, and holds quota until aborted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn multipart_upload_round_trip() {
    let conn = b2_connection();
    let bucket = b2_bucket();
    let provider = from_connection(&conn).expect("a valid B2 connection profile should build");
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
    write_pseudo_random_file(&path, total, 0x0B22_0002);
    let source = std::fs::read(&path).expect("reading the multipart fixture back should succeed");
    let source_hash = hex(&sha256_bytes(&source));

    let upload_id = provider
        .multipart_init(&bucket, &key)
        .await
        .expect("multipart_init should succeed against live B2");

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
                        "upload_part_from_file (part {number}) should succeed against live B2 -- \
                         a failure here most likely means the client sent `aws-chunked` transfer \
                         encoding, i.e. the non-AWS `RequestChecksumCalculation::WhenRequired` \
                         rule in provider::s3::build_client stopped applying: {e}"
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
        // mix-up shows up as a hash mismatch.
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
        cleanup_prefix(&provider, &conn, &bucket, &prefix).await;
        panic!("{message}");
    }

    cleanup_prefix(&provider, &conn, &bucket, &prefix).await;
}

// --- 5: THE batch-delete claim ----------------------------------------------

/// Multi-Object Delete works through `aws-sdk-s3`.
///
/// `supports_batch_delete` returns `true` for B2, which routes
/// `Provider::delete_objects` down the `POST /?delete` batch path. Aliyun OSS
/// and Rainyun ROS both go the other way: they hard-require the legacy
/// `Content-MD5` header, `aws-sdk-s3` only ever sends `x-amz-checksum-*`
/// flexible checksums, so both are forced onto the
/// one-DeleteObject-per-key fallback.
///
/// The pre-implementation probe (2026-07-30) established that B2 rejects a
/// `DeleteObjects` carrying **no** integrity header
/// (`400 InvalidRequest: Missing required header for this request: Content-MD5
/// OR x-amz-checksum-*`) and accepts one carrying `Content-MD5` -- but that
/// probe was hand-rolled Python. What actually ships is `aws-sdk-s3` sending
/// `x-amz-checksum-crc32` plus `x-amz-sdk-checksum-algorithm`, which that probe
/// never exercised. **Rainyun's suite is the cautionary tale**: the same
/// assumption looked fine on paper and was demolished by its first live run.
///
/// Multiple keys on purpose: `S3Provider::delete_objects` dispatches on
/// `self.batch_delete`, not on key count, so a single key would exercise the
/// same code path -- but a multi-key body is the shape a real `DeleteObjects`
/// request has, and it also proves the per-key result aggregation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn batch_delete_uses_the_multi_object_path() {
    let conn = b2_connection();
    let bucket = b2_bucket();

    assert!(
        supports_batch_delete(&conn.provider),
        "this test exists to prove Multi-Object Delete works for B2. \
         `supports_batch_delete(\"{}\")` returned false, so `delete_objects` would silently take \
         the one-request-per-key fallback and this test would prove nothing -- if that change was \
         deliberate (i.e. a live run DID reject the batch request), delete this test and record \
         the finding in this file's module doc comment instead",
        conn.provider
    );

    let provider = from_connection(&conn).expect("a valid B2 connection profile should build");
    let prefix = test_prefix();

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tiny.bin");
    let size = 512;
    write_pseudo_random_file(&path, size, 0x0B22_0003);

    let keys: Vec<String> = (0..3).map(|i| format!("{prefix}batch-{i}.bin")).collect();
    for key in &keys {
        provider
            .put_object_from_file(&bucket, key, &path, size, Arc::new(|_| {}))
            .await
            .expect("put_object_from_file should succeed against live B2");
    }

    let result = provider.delete_objects(&bucket, &keys).await.expect(
        "delete_objects should succeed against live B2. A `400 InvalidRequest ... Content-MD5 OR \
         x-amz-checksum-*` here is THE failure this test was written to catch: it would mean B2, \
         like Aliyun OSS and Rainyun ROS, rejects the flexible checksum aws-sdk-s3 sends on \
         Multi-Object Delete. The fix is one line -- add \"b2\" to the exclusion in \
         `provider::s3::supports_batch_delete` -- and this file's module doc comment, plus the \
         `b2_keeps_the_batch_delete_path` unit test in `provider::s3`, must be corrected to match",
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

    cleanup_prefix(&provider, &conn, &bucket, &prefix).await;
}

// --- 6: presigned GET --------------------------------------------------------

/// A presigned GET URL is fetchable with no credentials -- the Share feature's
/// foundation, and the proof that B2 accepts SigV4 **query-string** signing,
/// not just header signing.
///
/// The URL is never printed or passed to a subprocess; see
/// [`fetch_anonymously`].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn presigned_get_works() {
    let conn = b2_connection();
    let bucket = b2_bucket();
    let provider = from_connection(&conn).expect("a valid B2 connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}shared.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("shared.bin");
    let size = 8 * 1024;
    write_pseudo_random_file(&path, size, 0x0B22_0005);
    let source = std::fs::read(&path).expect("reading the fixture back should succeed");

    provider
        .put_object_from_file(&bucket, &key, &path, size, Arc::new(|_| {}))
        .await
        .expect("put_object_from_file should succeed against live B2");

    let url = provider
        .presign_get(&bucket, &key, 300)
        .await
        .expect("presign_get should produce a URL");

    let outcome = fetch_anonymously(&url).await;
    // Cleanup first: the assertions below must not be able to strand objects.
    cleanup_prefix(&provider, &conn, &bucket, &prefix).await;

    let (status, body) = outcome.expect("the anonymous fetch should have completed");
    assert_eq!(
        status, 200,
        "the presigned URL should be fetchable without credentials; got HTTP {status}. A 403 \
         means B2 rejected the SigV4 query-string signature"
    );
    assert_eq!(
        hex(&sha256_bytes(&body)),
        hex(&sha256_bytes(&source)),
        "the anonymously fetched body does not match the uploaded object"
    );
}

// --- 7: rename (copy-then-delete) -------------------------------------------

/// `rename_object` works: object stores have no native rename, so this is
/// `CopyObject` followed by `DeleteObject`, and `x-amz-copy-source` has to be
/// percent-encoded by hand (the SDK does not do it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn rename_object_moves_the_object() {
    let conn = b2_connection();
    let bucket = b2_bucket();
    let provider = from_connection(&conn).expect("a valid B2 connection profile should build");
    let prefix = test_prefix();
    // A space and a non-ASCII segment on purpose: those are exactly what an
    // unencoded `x-amz-copy-source` breaks on.
    let from_key = format!("{prefix}原始 文件.bin");
    let to_key = format!("{prefix}renamed 文件.bin");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("rename.bin");
    let size = 2 * 1024;
    write_pseudo_random_file(&path, size, 0x0B22_0006);

    provider
        .put_object_from_file(&bucket, &from_key, &path, size, Arc::new(|_| {}))
        .await
        .expect("put_object_from_file should succeed against live B2");

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

    cleanup_prefix(&provider, &conn, &bucket, &prefix).await;
}

// --- 8: recursive prefix delete ---------------------------------------------

/// `delete_prefix` removes a whole subtree, **including** the zero-byte
/// folder-marker object.
///
/// The marker is the interesting part: `list_objects` (delimiter `/`) rolls
/// `sub/` into a CommonPrefix and filters it out, so only the delimiter-less
/// `list_objects_flat` walk surfaces it as a real key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn delete_prefix_removes_every_object_under_it() {
    let conn = b2_connection();
    let bucket = b2_bucket();
    let provider = from_connection(&conn).expect("a valid B2 connection profile should build");
    let prefix = test_prefix();

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("leaf.bin");
    let size = 256;
    write_pseudo_random_file(&path, size, 0x0B22_0007);

    provider
        .create_folder(&bucket, &format!("{prefix}sub"))
        .await
        .expect("create_folder should write a zero-byte marker");
    for key in [format!("{prefix}a.bin"), format!("{prefix}sub/b.bin")] {
        provider
            .put_object_from_file(&bucket, &key, &path, size, Arc::new(|_| {}))
            .await
            .expect("put_object_from_file should succeed against live B2");
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

    // `delete_prefix` alone leaves delete markers behind on a versioned bucket,
    // which is correct for the app but a storage leak for a test suite -- see
    // this file's module doc.
    purge_versions(&conn, &bucket, &prefix).await;
}
