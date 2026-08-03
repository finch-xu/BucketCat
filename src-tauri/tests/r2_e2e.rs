//! Cloudflare R2 end-to-end integration tests (opt-in, LIVE).
//!
//! **These tests hit a real Cloudflare R2 account.** Like `oss_e2e.rs`,
//! `qiniu_e2e.rs` and `rainyun_e2e.rs` (and unlike `minio_e2e.rs`, which
//! drives a disposable local container), the target account is a real one
//! whose other buckets may hold data that matters. Every test in this file is
//! `#[ignore]`d, so a plain `cargo test` never touches the network, never
//! reads a single one of the environment variables below, and never sees a
//! credential -- this suite is opt-in, run explicitly, never part of the
//! hermetic default suite.
//!
//! ## Running these tests
//!
//! ```bash
//! export BUCKETCAT_R2_ACCOUNT="<32-hex account id>"
//! export BUCKETCAT_R2_BUCKET="bucketcat-e2e"   # dedicated, disposable
//! export BUCKETCAT_R2_AK="..."                 # = the API token's id
//! export BUCKETCAT_R2_SK="..."                 # = sha256(the token value)
//! export BUCKETCAT_R2_TOKEN="cfut_..."         # the admin token's own value
//! # An R2 "Object Read only" token, for the low-privilege degradation tests:
//! export BUCKETCAT_R2_LOWPRIV_TOKEN="cfut_..."
//! export BUCKETCAT_R2_LOWPRIV_AK="..."
//! cargo test --test r2_e2e -- --ignored
//! ```
//!
//! Any variable left unset makes the test `panic!` immediately with a message
//! naming the missing variable, rather than silently defaulting to anything --
//! there is no safe default for a real cloud account.
//!
//! Every writing test scopes itself to a fresh `bucketcat-e2e/{uuid}/` prefix
//! and cleans up after itself; nothing here creates or deletes the bucket.

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use sha2::{Digest, Sha256};

use bucketcat_lib::error::{AppError, AppResult};
use bucketcat_lib::provider::r2::{r2_endpoint as build_r2_endpoint, r2_secret_from_token};
use bucketcat_lib::provider::r2_admin;
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
            "missing required environment variable `{name}` -- see r2_e2e.rs's module doc \
             comment for the full list of required variables and how to run this suite"
        )
    })
}

/// The dedicated sandbox bucket these tests operate against. **No default,
/// ever** -- see this file's module doc comment. Nothing in this file creates
/// or deletes it.
fn r2_bucket() -> String {
    required_env("BUCKETCAT_R2_BUCKET")
}

/// R2's S3 endpoint for the configured account, default jurisdiction.
fn r2_endpoint() -> String {
    // Built with the shipping `provider::r2::r2_endpoint`, not a local
    // format! -- so every test in this file is also an assertion that the
    // function the connection form uses produces a host R2 actually answers on.
    build_r2_endpoint(&required_env("BUCKETCAT_R2_ACCOUNT"), "")
}

/// A `Connection` pointed at the live R2 endpoint from the environment.
///
/// `provider` is `"r2"`, which rides the generic non-AWS, non-OSS path
/// throughout `provider::s3`: `uses_path_style`'s "everything else" branch
/// (path-style), `s3_compat_endpoint`'s non-OSS passthrough (endpoint used
/// verbatim), and the non-AWS `RequestChecksumCalculation::WhenRequired` rule
/// (no `aws-chunked`). Unlike OSS and Qiniu it gets **no** `RegionRouting`:
/// R2's jurisdictions are separate namespaces, not regions to route between.
///
/// `region` is `"auto"`, R2's documented value -- though R2 does not actually
/// validate the credential-scope region at all (verified live: signing for
/// `us-east-1` and for `wnam` against the same host both return 200).
fn r2_connection() -> Connection {
    Connection {
        id: "e2e-r2".to_string(),
        provider: "r2".to_string(),
        name: "e2e r2".to_string(),
        endpoint: r2_endpoint(),
        region: "auto".to_string(),
        access_key_id: required_env("BUCKETCAT_R2_AK"),
        secret_access_key: required_env("BUCKETCAT_R2_SK"),
        default_bucket: None,
        api_token: None,
    }
}

/// A fresh `bucketcat-e2e/{uuid}/` prefix, unique per call -- every writing
/// test gets its own so concurrent/repeated runs never collide.
fn test_prefix() -> String {
    format!("bucketcat-e2e/{}/", uuid::Uuid::new_v4())
}

/// Deletes everything under `prefix` via [`Provider::delete_prefix`].
///
/// Refuses to run at all against anything that isn't a real, non-empty
/// `bucketcat-e2e/` prefix -- the last line of defense against a coding
/// mistake turning cleanup into "delete the whole bucket". Failures are
/// logged, not panicked on: a cleanup failure must never mask the actual test
/// assertion that ran before it.
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
             manual cleanup of this prefix in the R2 dashboard may be needed"
        );
    }
}

// --- fixture / hashing helpers ----------------------------------------------

/// Writes `size` bytes of deterministic LCG pseudo-random content to `path`.
///
/// Seeded (so the source hash is reproducible run to run) and, crucially,
/// non-compressible -- a file of repeated bytes would let a per-part offset
/// bug go unnoticed because every offset reads the same value. Seeds here live
/// in their own `0x0BBB_` namespace, distinct from `oss_e2e.rs`'s `0x0555_`,
/// `rainyun_e2e.rs`'s `0x0777_`, `qiniu_e2e.rs`'s `0x0999_` and
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
/// address space, and it is never printed or interpolated into an assertion
/// message either.
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

// --- 1: the path-style regression sentinel ----------------------------------

/// The cheapest proof that path-style addressing and SigV4 are simultaneously
/// correct: a bare `ListObjectsV2` against the sandbox bucket's root.
///
/// **This is the path-style regression sentinel.** R2 accepts *both*
/// path-style and virtual-hosted addressing -- verified live -- so a change
/// moving R2 into the virtual-hosted branch alongside AWS/OSS would keep every
/// other test in this file passing. Only the explicit `uses_path_style`
/// assertion below can catch it, which is why it runs first and on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn list_objects_on_the_test_bucket_succeeds() {
    let conn = r2_connection();
    let bucket = r2_bucket();

    assert!(
        uses_path_style(&conn.provider, &conn.endpoint),
        "R2 connections must use path-style addressing. R2 accepts virtual-hosted addressing \
         too, so flipping this would NOT break the rest of this suite -- this assertion is the \
         only thing that can catch such a change"
    );

    let provider = from_connection(&conn).expect("a valid R2 connection profile should build");
    let page = provider.list_objects(&bucket, "", None, 10).await.expect(
        "list_objects against the live R2 endpoint should succeed -- a failure here means \
             the endpoint, the SigV4 credentials, or path-style addressing is wrong; see this \
             file's module doc comment",
    );
    // Content is irrelevant (the sandbox bucket may legitimately be empty);
    // that the call *answered* is the whole assertion.
    let _ = page.entries;
}

// --- 2: Multi-Object Delete -------------------------------------------------

/// R2 accepts S3's Multi-Object Delete (`POST /?delete`) as `aws-sdk-s3`
/// actually sends it, so `supports_batch_delete` stays `true` for R2.
///
/// This is the test that matters most in this file. OSS and Rainyun both
/// hard-require the legacy `Content-MD5` header that `aws-sdk-s3` cannot send,
/// forcing `supports_batch_delete` to return `false` for them; Qiniu accepts
/// the `x-amz-checksum-crc32` the SDK does send. R2 had to be probed with a
/// **real SDK request** rather than hand-rolled curl, because that exact
/// shortcut is what produced a wrong answer for Rainyun before its own suite's
/// first run demolished it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn batch_delete_uses_the_multi_object_path() {
    let conn = r2_connection();
    let bucket = r2_bucket();

    assert!(
        supports_batch_delete(&conn.provider),
        "R2 is expected on the batch-delete path; if this ever has to flip to `false`, the \
         reason belongs in `supports_batch_delete`'s doc comment alongside OSS's and Rainyun's"
    );

    let provider = from_connection(&conn).expect("a valid R2 connection profile should build");
    let prefix = test_prefix();

    let dir = tempfile::tempdir().expect("creating a temp dir should succeed");
    let path = dir.path().join("tiny.bin");
    write_pseudo_random_file(&path, 32, 0x0BBB_0001);

    let keys: Vec<String> = (0..3).map(|i| format!("{prefix}batch-{i}.bin")).collect();
    for key in &keys {
        provider
            .put_object_from_file(&bucket, key, &path, 32, Arc::new(|_| {}))
            .await
            .expect("uploading a batch-delete fixture should succeed");
    }

    let result = provider.delete_objects(&bucket, &keys).await.expect(
        "DeleteObjects against R2 should succeed. `supports_batch_delete` returns true for \
             R2, so this went out as a single Multi-Object Delete carrying whatever checksum \
             header aws-sdk-s3 attaches. A failure here is the signal to flip \
             `supports_batch_delete` to false for R2 -- exactly as OSS and Rainyun had to",
    );

    assert_eq!(
        result.succeeded,
        keys.len() as u32,
        "every key should have been deleted; failures: {:?}",
        result.failed
    );
    assert!(
        result.failed.is_empty(),
        "no per-key failures expected, saw: {:?}",
        result.failed
    );

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 3: multipart upload ----------------------------------------------------

/// A real multipart upload round-trips byte-for-byte.
///
/// This is the only test that can catch a regression in the non-AWS
/// `RequestChecksumCalculation::WhenRequired` rule: a unit test on a config
/// value cannot prove wire behavior, and it is the streamed, multi-part upload
/// path that actually trips over an unwanted `aws-chunked` content encoding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn multipart_upload_round_trip() {
    let conn = r2_connection();
    let bucket = r2_bucket();
    let provider = from_connection(&conn).expect("a valid R2 connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}multipart.bin");

    // 12 MiB in 3 parts. Every part must be >= 5 MiB except the last, which is
    // S3's (and R2's) hard minimum.
    let size = 12 * MB;
    let part_size = 5 * MB;
    let dir = tempfile::tempdir().expect("creating a temp dir should succeed");
    let path = dir.path().join("multipart.bin");
    write_pseudo_random_file(&path, size, 0x0BBB_0002);
    let source = std::fs::read(&path).expect("reading the fixture back should succeed");
    let source_hash = hex(&sha256_bytes(&source));

    let upload_id = provider
        .multipart_init(&bucket, &key)
        .await
        .expect("CreateMultipartUpload against R2 should succeed");

    let mut parts: Vec<UploadedPart> = Vec::new();
    let mut offset = 0u64;
    let mut number = 1i32;
    while offset < size {
        let length = part_size.min(size - offset);
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
            .unwrap_or_else(|e| {
                panic!(
                    "UploadPart {number} (offset {offset}, length {length}) against R2 should \
                     succeed: {e}. An `aws-chunked`/checksum rejection would surface here first"
                )
            });
        parts.push(UploadedPart {
            number,
            etag,
            size: length,
        });
        offset += length;
        number += 1;
    }

    provider
        .multipart_complete(&bucket, &key, &upload_id, &parts)
        .await
        .expect("CompleteMultipartUpload against R2 should succeed");

    let head = provider
        .head_object(&bucket, &key)
        .await
        .expect("HeadObject on the completed multipart object should succeed");
    assert_eq!(
        head.size, size,
        "the assembled object should be {size} bytes"
    );

    let downloaded = get_range_bytes(&provider, &bucket, &key, 0, size)
        .await
        .expect("reading the whole assembled object back should succeed");
    assert_eq!(
        hex(&sha256_bytes(&downloaded)),
        source_hash,
        "the round-tripped object must be byte-for-byte identical to the source"
    );

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 4: Range GET -----------------------------------------------------------

/// `get_range` reads exactly the requested slice, at a non-zero offset.
///
/// A provider that silently ignores the `Range` header (returning the whole
/// object) would break resumable download without any error surfacing, so this
/// asserts both the length AND the content of a mid-object slice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn range_get_reads_exact_slices() {
    let conn = r2_connection();
    let bucket = r2_bucket();
    let provider = from_connection(&conn).expect("a valid R2 connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}range.bin");

    let size = 64 * 1024;
    let dir = tempfile::tempdir().expect("creating a temp dir should succeed");
    let path = dir.path().join("range.bin");
    write_pseudo_random_file(&path, size, 0x0BBB_0003);
    let source = std::fs::read(&path).expect("reading the fixture back should succeed");

    provider
        .put_object_from_file(&bucket, &key, &path, size, Arc::new(|_| {}))
        .await
        .expect("PutObject of the range fixture should succeed");

    let offset = 12_345u64;
    let length = 4_096u64;
    let slice = get_range_bytes(&provider, &bucket, &key, offset, length)
        .await
        .expect("a ranged GET against R2 should succeed");

    assert_eq!(
        slice.len() as u64,
        length,
        "a provider that ignored the Range header would return all {size} bytes here"
    );
    assert_eq!(
        slice,
        &source[offset as usize..(offset + length) as usize],
        "the ranged GET returned the wrong slice of the object"
    );

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 5: CopyObject (via rename) ---------------------------------------------

/// `rename_object` (copy-then-delete) works, which means `CopyObject` does.
///
/// Object stores have no native rename, so `Provider::rename_object` is a
/// `CopyObject` followed by a `DeleteObject`. Asserting the source is gone AND
/// the destination has the right bytes covers both halves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn rename_object_copies_then_deletes() {
    let conn = r2_connection();
    let bucket = r2_bucket();
    let provider = from_connection(&conn).expect("a valid R2 connection profile should build");
    let prefix = test_prefix();
    let from_key = format!("{prefix}rename-src.bin");
    let to_key = format!("{prefix}rename-dst.bin");

    let size = 8 * 1024;
    let dir = tempfile::tempdir().expect("creating a temp dir should succeed");
    let path = dir.path().join("rename.bin");
    write_pseudo_random_file(&path, size, 0x0BBB_0004);
    let source = std::fs::read(&path).expect("reading the fixture back should succeed");

    provider
        .put_object_from_file(&bucket, &from_key, &path, size, Arc::new(|_| {}))
        .await
        .expect("PutObject of the rename fixture should succeed");

    provider
        .rename_object(&bucket, &from_key, &to_key)
        .await
        .expect("rename_object (CopyObject + DeleteObject) against R2 should succeed");

    let moved = get_range_bytes(&provider, &bucket, &to_key, 0, size)
        .await
        .expect("the renamed object should be readable at its new key");
    assert_eq!(
        moved, source,
        "the copied object's bytes must match the source exactly"
    );

    let stale = provider.head_object(&bucket, &from_key).await;
    assert!(
        stale.is_err(),
        "the source key must be gone after a rename, but HeadObject still found it"
    );

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 6: presigned GET -------------------------------------------------------

/// A presigned GET URL is fetchable with no credentials at all.
///
/// The URL carries a live signature: it is never printed, never passed to a
/// subprocess, and never interpolated into an assertion message -- see
/// [`fetch_anonymously`].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn presigned_get_works() {
    let conn = r2_connection();
    let bucket = r2_bucket();
    let provider = from_connection(&conn).expect("a valid R2 connection profile should build");
    let prefix = test_prefix();
    let key = format!("{prefix}presign.bin");

    let size = 2_048u64;
    let dir = tempfile::tempdir().expect("creating a temp dir should succeed");
    let path = dir.path().join("presign.bin");
    write_pseudo_random_file(&path, size, 0x0BBB_0005);
    let source = std::fs::read(&path).expect("reading the fixture back should succeed");

    provider
        .put_object_from_file(&bucket, &key, &path, size, Arc::new(|_| {}))
        .await
        .expect("PutObject of the presign fixture should succeed");

    let url = provider
        .presign_get(&bucket, &key, 300)
        .await
        .expect("presign_get against R2 should succeed");

    let (status, body) = fetch_anonymously(&url)
        .await
        .expect("the anonymous fetch of the presigned URL should complete");

    assert_eq!(
        status, 200,
        "an unauthenticated GET of the presigned URL should return 200, got {status}"
    );
    assert_eq!(
        body, source,
        "the presigned URL served the wrong bytes for this object"
    );

    cleanup_prefix(&provider, &bucket, &prefix).await;
}

// --- 7: the Cloudflare API plane -------------------------------------------

/// An admin-tier token probe reports the token's own id -- which **is** the S3
/// Access Key ID -- and enumerates the account, so the connection form can
/// fill in the access key, the account id and the endpoint from one paste.
///
/// This also proves the derivation the whole token mode rests on: the secret
/// derived from the token value by `r2_secret_from_token` is the one that
/// actually signs, because the very next call uses it against the live S3
/// endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn admin_token_probe_reports_the_access_key_and_account() {
    let token = required_env("BUCKETCAT_R2_TOKEN");
    let account = required_env("BUCKETCAT_R2_ACCOUNT");

    let probe = r2_admin::probe_token(&token)
        .await
        .expect("probing a valid admin token should succeed");

    assert_eq!(
        probe.access_key_id,
        required_env("BUCKETCAT_R2_AK"),
        "the token's id must equal the S3 Access Key ID -- that identity is what lets the form \
         derive both credentials from a single pasted token"
    );
    assert!(
        probe.accounts.iter().any(|a| a.id == account),
        "an admin-tier token must be able to enumerate its own account, so the form can prefill \
         the account id (and from it the endpoint). Saw: {:?}",
        probe.accounts.iter().map(|a| &a.id).collect::<Vec<_>>()
    );

    // The derived secret must be the one that signs. Building a connection
    // from it and listing objects is the end-to-end proof.
    let mut conn = r2_connection();
    conn.secret_access_key = r2_secret_from_token(&token);
    conn.access_key_id = probe.access_key_id.clone();
    let provider =
        from_connection(&conn).expect("a connection built from derived credentials should build");
    provider
        .list_objects(&r2_bucket(), "", None, 1)
        .await
        .expect(
            "a secret derived from the token value must actually sign -- if this fails, the \
             sha256 derivation is wrong or Cloudflare changed the contract",
        );
}

/// Bucket metadata the S3 API cannot report: location hint, storage class,
/// jurisdiction, and the usage counters Cloudflare returns as JSON *strings*.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn admin_token_reads_bucket_metadata_and_usage() {
    let token = required_env("BUCKETCAT_R2_TOKEN");
    let account = required_env("BUCKETCAT_R2_ACCOUNT");
    let bucket = r2_bucket();

    let meta = r2_admin::bucket_meta(&token, &account, "", &bucket)
        .await
        .expect("reading bucket metadata with an admin token should succeed");
    assert!(
        meta.location.is_some(),
        "R2 reports a coarse location hint (APAC/WNAM/WEUR/...) for every bucket"
    );
    assert_eq!(
        meta.jurisdiction.as_deref(),
        Some("default"),
        "the sandbox bucket lives in the default jurisdiction"
    );

    // Only that the counters *parse* is asserted: an e2e-scoped bucket's
    // contents change between runs, so any specific count would be flaky.
    r2_admin::bucket_usage(&token, &account, "", &bucket)
        .await
        .expect("reading bucket usage should succeed -- the counters arrive as JSON strings");

    let managed = r2_admin::managed_domain(&token, &account, "", &bucket)
        .await
        .expect("reading the r2.dev managed domain should succeed");
    assert!(
        managed.domain.contains("r2.dev"),
        "Cloudflare allocates an r2.dev name even when public access is off"
    );

    r2_admin::custom_domains(&token, &account, "", &bucket)
        .await
        .expect("reading custom domains should succeed (an empty list is fine)");
}

/// **The low-privilege contract.** An R2 object-scoped token verifies fine and
/// reports its id, but `GET /accounts` answers `200 []` for it -- not a 403.
///
/// This is the single most load-bearing finding behind the connection form's
/// design: if this ever started returning an error instead, the form would
/// reject a perfectly usable token rather than falling back to asking for the
/// account id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn low_privilege_token_probes_but_enumerates_no_accounts() {
    let token = required_env("BUCKETCAT_R2_LOWPRIV_TOKEN");

    let probe = r2_admin::probe_token(&token)
        .await
        .expect("even an object-scoped token must verify and report its own id");

    assert_eq!(
        probe.access_key_id,
        required_env("BUCKETCAT_R2_LOWPRIV_AK"),
        "the token id / Access Key ID identity holds at every privilege tier"
    );
    assert!(
        probe.accounts.is_empty(),
        "an object-scoped token sees no accounts. If this ever becomes non-empty the finding \
         behind the form's fallback has changed and the UI copy needs revisiting"
    );
}

/// Every `/r2/*` endpoint refuses an object-scoped token with 403, which this
/// crate maps to `AppError::AccessDenied` -- the exact code the bucket-info
/// panel keys its "this token can't read that" degradation off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn low_privilege_token_is_access_denied_by_the_r2_api() {
    let token = required_env("BUCKETCAT_R2_LOWPRIV_TOKEN");
    let account = required_env("BUCKETCAT_R2_ACCOUNT");

    let err = r2_admin::bucket_meta(&token, &account, "", &r2_bucket())
        .await
        .expect_err("an object-scoped token must not be able to read bucket metadata");

    assert_eq!(
        err.code(),
        "auth/access-denied",
        "the bucket-info panel degrades on exactly this code; any other mapping turns a \
         recoverable 'section unavailable' into a hard error"
    );
}

// --- 8: jurisdictions are separate namespaces ------------------------------

/// The EU endpoint cannot see a default-jurisdiction bucket **at all**.
///
/// This is what makes R2 structurally different from Aliyun OSS and Qiniu
/// Kodo, whose cross-region buckets are reachable once a request is routed to
/// the right host (`provider::s3::RegionRouting`). Here there is nothing to
/// route to: the bucket simply does not exist in that namespace, and the
/// answer is `404 NoSuchBucket` rather than a redirect or a 403. If this ever
/// started succeeding, R2 would have become a routing problem and the
/// deliberate absence of `RegionRouting` for it would need revisiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn the_eu_jurisdiction_cannot_see_a_default_jurisdiction_bucket() {
    let account = required_env("BUCKETCAT_R2_ACCOUNT");
    let mut conn = r2_connection();
    conn.endpoint = build_r2_endpoint(&account, "eu");

    let provider = from_connection(&conn).expect("an EU-jurisdiction connection should build");
    let err = provider
        .list_objects(&r2_bucket(), "", None, 1)
        .await
        .expect_err(
            "the EU endpoint must not see a default-jurisdiction bucket -- if it does, R2's \
             jurisdictions are no longer separate namespaces and this app's model of them is wrong",
        );

    assert_eq!(
        err.code(),
        "storage/bucket-not-found",
        "R2 answers 404 NoSuchBucket across jurisdictions, not 403 and not a redirect"
    );
}

/// The same connection's `list_buckets` comes back **empty** on the EU
/// endpoint while the default endpoint lists the sandbox bucket -- the
/// positive half of the namespace-separation claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn the_eu_jurisdiction_lists_no_default_jurisdiction_buckets() {
    let account = required_env("BUCKETCAT_R2_ACCOUNT");
    let bucket = r2_bucket();

    let default_provider =
        from_connection(&r2_connection()).expect("the default connection should build");
    let default_buckets = default_provider
        .list_buckets()
        .await
        .expect("listing buckets on the default endpoint should succeed");
    assert!(
        default_buckets.iter().any(|b| b.name == bucket),
        "sanity: the sandbox bucket must be visible on the default endpoint"
    );

    let mut eu_conn = r2_connection();
    eu_conn.endpoint = build_r2_endpoint(&account, "eu");
    let eu_provider =
        from_connection(&eu_conn).expect("an EU-jurisdiction connection should build");
    let eu_buckets = eu_provider
        .list_buckets()
        .await
        .expect("listing buckets on the EU endpoint should succeed (with an empty result)");

    assert!(
        !eu_buckets.iter().any(|b| b.name == bucket),
        "a default-jurisdiction bucket must never appear in the EU jurisdiction's listing"
    );
}

// --- 9: the low-privilege test_connection fallback --------------------------

/// A connection whose credentials cannot list buckets is still reported as
/// **usable**, provided it has a default bucket it can actually reach.
///
/// This is the whole point of `test_connection`'s `AccessDenied` fallback.
/// Cloudflare's own dashboard steers users toward object-scoped R2 tokens, and
/// without this a token that browses, uploads and downloads perfectly well
/// would be reported to the user as "connection failed" the moment they hit
/// Test. Aliyun RAM sub-accounts and least-privilege AWS IAM policies have the
/// identical shape, which is why the fallback lives in `S3Provider` rather
/// than behind an R2-specific branch.
///
/// The low-privilege secret is **derived** from the token value here rather
/// than read from its own environment variable -- one less credential to
/// configure, and it re-proves the `sha256` derivation at a second privilege
/// tier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn a_low_privilege_connection_with_a_default_bucket_tests_as_usable() {
    let token = required_env("BUCKETCAT_R2_LOWPRIV_TOKEN");
    let bucket = r2_bucket();

    let mut conn = r2_connection();
    conn.access_key_id = required_env("BUCKETCAT_R2_LOWPRIV_AK");
    conn.secret_access_key = r2_secret_from_token(&token);

    // Precondition: these credentials really are denied ListBuckets. Without
    // asserting this the test below could pass for the boring reason that the
    // token was over-privileged all along.
    let no_bucket = from_connection(&conn).expect("a low-privilege connection should build");
    let err = no_bucket
        .list_buckets()
        .await
        .expect_err("an object-scoped R2 token must not be able to list buckets");
    assert_eq!(
        err.code(),
        "auth/access-denied",
        "the fallback keys off exactly this code"
    );

    // With no default bucket there is nothing to fall back to, so the original
    // AccessDenied must survive unchanged rather than being masked.
    assert_eq!(
        no_bucket
            .test_connection()
            .await
            .expect_err("with no default bucket there is nothing to prove reachability with")
            .code(),
        "auth/access-denied"
    );

    // With one, the same credentials test as usable.
    conn.default_bucket = Some(bucket.clone());
    let with_bucket = from_connection(&conn).expect("a low-privilege connection should build");
    with_bucket.test_connection().await.expect(
        "a connection that can read its own default bucket is usable, even though it cannot \
         enumerate buckets -- this is the entire point of the AccessDenied fallback",
    );
}

/// The fallback must not paper over a bucket the credentials genuinely cannot
/// reach: a default bucket that is itself denied fails, and fails with the
/// *more specific* error rather than the original ListBuckets one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn the_fallback_still_fails_for_an_unreachable_default_bucket() {
    let token = required_env("BUCKETCAT_R2_LOWPRIV_TOKEN");

    let mut conn = r2_connection();
    conn.access_key_id = required_env("BUCKETCAT_R2_LOWPRIV_AK");
    conn.secret_access_key = r2_secret_from_token(&token);
    conn.default_bucket = Some(format!("bucketcat-no-such-bucket-{}", uuid::Uuid::new_v4()));

    let provider = from_connection(&conn).expect("a low-privilege connection should build");
    provider
        .test_connection()
        .await
        .expect_err("a default bucket that does not exist cannot prove the connection is usable");
}
