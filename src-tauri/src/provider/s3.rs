//! AWS S3-compatible provider (admin plane): bucket list/create/delete plus
//! a lightweight connection check, backed by `aws-sdk-s3`.
//!
//! This module is the only place in the crate that touches `aws_sdk_s3`
//! types outside of [`crate::error`]'s `From<aws_sdk_s3::Error>` -- the
//! [`Provider`] trait and [`Bucket`] DTO in [`crate::provider`] keep every
//! other caller (in particular `crate::commands`) free of an SDK dependency.

use std::path::Path;

use async_trait::async_trait;
use aws_sdk_s3::config::{
    Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::DateTimeFormat;
use aws_sdk_s3::primitives::{ByteStream, Length};
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, Delete, ObjectIdentifier};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

use crate::error::{AppError, AppResult};
use crate::provider::{
    BatchResult, Bucket, FailedKey, ListPage, ObjectEntry, ObjectHead, Provider, UploadedPart,
};
use crate::store::Connection;

/// Returns true only for real AWS endpoints (`*.amazonaws.com`, matched on
/// whole hostname labels, case-insensitively). Every S3-compatible backend
/// this app targets -- MinIO, Cloudflare R2, self-hosted -- returns false
/// here and gets `force_path_style(true)`; virtual-hosted-style addressing
/// otherwise breaks for endpoints that don't support wildcard DNS for
/// bucket subdomains.
pub fn is_aws_endpoint(endpoint: &str) -> bool {
    let without_scheme = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    let authority = without_scheme.split('/').next().unwrap_or("");
    // Strip a trailing `:port` when everything after the last `:` is
    // digits. Deliberately simple (no IPv6-literal handling): a non-AWS
    // host that confuses this still correctly returns `false`, which is
    // the safe (path-style) default.
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    };
    let host = host.to_ascii_lowercase();
    host == "amazonaws.com" || host.ends_with(".amazonaws.com")
}

/// A constructed S3 (or S3-compatible) admin-plane client for one saved
/// [`Connection`].
#[derive(Debug)]
pub struct S3Provider {
    client: aws_sdk_s3::Client,
}

/// Builds an [`S3Provider`] from a saved connection profile.
///
/// - An empty `endpoint` is rejected before any SDK type is touched. This
///   maps to [`AppError::Internal`] rather than a `storage/*` code: a
///   `Connection` with no endpoint should never have made it past input
///   validation at the store/command boundary, so reaching here with one
///   signals a bug upstream, not a normal "storage" condition (like a
///   missing bucket) the user caused.
/// - An empty `region` defaults to `"us-east-1"` -- the same fallback
///   `aws-cli`/`boto3` apply when no region is configured -- but **only**
///   for non-AWS endpoints ([`is_aws_endpoint`] is false): MinIO/R2/
///   self-hosted backends routinely don't care about region at all. A
///   real AWS endpoint with an empty region is rejected the same way as
///   an empty endpoint (`AppError::Internal`): the UI is expected to
///   prefill a region for AWS connections, so an empty one reaching here
///   signals the same kind of upstream-validation gap, and silently
///   guessing `"us-east-1"` for a real AWS account risks pointing
///   requests at the wrong region instead of failing fast.
/// - `force_path_style` is the negation of [`is_aws_endpoint`]: MinIO, R2
///   and other self-hosted backends need path-style addressing.
/// - For every non-AWS endpoint, `request_checksum_calculation` and
///   `response_checksum_validation` are both explicitly set to
///   [`RequestChecksumCalculation::WhenRequired`] /
///   [`ResponseChecksumValidation::WhenRequired`] (design §2, 2026-07-23
///   revision, **mandatory**): aws-sdk-rust >=1.69 defaults both to
///   `WhenSupported`, which attaches a request checksum / demands response
///   checksum validation on every eligible operation -- Cloudflare R2,
///   older MinIO and Dell ECS all reject that with `NotImplemented`. A real
///   AWS endpoint keeps the SDK's own default (`WhenSupported`) untouched.
pub fn from_connection(conn: &Connection) -> AppResult<S3Provider> {
    if conn.endpoint.trim().is_empty() {
        return Err(AppError::Internal {
            message: "connection endpoint must not be empty".to_string(),
        });
    }

    let is_aws = is_aws_endpoint(&conn.endpoint);

    let region = if conn.region.trim().is_empty() {
        if is_aws {
            return Err(AppError::Internal {
                message: "connection region must not be empty for an AWS endpoint".to_string(),
            });
        }
        "us-east-1".to_string()
    } else {
        conn.region.clone()
    };

    let credentials = Credentials::new(
        conn.access_key_id.clone(),
        conn.secret_access_key.clone(),
        None,
        None,
        "bucketcat",
    );

    let mut config_builder = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .endpoint_url(conn.endpoint.clone())
        .region(Region::new(region))
        .credentials_provider(credentials)
        .force_path_style(!is_aws);

    // See the `from_connection` doc comment: non-AWS endpoints must not get
    // the SDK's `WhenSupported` default, which breaks R2/older MinIO/Dell
    // ECS with `NotImplemented`. Real AWS endpoints are left on the SDK
    // default.
    if !is_aws {
        config_builder = config_builder
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
    }

    let config = config_builder.build();

    Ok(S3Provider {
        client: aws_sdk_s3::Client::from_conf(config),
    })
}

/// Classifies an [`SdkError`] into an [`AppError`] *before* it collapses
/// into `aws_sdk_s3::Error`, recovering the network-cause fidelity that
/// conversion throws away (see the doc comment on
/// `impl From<aws_sdk_s3::Error> for AppError` in `crate::error`).
///
/// Returns `None` for `SdkError::ServiceError` (and any future
/// `#[non_exhaustive]` variant this crate doesn't yet know about) so the
/// caller falls through to the existing `aws_sdk_s3::Error` conversion,
/// which is where modeled S3 errors like `NoSuchBucket` /
/// `BucketAlreadyExists` get their structured detail. Pure and
/// network-free: every branch is exercised in `tests` below by
/// constructing `SdkError` values directly via its public constructors,
/// with no real request involved.
fn classify_sdk_error<E, R>(err: &SdkError<E, R>) -> Option<AppError> {
    match err {
        SdkError::TimeoutError(_) => Some(AppError::Timeout),
        // `DispatchFailure` wraps a `ConnectorError`; only its `is_timeout`
        // kind maps to `Timeout`, everything else (io/user/other) never
        // reached the network successfully, so it's `Unreachable`.
        SdkError::DispatchFailure(failure) => Some(if failure.is_timeout() {
            AppError::Timeout
        } else {
            AppError::Unreachable
        }),
        // The request never made it onto the wire (e.g. the endpoint
        // resolver rejected the configured `endpoint_url`); there is no
        // response to inspect, so this is a connectivity problem from the
        // caller's point of view.
        SdkError::ConstructionFailure(_) => Some(AppError::Unreachable),
        // A response came back but couldn't be parsed as this protocol --
        // typically because the endpoint isn't actually an S3-compatible
        // server. Also a connectivity/target problem, not a modeled S3
        // error.
        SdkError::ResponseError(_) => Some(AppError::Unreachable),
        SdkError::ServiceError(_) => None,
        _ => None,
    }
}

/// Maps a raw S3 error *code* (as read off [`ProvideErrorMetadata::code`])
/// to an [`AppError`], recovering detail for codes the SDK's generated
/// per-operation error enum doesn't model as a distinct variant for some
/// operations. `ListBucketsError` and `DeleteBucketError` model *nothing*
/// but `Unhandled` (unlike e.g. `CreateBucketError`, which models
/// `BucketAlreadyExists`/`BucketAlreadyOwnedByYou`) -- so a wrong secret
/// key (`SignatureDoesNotMatch`) on `test_connection`/`list_buckets`, or a
/// delete against a bucket that's already gone (`NoSuchBucket`) on
/// `delete_bucket`, both erase to `aws_sdk_s3::Error::Unhandled` and, with
/// nothing to recover them, to `AppError::Internal` -- even though the
/// server's raw response said exactly what went wrong. Confirmed against
/// live MinIO by `tests/minio_e2e.rs`'s `wrong_secret_...` test, which
/// previously had to pin `internal` as a documented gap.
///
/// Only consulted from [`normalize_s3_error`] as the last fallback tier,
/// *after* the existing modeled-variant/`aws_sdk_s3::Error` mapping has had
/// its turn -- i.e. only for cases that would otherwise become
/// `AppError::Internal`. A real modeled variant (already a non-`Internal`
/// `AppError`) is never second-guessed by this.
///
/// Pure string -> variant mapping, deliberately factored out (rather than
/// threaded through `SdkError`'s generics) so it's directly testable with
/// plain string literals, no SDK error construction required.
fn classify_error_code(code: &str, message: Option<&str>) -> Option<AppError> {
    match code {
        "SignatureDoesNotMatch" | "InvalidAccessKeyId" => Some(AppError::InvalidCredentials),
        "AccessDenied" | "AccessDeniedException" => Some(AppError::AccessDenied),
        "NoSuchBucket" => Some(AppError::BucketNotFound {
            bucket: message.unwrap_or("unknown").to_string(),
        }),
        // "NotFound" is what `HeadObject` synthesizes on a 404: that
        // request has no XML body to carry a real S3 error code, unlike
        // `GetObject`/`DeleteObject`'s `NoSuchKey`, so `aws-sdk-s3` fills in
        // this placeholder code instead. Both mean the same thing to the
        // caller: the key doesn't exist.
        "NoSuchKey" | "NotFound" => Some(AppError::KeyNotFound {
            key: message.unwrap_or("unknown").to_string(),
        }),
        _ => None,
    }
}

/// Display name for a key: the final path segment ("docs/2026/" -> "2026",
/// "docs/readme.md" -> "readme.md"). Pure, unit-tested below.
pub(crate) fn display_name(key: &str, is_prefix: bool) -> String {
    let trimmed = if is_prefix {
        key.trim_end_matches('/')
    } else {
        key
    };
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

/// Maps one `aws_sdk_s3::types::Object` row into a file [`ObjectEntry`]
/// (`is_prefix` always `false`). Returns `None` when the object has no key
/// (the SDK models it as optional; in practice ListObjectsV2 always sets
/// it). Pure field mapping only -- callers that need to filter out a
/// folder's own zero-byte marker object (as [`to_list_page`] does) do that
/// filtering themselves before calling this. Shared by [`to_list_page`] and
/// [`S3Provider::list_objects_flat`] so the mapping exists in exactly one
/// place.
fn object_to_entry(obj: &aws_sdk_s3::types::Object) -> Option<ObjectEntry> {
    let key = obj.key()?;
    Some(ObjectEntry {
        key: key.to_string(),
        name: display_name(key, false),
        size: obj.size().and_then(|s| u64::try_from(s).ok()),
        last_modified: obj
            .last_modified()
            .and_then(|d| d.fmt(DateTimeFormat::DateTime).ok()),
        storage_class: obj.storage_class().map(|c| c.as_str().to_string()),
        is_prefix: false,
    })
}

/// Maps one ListObjectsV2 response page into the domain [`ListPage`]:
/// common prefixes become `is_prefix` entries (listed first), objects
/// become file entries, and the zero-byte folder-marker object whose key
/// equals the listed `prefix` itself (created by `create_folder`) is
/// filtered out -- but *only* when `prefix` denotes a folder (ends with
/// `/`). The search box (design §6) feeds arbitrary typed text straight
/// into `prefix`, so typing a file's complete name (e.g. `"readme.md"`)
/// makes that file's key equal `prefix` too; filtering it out in that case
/// would make an exact-name search vanish the very file it matched. Pure —
/// unit-tested against builder-constructed outputs, no network.
fn to_list_page(
    output: &aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output,
    prefix: &str,
) -> ListPage {
    let mut entries: Vec<ObjectEntry> = Vec::new();
    for cp in output.common_prefixes() {
        let Some(key) = cp.prefix() else { continue };
        entries.push(ObjectEntry {
            key: key.to_string(),
            name: display_name(key, true),
            size: None,
            last_modified: None,
            storage_class: None,
            is_prefix: true,
        });
    }
    for obj in output.contents() {
        let Some(key) = obj.key() else { continue };
        if key == prefix && prefix.ends_with('/') {
            continue;
        }
        if let Some(entry) = object_to_entry(obj) {
            entries.push(entry);
        }
    }
    ListPage {
        entries,
        next_token: output.next_continuation_token().map(str::to_string),
    }
}

/// DeleteObjects accepts at most 1000 keys per request (S3 hard limit);
/// larger batches are split client-side.
const DELETE_BATCH_MAX: usize = 1000;

/// Keys per page while `delete_prefix` walks a subtree. 1000 is
/// ListObjectsV2's own server-side maximum, so this minimizes the number of
/// round-trips needed to enumerate a large folder before deleting it.
const FLAT_WALK_PAGE_SIZE: i32 = 1000;

/// Splits `keys` into consecutive sub-slices of at most `size` elements, the
/// final chunk carrying the remainder. `size` must be non-zero (the only
/// caller passes the compile-time constant [`DELETE_BATCH_MAX`]). Extracted
/// as a pure fn so `delete_prefix`'s ≤1000 batching is unit-testable without
/// a live store. Pure, unit-tested below.
pub(crate) fn chunk_keys(keys: &[String], size: usize) -> Vec<&[String]> {
    keys.chunks(size).collect()
}

/// Whether `prefix` is safe to hand to a recursive `delete_prefix`. Requires a
/// folder-shaped, `/`-terminated prefix: an empty one would wipe the whole
/// bucket, and a non-slash one (`doc`) would match and delete siblings
/// (`docs/…`, `document.txt`) under the delimiter-less walk. Pure so the
/// destructive boundary's guard is unit-tested rather than trusted.
pub(crate) fn is_deletable_prefix(prefix: &str) -> bool {
    prefix.ends_with('/')
}

/// Characters percent-encoded in an `x-amz-copy-source` value: everything
/// except RFC 3986 unreserved characters and `/` (the bucket/key
/// separator). The SDK does NOT encode this header itself, and unencoded
/// spaces/`+`/non-ASCII (e.g. Chinese object names) break the request
/// signature.
const COPY_SOURCE_ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'/');

/// `bucket/key` with the key percent-encoded for `x-amz-copy-source`.
/// Pure, unit-tested below.
pub(crate) fn encode_copy_source(bucket: &str, key: &str) -> String {
    format!(
        "{}/{}",
        bucket,
        utf8_percent_encode(key, COPY_SOURCE_ENCODE)
    )
}

/// Normalizes a folder prefix to the zero-byte marker-object key
/// `"<trimmed>/"`. Rejects empty/blank prefixes and empty inner segments
/// (`a//b`) as `AppError::Internal` — the UI validates names before
/// calling, so reaching here with one is an upstream bug, mirroring
/// `from_connection`'s empty-endpoint rationale. Pure, unit-tested below.
pub(crate) fn folder_marker_key(prefix: &str) -> AppResult<String> {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.is_empty() || trimmed.split('/').any(|segment| segment.trim().is_empty()) {
        return Err(AppError::Internal {
            message: "folder prefix must be non-empty with no empty path segments".to_string(),
        });
    }
    Ok(format!("{trimmed}/"))
}

/// Maps one per-key DeleteObjects error into a [`FailedKey`], reusing
/// [`classify_error_code`] so the `code` is an `AppError`-style i18n code
/// (`auth/access-denied`, `storage/key-not-found`, ...), falling back to
/// the same "unclassified" code [`AppError::Internal`] itself reports, so
/// this can never drift from that mapping. Pure, unit-tested below.
fn failed_key(err: &aws_sdk_s3::types::Error) -> FailedKey {
    let code = err
        .code()
        .and_then(|c| classify_error_code(c, err.message()))
        .map(|e| e.code().to_string())
        .unwrap_or_else(|| {
            AppError::Internal {
                message: String::new(),
            }
            .code()
            .to_string()
        });
    FailedKey {
        key: err.key().unwrap_or_default().to_string(),
        code,
    }
}

/// Converts a whole-chunk `DeleteObjects` request failure -- a
/// network/service error that rejected the *entire* chunk before S3 could
/// even report which individual keys succeeded or failed -- into one
/// [`FailedKey`] per key in that chunk, all carrying the same normalized
/// `err.code()`. This is what lets `delete_objects` keep going after a
/// mid-batch chunk failure (design §7: never abort the whole batch)
/// instead of losing track of which keys in the failing chunk were never
/// even attempted. Pure, unit-tested below.
fn chunk_failure_keys(chunk: &[String], err: &AppError) -> Vec<FailedKey> {
    let code = err.code().to_string();
    chunk
        .iter()
        .map(|key| FailedKey {
            key: key.clone(),
            code: code.clone(),
        })
        .collect()
}

/// Normalizes any of the three bucket-operation `SdkError`s into an
/// [`AppError`] in three tiers:
///
/// 1. [`classify_sdk_error`] -- network-cause fidelity for anything that
///    isn't a real, parsed service response.
/// 2. `aws_sdk_s3::Error`'s own `From` impl (see `crate::error`) -- modeled
///    per-operation variants (e.g. `BucketAlreadyExists`) first preference,
///    its own keyword sniff second.
/// 3. If (2) still lands on `AppError::Internal`, [`classify_error_code`]
///    gets one more attempt using the *raw* error code/message read off
///    the pre-erasure operation error (`SdkError::ServiceError`'s `E`, via
///    [`ProvideErrorMetadata`], captured before `err` is consumed by the
///    tier-2 conversion) -- this is what recovers e.g.
///    `SignatureDoesNotMatch` on `ListBuckets`/`DeleteBucket`, whose
///    generated error enums don't model anything but `Unhandled` and so
///    never reach tier 2's modeled-variant arms.
fn normalize_s3_error<E, R>(err: SdkError<E, R>) -> AppError
where
    E: ProvideErrorMetadata,
    aws_sdk_s3::Error: From<SdkError<E, R>>,
{
    if let Some(app_err) = classify_sdk_error(&err) {
        return app_err;
    }

    // At this point `err` is `SdkError::ServiceError` (the only case
    // `classify_sdk_error` returns `None` for, aside from a hypothetical
    // future `#[non_exhaustive]` variant it also can't classify -- for
    // which `code`/`message` below are simply `None`, a no-op for tier 3).
    let (code, message) = match &err {
        SdkError::ServiceError(ctx) => (
            ctx.err().code().map(str::to_string),
            ctx.err().message().map(str::to_string),
        ),
        _ => (None, None),
    };

    let mapped = AppError::from(aws_sdk_s3::Error::from(err));
    if matches!(mapped, AppError::Internal { .. }) {
        if let Some(app_err) = code
            .as_deref()
            .and_then(|c| classify_error_code(c, message.as_deref()))
        {
            return app_err;
        }
    }
    mapped
}

/// Builds a [`AppError::FileIo`] naming the offending path. Factored out so
/// every local-file failure in this module reports the same shape.
fn file_io_error(path: &Path, message: impl std::fmt::Display) -> AppError {
    AppError::FileIo {
        path: path.display().to_string(),
        message: message.to_string(),
    }
}

/// The parts in the order `CompleteMultipartUpload` requires (ascending by
/// part number). Concurrency finishes parts out of order, and S3 answers an
/// unsorted list with `InvalidPartOrder`.
///
/// Called by [`S3Provider::multipart_complete`] -- deliberately not a
/// test-only mirror of that sort, so a regression in the ordering shows up
/// in this module's unit tests rather than only against a live server.
pub(crate) fn sorted_parts(parts: &[UploadedPart]) -> Vec<&UploadedPart> {
    let mut ordered: Vec<&UploadedPart> = parts.iter().collect();
    ordered.sort_unstable_by_key(|p| p.number);
    ordered
}

/// Opens `path[offset .. offset + length]` as a request body.
///
/// `read_from().path(..)` (rather than `.file(..)`) is deliberate: it keeps
/// the stream **rewindable**, so the SDK can replay the body during its own
/// internal retries. Handing over an already-open `File` produces a
/// one-shot stream, and a retried request would send an empty body.
///
/// `Length::Exact` also doubles as an integrity check: if the file shrank
/// since the plan was computed, this fails here rather than silently
/// uploading a short part.
async fn body_range(path: &Path, offset: u64, length: u64) -> AppResult<ByteStream> {
    ByteStream::read_from()
        .path(path)
        .offset(offset)
        .length(Length::Exact(length))
        .build()
        .await
        .map_err(|err| file_io_error(path, err))
}

/// An inclusive HTTP `Range` header value for `[offset, offset+length)`.
/// HTTP ranges are inclusive on both ends, so the last byte is
/// `offset + length - 1`.
pub fn range_header(offset: u64, length: u64) -> String {
    // Inclusive upper bound. `length` is never 0 in practice (`get_range`
    // short-circuits that case before calling this), but guard the
    // subtraction so a stray 0 can't underflow `u64`: an empty range
    // degenerates to the single start byte, which is never actually sent.
    let end = offset + length.saturating_sub(1);
    format!("bytes={}-{}", offset, end)
}

#[async_trait]
impl Provider for S3Provider {
    async fn test_connection(&self) -> AppResult<()> {
        self.list_buckets().await.map(|_| ())
    }

    async fn list_buckets(&self) -> AppResult<Vec<Bucket>> {
        let output = self
            .client
            .list_buckets()
            .send()
            .await
            .map_err(normalize_s3_error)?;

        Ok(output
            .buckets()
            .iter()
            .map(|b| Bucket {
                name: b.name().unwrap_or_default().to_string(),
                creation_date: b
                    .creation_date()
                    .and_then(|d| d.fmt(DateTimeFormat::DateTime).ok()),
            })
            .collect())
    }

    async fn create_bucket(&self, name: &str) -> AppResult<()> {
        self.client
            .create_bucket()
            .bucket(name)
            .send()
            .await
            .map_err(normalize_s3_error)?;
        Ok(())
    }

    async fn delete_bucket(&self, name: &str) -> AppResult<()> {
        self.client
            .delete_bucket()
            .bucket(name)
            .send()
            .await
            .map_err(normalize_s3_error)?;
        Ok(())
    }

    async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        token: Option<&str>,
        max_keys: i32,
    ) -> AppResult<ListPage> {
        let mut request = self
            .client
            .list_objects_v2()
            .bucket(bucket)
            .delimiter("/")
            .max_keys(max_keys);
        if !prefix.is_empty() {
            request = request.prefix(prefix);
        }
        if let Some(token) = token {
            request = request.continuation_token(token);
        }
        let output = request.send().await.map_err(normalize_s3_error)?;
        Ok(to_list_page(&output, prefix))
    }

    async fn delete_objects(&self, bucket: &str, keys: &[String]) -> AppResult<BatchResult> {
        let mut succeeded: u32 = 0;
        let mut failed: Vec<FailedKey> = Vec::new();
        for chunk in keys.chunks(DELETE_BATCH_MAX) {
            let identifiers = chunk
                .iter()
                .map(|key| ObjectIdentifier::builder().key(key).build())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| AppError::Internal {
                    message: format!("invalid delete key: {e}"),
                })?;
            let delete = Delete::builder()
                .set_objects(Some(identifiers))
                .quiet(true)
                .build()
                .map_err(|e| AppError::Internal {
                    message: format!("failed to build delete request: {e}"),
                })?;
            // A whole-chunk failure here (network blip, expired creds mid-
            // batch, ...) must NOT abort the loop or discard whatever
            // `succeeded`/`failed` were already accumulated from earlier
            // chunks (design §7) -- so this is deliberately NOT `?`. Every
            // key in the failing chunk is instead recorded as failed via
            // `chunk_failure_keys`, and the loop moves on to the next
            // chunk.
            match self
                .client
                .delete_objects()
                .bucket(bucket)
                .delete(delete)
                .send()
                .await
            {
                Ok(output) => {
                    let errors = output.errors();
                    succeeded += (chunk.len() - errors.len()) as u32;
                    failed.extend(errors.iter().map(failed_key));
                }
                Err(e) => {
                    failed.extend(chunk_failure_keys(chunk, &normalize_s3_error(e)));
                }
            }
        }
        Ok(BatchResult { succeeded, failed })
    }

    async fn delete_prefix(&self, bucket: &str, prefix: &str) -> AppResult<BatchResult> {
        // Harden the destructive boundary here, not just upstream: require a
        // folder-shaped (`/`-terminated) prefix. An empty prefix would
        // enumerate and delete every object in the bucket; a non-slash prefix
        // like `doc` would match and delete siblings `docs/…`, `docs2/…`,
        // `document.txt` under the delimiter-less walk. The only deliberate
        // "empty the bucket" path is the separately-guarded delete-bucket flow,
        // and the frontend only ever passes a real `prefix/` folder key, so
        // anything else reaching here is an upstream bug -- mapped to `Internal`
        // (like `folder_marker_key`/`from_connection`) rather than a storage
        // condition the user caused.
        if !is_deletable_prefix(prefix) {
            return Err(AppError::Internal {
                message: "delete_prefix requires a folder prefix ending in '/'".to_string(),
            });
        }

        // Walk the entire subtree with the delimiter-less listing,
        // accumulating every key -- INCLUDING the `prefix/` zero-byte
        // folder-marker object, which `list_objects_flat` returns as a real
        // row (unlike the delimiter-`/` `list_objects`, which rolls it into a
        // CommonPrefix and filters it). Surfacing that marker is exactly what
        // makes an *empty* in-app folder deletable (closes M3 gap I4).
        let mut keys: Vec<String> = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self
                .list_objects_flat(bucket, prefix, token.as_deref(), FLAT_WALK_PAGE_SIZE)
                .await?;
            keys.extend(page.entries.into_iter().map(|entry| entry.key));
            match page.next_token {
                Some(next) => token = Some(next),
                None => break,
            }
        }

        // Fold every ≤1000-key batch's outcome into a single BatchResult:
        // sum successes, concatenate per-key failures. Each `chunk` is
        // already ≤1000, so `delete_objects` issues exactly one DeleteObjects
        // request per chunk, and its own design-§7 handling turns a
        // whole-chunk request failure into per-key failures rather than
        // aborting the walk.
        let mut succeeded: u32 = 0;
        let mut failed: Vec<FailedKey> = Vec::new();
        for chunk in chunk_keys(&keys, DELETE_BATCH_MAX) {
            let batch = self.delete_objects(bucket, chunk).await?;
            succeeded += batch.succeeded;
            failed.extend(batch.failed);
        }
        Ok(BatchResult { succeeded, failed })
    }

    async fn rename_object(&self, bucket: &str, from_key: &str, to_key: &str) -> AppResult<()> {
        if from_key.is_empty() || to_key.is_empty() || from_key == to_key {
            return Err(AppError::Internal {
                message: "rename requires distinct, non-empty source and target keys".to_string(),
            });
        }
        self.client
            .copy_object()
            .bucket(bucket)
            .copy_source(encode_copy_source(bucket, from_key))
            .key(to_key)
            .send()
            .await
            .map_err(normalize_s3_error)?;
        self.client
            .delete_object()
            .bucket(bucket)
            .key(from_key)
            .send()
            .await
            .map_err(normalize_s3_error)?;
        Ok(())
    }

    async fn create_folder(&self, bucket: &str, prefix: &str) -> AppResult<()> {
        let key = folder_marker_key(prefix)?;
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(b""))
            .send()
            .await
            .map_err(normalize_s3_error)?;
        Ok(())
    }

    async fn put_object_from_file(
        &self,
        bucket: &str,
        key: &str,
        path: &Path,
        length: u64,
    ) -> AppResult<()> {
        let body = body_range(path, 0, length).await?;
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(body)
            .send()
            .await
            .map_err(normalize_s3_error)?;
        Ok(())
    }

    async fn multipart_init(&self, bucket: &str, key: &str) -> AppResult<String> {
        let out = self
            .client
            .create_multipart_upload()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(normalize_s3_error)?;

        out.upload_id()
            .map(str::to_string)
            .ok_or_else(|| AppError::Internal {
                message: "server accepted CreateMultipartUpload but returned no upload id"
                    .to_string(),
            })
    }

    async fn upload_part_from_file(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: i32,
        path: &Path,
        offset: u64,
        length: u64,
    ) -> AppResult<String> {
        let body = body_range(path, offset, length).await?;
        let out = self
            .client
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(part_number)
            .body(body)
            .send()
            .await
            .map_err(normalize_s3_error)?;

        out.e_tag()
            .map(str::to_string)
            .ok_or_else(|| AppError::Internal {
                message: format!("server accepted part {part_number} but returned no ETag"),
            })
    }

    async fn multipart_complete(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        parts: &[UploadedPart],
    ) -> AppResult<()> {
        let ordered = sorted_parts(parts);

        let completed: Vec<CompletedPart> = ordered
            .iter()
            .map(|p| {
                CompletedPart::builder()
                    .part_number(p.number)
                    .e_tag(&p.etag)
                    .build()
            })
            .collect();

        self.client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(completed))
                    .build(),
            )
            .send()
            .await
            .map_err(normalize_s3_error)?;
        Ok(())
    }

    async fn multipart_abort(&self, bucket: &str, key: &str, upload_id: &str) -> AppResult<()> {
        self.client
            .abort_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
            .map_err(normalize_s3_error)?;
        Ok(())
    }

    async fn multipart_list(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> AppResult<Vec<UploadedPart>> {
        let mut out = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_parts()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id);
            if let Some(m) = &marker {
                req = req.part_number_marker(m);
            }
            let resp = req.send().await.map_err(normalize_s3_error)?;
            for p in resp.parts() {
                out.push(UploadedPart {
                    number: p.part_number().unwrap_or_default(),
                    etag: p.e_tag().unwrap_or_default().to_string(),
                    size: p.size().unwrap_or_default().max(0) as u64,
                });
            }
            if resp.is_truncated().unwrap_or(false) {
                marker = resp.next_part_number_marker().map(|s| s.to_string());
            } else {
                break;
            }
        }
        Ok(out)
    }

    async fn head_object(&self, bucket: &str, key: &str) -> AppResult<ObjectHead> {
        let out = self
            .client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(normalize_s3_error)?;
        Ok(ObjectHead {
            // A HeadObject with no content length is nonsensical for a real
            // object; treat a missing one as zero rather than failing.
            size: out.content_length().unwrap_or(0).max(0) as u64,
            etag: out.e_tag().map(str::to_string),
            content_type: out.content_type().map(str::to_string),
        })
    }

    async fn get_range(
        &self,
        bucket: &str,
        key: &str,
        offset: u64,
        length: u64,
    ) -> AppResult<Vec<u8>> {
        // A zero-length range is malformed HTTP (and `range_header` would
        // have to guard against underflowing `offset + length - 1`); a
        // 0-byte object is a real case (a single zero-length chunk from
        // `plan_upload(0)`), so answer it here without a request.
        if length == 0 {
            return Ok(Vec::new());
        }
        let out = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .range(range_header(offset, length))
            .send()
            .await
            .map_err(normalize_s3_error)?;
        let data = out.body.collect().await.map_err(|err| AppError::Internal {
            message: format!("failed to read object body: {err}"),
        })?;
        Ok(data.into_bytes().to_vec())
    }

    async fn list_objects_flat(
        &self,
        bucket: &str,
        prefix: &str,
        token: Option<&str>,
        max_keys: i32,
    ) -> AppResult<ListPage> {
        let mut req = self
            .client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(prefix)
            .max_keys(max_keys);
        // No `.delimiter("/")` -- that omission is the whole point: this walks
        // the entire subtree, where `list_objects` stops at each folder.
        if let Some(token) = token {
            req = req.continuation_token(token);
        }
        let out = req.send().await.map_err(normalize_s3_error)?;
        // Reuse the existing object-row mapper, but every row here is a real
        // object (never a common prefix), so pass an empty prefix list.
        let entries = out.contents().iter().filter_map(object_to_entry).collect();
        Ok(ListPage {
            entries,
            next_token: out.next_continuation_token().map(str::to_string),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::error::ErrorMetadata;

    fn aws_connection() -> Connection {
        Connection {
            id: "c1".to_string(),
            provider: "aws".to_string(),
            name: "prod".to_string(),
            endpoint: "https://s3.amazonaws.com".to_string(),
            region: "us-east-1".to_string(),
            access_key_id: "AKIAEXAMPLE".to_string(),
            secret_access_key: "secret".to_string(),
            default_bucket: None,
        }
    }

    fn minio_connection() -> Connection {
        Connection {
            id: "c2".to_string(),
            provider: "minio".to_string(),
            name: "local".to_string(),
            endpoint: "http://localhost:9000".to_string(),
            region: "".to_string(),
            access_key_id: "minioadmin".to_string(),
            secret_access_key: "minioadmin".to_string(),
            default_bucket: None,
        }
    }

    // --- is_aws_endpoint -------------------------------------------------

    #[test]
    fn recognizes_bare_aws_host() {
        assert!(is_aws_endpoint("s3.amazonaws.com"));
    }

    #[test]
    fn recognizes_scheme_prefixed_aws_host() {
        assert!(is_aws_endpoint("https://s3.amazonaws.com"));
    }

    #[test]
    fn recognizes_regional_aws_host() {
        assert!(is_aws_endpoint("https://s3.us-west-2.amazonaws.com"));
    }

    #[test]
    fn is_case_insensitive() {
        assert!(is_aws_endpoint("https://S3.AMAZONAWS.COM"));
    }

    #[test]
    fn rejects_localhost() {
        assert!(!is_aws_endpoint("http://localhost:9000"));
    }

    #[test]
    fn rejects_r2() {
        assert!(!is_aws_endpoint("https://abc123.r2.cloudflarestorage.com"));
    }

    #[test]
    fn rejects_lookalike_host_without_label_boundary() {
        // "evil-amazonaws.com" ends with the substring "amazonaws.com" but
        // is not a subdomain of it -- must not match.
        assert!(!is_aws_endpoint("https://evil-amazonaws.com"));
    }

    #[test]
    fn rejects_suffix_spoofed_host() {
        // "s3.amazonaws.com.evil.com" *starts* with a real AWS hostname but
        // the actual (right-most) hostname label is "evil.com" -- must not
        // match, since only a hostname *ending in* ".amazonaws.com" is AWS.
        assert!(!is_aws_endpoint("s3.amazonaws.com.evil.com"));
    }

    #[test]
    fn rejects_empty_string() {
        assert!(!is_aws_endpoint(""));
    }

    // --- from_connection ---------------------------------------------------

    #[test]
    fn empty_endpoint_is_rejected() {
        let mut conn = aws_connection();
        conn.endpoint = "".to_string();

        let err = from_connection(&conn).unwrap_err();

        assert_eq!(err.code(), "internal");
    }

    #[test]
    fn whitespace_only_endpoint_is_rejected() {
        let mut conn = aws_connection();
        conn.endpoint = "   ".to_string();

        assert!(from_connection(&conn).is_err());
    }

    #[test]
    fn valid_aws_connection_builds_successfully() {
        assert!(from_connection(&aws_connection()).is_ok());
    }

    #[test]
    fn valid_minio_connection_builds_successfully() {
        assert!(from_connection(&minio_connection()).is_ok());
    }

    #[test]
    fn non_aws_empty_region_defaults_to_us_east_1() {
        let conn = minio_connection();
        assert_eq!(conn.region, "");

        let provider = from_connection(&conn).unwrap();

        assert_eq!(
            provider.client.config().region().map(|r| r.as_ref()),
            Some("us-east-1")
        );
    }

    #[test]
    fn aws_endpoint_with_empty_region_is_rejected() {
        let mut conn = aws_connection();
        conn.region = "".to_string();

        let err = from_connection(&conn).unwrap_err();

        assert_eq!(err.code(), "internal");
    }

    #[test]
    fn aws_endpoint_with_whitespace_only_region_is_rejected() {
        let mut conn = aws_connection();
        conn.region = "   ".to_string();

        assert!(from_connection(&conn).is_err());
    }

    // --- checksum config (design §2, 2026-07-23 revision) -------------------
    //
    // `aws_sdk_s3::Config` exposes public getters for both settings
    // (`Config::request_checksum_calculation` / `::response_checksum_validation`,
    // verified against the vendored aws-sdk-s3 1.139.0 source), so this is
    // asserted directly rather than only smoke-tested.

    #[test]
    fn non_aws_endpoint_forces_checksum_when_required() {
        let provider = from_connection(&minio_connection()).unwrap();
        let config = provider.client.config();

        assert_eq!(
            config.request_checksum_calculation(),
            Some(&RequestChecksumCalculation::WhenRequired)
        );
        assert_eq!(
            config.response_checksum_validation(),
            Some(&ResponseChecksumValidation::WhenRequired)
        );
    }

    #[test]
    fn aws_endpoint_leaves_checksum_config_on_sdk_default() {
        let provider = from_connection(&aws_connection()).unwrap();
        let config = provider.client.config();

        // Deliberately unset (not explicitly `WhenSupported`) -- an AWS
        // endpoint must fall through to whatever the SDK's own default is,
        // not have this code bake one in.
        assert_eq!(config.request_checksum_calculation(), None);
        assert_eq!(config.response_checksum_validation(), None);
    }

    // --- SdkError classification (pure, no network) -------------------------

    #[test]
    fn classify_timeout_error_as_timeout() {
        let err: SdkError<(), ()> = SdkError::timeout_error("boom");
        assert!(matches!(classify_sdk_error(&err), Some(AppError::Timeout)));
    }

    #[test]
    fn classify_dispatch_timeout_as_timeout() {
        let connector_err = aws_sdk_s3::error::ConnectorError::timeout("boom".into());
        let err: SdkError<(), ()> = SdkError::dispatch_failure(connector_err);
        assert!(matches!(classify_sdk_error(&err), Some(AppError::Timeout)));
    }

    #[test]
    fn classify_dispatch_io_as_unreachable() {
        let connector_err = aws_sdk_s3::error::ConnectorError::io("boom".into());
        let err: SdkError<(), ()> = SdkError::dispatch_failure(connector_err);
        assert!(matches!(
            classify_sdk_error(&err),
            Some(AppError::Unreachable)
        ));
    }

    #[test]
    fn classify_construction_failure_as_unreachable() {
        let err: SdkError<(), ()> = SdkError::construction_failure("boom");
        assert!(matches!(
            classify_sdk_error(&err),
            Some(AppError::Unreachable)
        ));
    }

    #[test]
    fn classify_response_error_as_unreachable() {
        let err: SdkError<(), ()> = SdkError::response_error("boom", ());
        assert!(matches!(
            classify_sdk_error(&err),
            Some(AppError::Unreachable)
        ));
    }

    #[test]
    fn classify_service_error_defers_to_from_impl() {
        let err: SdkError<(), ()> = SdkError::service_error((), ());
        assert!(classify_sdk_error(&err).is_none());
    }

    #[test]
    fn normalize_service_error_uses_existing_bucket_already_exists_mapping() {
        let inner = aws_sdk_s3::types::error::BucketAlreadyExists::builder()
            .message("taken")
            .build();
        let create_err =
            aws_sdk_s3::operation::create_bucket::CreateBucketError::BucketAlreadyExists(inner);
        let sdk_err: SdkError<_, ()> = SdkError::service_error(create_err, ());

        let app_err = normalize_s3_error(sdk_err);

        assert_eq!(app_err.code(), "storage/bucket-exists");
    }

    #[test]
    fn normalize_timeout_does_not_fall_through_to_keyword_sniff() {
        let err: SdkError<aws_sdk_s3::operation::list_buckets::ListBucketsError, ()> =
            SdkError::timeout_error("connection timed out");

        let app_err = normalize_s3_error(err);

        assert_eq!(app_err.code(), "network/timeout");
    }

    // --- classify_error_code (pure) -----------------------------------------

    #[test]
    fn classify_error_code_maps_signature_does_not_match_to_invalid_credentials() {
        assert!(matches!(
            classify_error_code("SignatureDoesNotMatch", None),
            Some(AppError::InvalidCredentials)
        ));
    }

    #[test]
    fn classify_error_code_maps_invalid_access_key_id_to_invalid_credentials() {
        assert!(matches!(
            classify_error_code("InvalidAccessKeyId", None),
            Some(AppError::InvalidCredentials)
        ));
    }

    #[test]
    fn classify_error_code_maps_access_denied_variants_to_access_denied() {
        assert!(matches!(
            classify_error_code("AccessDenied", None),
            Some(AppError::AccessDenied)
        ));
        assert!(matches!(
            classify_error_code("AccessDeniedException", None),
            Some(AppError::AccessDenied)
        ));
    }

    #[test]
    fn classify_error_code_maps_no_such_bucket_using_message_as_bucket_name() {
        let app_err = classify_error_code("NoSuchBucket", Some("my-bucket"));
        assert!(matches!(
            app_err,
            Some(AppError::BucketNotFound { ref bucket }) if bucket == "my-bucket"
        ));
    }

    #[test]
    fn classify_error_code_no_such_bucket_falls_back_to_unknown_without_message() {
        let app_err = classify_error_code("NoSuchBucket", None);
        assert!(matches!(
            app_err,
            Some(AppError::BucketNotFound { ref bucket }) if bucket == "unknown"
        ));
    }

    #[test]
    fn classify_error_code_returns_none_for_unrecognized_code() {
        assert!(classify_error_code("SomeOtherCode", None).is_none());
    }

    // --- normalize_s3_error metadata-code fallback (pure, no network) -------

    #[test]
    fn normalize_recovers_invalid_credentials_from_unmodeled_list_buckets_error() {
        // Reproduces the real MinIO wire response from a wrong secret key
        // (see tests/minio_e2e.rs): a 403 with S3 error code
        // `SignatureDoesNotMatch`, which `ListBucketsError` doesn't model
        // as its own variant, so the SDK's own conversion alone would
        // collapse this to `AppError::Internal`.
        let meta = ErrorMetadata::builder()
            .code("SignatureDoesNotMatch")
            .message("The request signature we calculated does not match")
            .build();
        let unhandled = aws_sdk_s3::operation::list_buckets::ListBucketsError::generic(meta);
        let sdk_err: SdkError<_, ()> = SdkError::service_error(unhandled, ());

        let app_err = normalize_s3_error(sdk_err);

        assert_eq!(app_err.code(), "auth/invalid-credentials");
    }

    #[test]
    fn normalize_recovers_bucket_not_found_from_unmodeled_delete_bucket_error() {
        let meta = ErrorMetadata::builder()
            .code("NoSuchBucket")
            .message("no-such-bucket")
            .build();
        let unhandled = aws_sdk_s3::operation::delete_bucket::DeleteBucketError::generic(meta);
        let sdk_err: SdkError<_, ()> = SdkError::service_error(unhandled, ());

        let app_err = normalize_s3_error(sdk_err);

        assert_eq!(app_err.code(), "storage/bucket-not-found");
    }

    #[test]
    fn normalize_leaves_modeled_variant_mapping_untouched() {
        // A modeled variant (tier 2) must win outright -- the metadata-code
        // fallback (tier 3) must never be consulted, let alone override it.
        let inner = aws_sdk_s3::types::error::BucketAlreadyExists::builder()
            .message("taken")
            .build();
        let create_err =
            aws_sdk_s3::operation::create_bucket::CreateBucketError::BucketAlreadyExists(inner);
        let sdk_err: SdkError<_, ()> = SdkError::service_error(create_err, ());

        let app_err = normalize_s3_error(sdk_err);

        assert_eq!(app_err.code(), "storage/bucket-exists");
    }

    #[test]
    fn normalize_unrecognized_service_error_still_falls_through_to_internal() {
        let meta = ErrorMetadata::builder().code("SomeUnknownCode").build();
        let unhandled = aws_sdk_s3::operation::list_buckets::ListBucketsError::generic(meta);
        let sdk_err: SdkError<_, ()> = SdkError::service_error(unhandled, ());

        let app_err = normalize_s3_error(sdk_err);

        assert_eq!(app_err.code(), "internal");
    }

    // --- display_name (pure) ------------------------------------------------

    #[test]
    fn display_name_of_root_file_is_the_key() {
        assert_eq!(display_name("readme.md", false), "readme.md");
    }

    #[test]
    fn display_name_of_nested_file_is_last_segment() {
        assert_eq!(display_name("docs/2026/readme.md", false), "readme.md");
    }

    #[test]
    fn display_name_of_prefix_strips_trailing_slash() {
        assert_eq!(display_name("docs/2026/", true), "2026");
    }

    #[test]
    fn display_name_of_root_prefix_is_segment() {
        assert_eq!(display_name("docs/", true), "docs");
    }

    // --- to_list_page (pure, SDK output builders) ---------------------------

    fn sample_object(key: &str, size: i64, secs: i64) -> aws_sdk_s3::types::Object {
        aws_sdk_s3::types::Object::builder()
            .key(key)
            .size(size)
            .last_modified(aws_sdk_s3::primitives::DateTime::from_secs(secs))
            .storage_class(aws_sdk_s3::types::ObjectStorageClass::Standard)
            .build()
    }

    #[test]
    fn to_list_page_maps_prefixes_then_files() {
        let output = aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output::builder()
            .common_prefixes(
                aws_sdk_s3::types::CommonPrefix::builder()
                    .prefix("docs/photos/")
                    .build(),
            )
            .contents(sample_object("docs/readme.md", 1234, 1_752_830_520))
            .build();

        let page = to_list_page(&output, "docs/");

        assert_eq!(page.entries.len(), 2);
        let folder = &page.entries[0];
        assert_eq!(folder.key, "docs/photos/");
        assert_eq!(folder.name, "photos");
        assert!(folder.is_prefix);
        assert_eq!(folder.size, None);
        let file = &page.entries[1];
        assert_eq!(file.key, "docs/readme.md");
        assert_eq!(file.name, "readme.md");
        assert!(!file.is_prefix);
        assert_eq!(file.size, Some(1234));
        assert_eq!(file.storage_class.as_deref(), Some("STANDARD"));
        // RFC 3339, parseable by the frontend's `new Date(...)`.
        assert!(file
            .last_modified
            .as_deref()
            .is_some_and(|d| d.contains('T') && d.ends_with('Z')));
    }

    #[test]
    fn to_list_page_filters_the_listed_prefixs_own_folder_marker() {
        // Listing prefix "docs/" returns the zero-byte marker object
        // "docs/" itself in Contents when the folder was created via
        // create_folder -- it must not appear as a row.
        let output = aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output::builder()
            .contents(sample_object("docs/", 0, 1_752_830_520))
            .contents(sample_object("docs/readme.md", 10, 1_752_830_520))
            .build();

        let page = to_list_page(&output, "docs/");

        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].key, "docs/readme.md");
    }

    #[test]
    fn to_list_page_keeps_a_file_whose_key_exactly_equals_a_non_folder_search_prefix() {
        // The search box feeds typed text straight into `prefix` (design
        // §6). Typing a file's complete name, e.g. "readme.md", produces a
        // ListObjectsV2 call with `prefix == "readme.md"`, and the object
        // itself has key "readme.md" too -- since "readme.md" doesn't end
        // in "/", this is NOT the create_folder marker case, so the object
        // must still be returned instead of silently vanishing.
        let output = aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output::builder()
            .contents(sample_object("readme.md", 42, 1_752_830_520))
            .build();

        let page = to_list_page(&output, "readme.md");

        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].key, "readme.md");
    }

    #[test]
    fn to_list_page_still_filters_folder_marker_when_prefix_ends_in_slash() {
        // Companion to the test above: a folder-style prefix ("docs/") must
        // keep filtering out its own zero-byte marker object.
        let output = aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output::builder()
            .contents(sample_object("docs/", 0, 1_752_830_520))
            .build();

        let page = to_list_page(&output, "docs/");

        assert!(page.entries.is_empty());
    }

    #[test]
    fn to_list_page_passes_next_token_through() {
        let output = aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output::builder()
            .next_continuation_token("opaque-token")
            .build();
        let page = to_list_page(&output, "");
        assert_eq!(page.next_token.as_deref(), Some("opaque-token"));
    }

    #[test]
    fn to_list_page_has_no_token_on_last_page() {
        let output = aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output::builder().build();
        let page = to_list_page(&output, "");
        assert_eq!(page.next_token, None);
    }

    #[test]
    fn to_list_page_negative_size_becomes_none() {
        let output = aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output::builder()
            .contents(sample_object("weird.bin", -1, 1_752_830_520))
            .build();
        let page = to_list_page(&output, "");
        assert_eq!(page.entries[0].size, None);
    }

    // --- encode_copy_source (pure) ------------------------------------------

    #[test]
    fn encode_copy_source_plain_ascii_passes_through() {
        assert_eq!(
            encode_copy_source("bkt", "docs/readme.md"),
            "bkt/docs/readme.md"
        );
    }

    #[test]
    fn encode_copy_source_encodes_spaces_and_plus() {
        assert_eq!(
            encode_copy_source("bkt", "my file+1.txt"),
            "bkt/my%20file%2B1.txt"
        );
    }

    #[test]
    fn encode_copy_source_encodes_non_ascii() {
        assert_eq!(
            encode_copy_source("bkt", "照片/猫.jpg"),
            "bkt/%E7%85%A7%E7%89%87/%E7%8C%AB.jpg"
        );
    }

    #[test]
    fn encode_copy_source_keeps_slashes_and_unreserved() {
        assert_eq!(encode_copy_source("bkt", "a-b_c.d~e/f"), "bkt/a-b_c.d~e/f");
    }

    #[test]
    fn encode_copy_source_encodes_question_mark_and_hash() {
        assert_eq!(
            encode_copy_source("bkt", "weird?name#1.txt"),
            "bkt/weird%3Fname%231.txt"
        );
    }

    // --- folder_marker_key (pure) -------------------------------------------

    #[test]
    fn folder_marker_key_appends_slash() {
        assert_eq!(folder_marker_key("docs/newdir").unwrap(), "docs/newdir/");
    }

    #[test]
    fn folder_marker_key_normalizes_existing_slashes() {
        assert_eq!(folder_marker_key("newdir/").unwrap(), "newdir/");
        assert_eq!(folder_marker_key("/newdir").unwrap(), "newdir/");
    }

    #[test]
    fn folder_marker_key_single_segment_without_slash() {
        assert_eq!(folder_marker_key("a").unwrap(), "a/");
    }

    #[test]
    fn folder_marker_key_collapses_multiple_trailing_slashes() {
        assert_eq!(folder_marker_key("a//").unwrap(), "a/");
    }

    #[test]
    fn folder_marker_key_rejects_empty_and_blank() {
        assert!(folder_marker_key("").is_err());
        assert!(folder_marker_key("   ").is_err());
        assert!(folder_marker_key("//").is_err());
    }

    #[test]
    fn folder_marker_key_rejects_empty_inner_segment() {
        assert!(folder_marker_key("a//b").is_err());
    }

    // --- failed_key mapping (pure) ------------------------------------------

    fn s3_batch_error(key: &str, code: &str) -> aws_sdk_s3::types::Error {
        aws_sdk_s3::types::Error::builder()
            .key(key)
            .code(code)
            .message("detail")
            .build()
    }

    #[test]
    fn failed_key_maps_known_code_to_app_error_code() {
        let f = failed_key(&s3_batch_error("locked.txt", "AccessDenied"));
        assert_eq!(f.key, "locked.txt");
        assert_eq!(f.code, "auth/access-denied");
    }

    #[test]
    fn failed_key_falls_back_to_internal_for_unknown_code() {
        let f = failed_key(&s3_batch_error("odd.txt", "SomeWeirdCode"));
        assert_eq!(f.code, "internal");
    }

    #[test]
    fn failed_key_tolerates_missing_key_and_code() {
        let f = failed_key(&aws_sdk_s3::types::Error::builder().build());
        assert_eq!(f.key, "");
        assert_eq!(f.code, "internal");
    }

    // --- chunk_failure_keys (pure) ------------------------------------------

    #[test]
    fn chunk_failure_keys_maps_every_key_to_the_same_normalized_code() {
        let chunk = vec![
            "a.txt".to_string(),
            "b.txt".to_string(),
            "c.txt".to_string(),
        ];

        let result = chunk_failure_keys(&chunk, &AppError::Timeout);

        assert_eq!(result.len(), chunk.len());
        for (failed, key) in result.iter().zip(chunk.iter()) {
            assert_eq!(&failed.key, key);
            assert_eq!(failed.code, "network/timeout");
        }
    }

    #[test]
    fn chunk_failure_keys_empty_chunk_yields_empty_vec() {
        assert!(chunk_failure_keys(&[], &AppError::Unreachable).is_empty());
    }

    // --- chunk_keys (pure) --------------------------------------------------
    //
    // These pin the ≤1000 batching `delete_prefix` feeds into `delete_objects`
    // one batch at a time. The assertions check chunk *sizes and boundaries*,
    // not just the chunk count, so an off-by-one chunker (e.g. one that split
    // at 999 or 1001, or dropped/duplicated a key across a boundary) fails
    // them instead of sliding through on a matching count alone.

    fn numbered_keys(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("k{i}")).collect()
    }

    #[test]
    fn is_deletable_prefix_requires_a_trailing_slash() {
        // The recursive-delete guard: only folder-shaped prefixes are safe.
        assert!(is_deletable_prefix("docs/"));
        assert!(is_deletable_prefix("a/b/c/"));
        // Empty would wipe the bucket; a non-slash prefix would delete
        // siblings (docs2/, document.txt) under the delimiter-less walk.
        assert!(!is_deletable_prefix(""));
        assert!(!is_deletable_prefix("docs"));
        assert!(!is_deletable_prefix("doc"));
    }

    #[test]
    fn chunk_keys_of_zero_keys_is_no_chunks() {
        assert!(chunk_keys(&[], DELETE_BATCH_MAX).is_empty());
    }

    #[test]
    fn chunk_keys_of_exactly_one_batch_is_a_single_full_chunk() {
        let keys = numbered_keys(1000);
        let chunks = chunk_keys(&keys, DELETE_BATCH_MAX);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 1000);
        assert_eq!(chunks[0].first().map(String::as_str), Some("k0"));
        assert_eq!(chunks[0].last().map(String::as_str), Some("k999"));
    }

    #[test]
    fn chunk_keys_of_one_over_a_batch_splits_into_1000_plus_1() {
        let keys = numbered_keys(1001);
        let chunks = chunk_keys(&keys, DELETE_BATCH_MAX);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 1000);
        assert_eq!(chunks[1].len(), 1);
        // Boundary must fall between k999 and k1000, with nothing lost.
        assert_eq!(chunks[0].last().map(String::as_str), Some("k999"));
        assert_eq!(chunks[1].first().map(String::as_str), Some("k1000"));
    }

    #[test]
    fn chunk_keys_of_two_batches_splits_into_1000_plus_1000() {
        let keys = numbered_keys(2000);
        let chunks = chunk_keys(&keys, DELETE_BATCH_MAX);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 1000);
        assert_eq!(chunks[1].len(), 1000);
        assert_eq!(chunks[0].first().map(String::as_str), Some("k0"));
        assert_eq!(chunks[1].first().map(String::as_str), Some("k1000"));
        assert_eq!(chunks[1].last().map(String::as_str), Some("k1999"));
    }

    // --- classify_error_code: NoSuchKey -------------------------------------

    #[test]
    fn classify_error_code_maps_no_such_key_to_key_not_found() {
        let app_err = classify_error_code("NoSuchKey", Some("docs/gone.md"));
        assert!(matches!(
            app_err,
            Some(AppError::KeyNotFound { ref key }) if key == "docs/gone.md"
        ));
    }

    #[test]
    fn classify_error_code_maps_not_found_to_key_not_found() {
        // `HeadObject` is a HEAD request: a 404 has no XML body, so
        // `aws-sdk-s3` synthesizes the metadata code "NotFound" instead of
        // the "NoSuchKey" that GetObject/DeleteObject's XML-bodied 404s
        // produce. Both must classify the same way.
        let app_err = classify_error_code("NotFound", Some("some-key"));
        assert!(matches!(
            app_err,
            Some(AppError::KeyNotFound { ref key }) if key == "some-key"
        ));
    }

    // --- transfer plane helpers (pure) --------------------------------------

    #[test]
    fn completed_parts_are_sorted_by_number() {
        // S3 rejects CompleteMultipartUpload when the part list is not in
        // ascending order (InvalidPartOrder). Parts finish out of order under
        // concurrency, so the order must be restored before completing.
        let parts = vec![
            UploadedPart {
                number: 3,
                etag: "\"c\"".to_string(),
                size: 10,
            },
            UploadedPart {
                number: 1,
                etag: "\"a\"".to_string(),
                size: 10,
            },
            UploadedPart {
                number: 2,
                etag: "\"b\"".to_string(),
                size: 10,
            },
        ];
        let sorted = sorted_parts(&parts);
        let numbers: Vec<i32> = sorted.iter().map(|p| p.number).collect();
        assert_eq!(numbers, vec![1, 2, 3]);
    }

    #[test]
    fn sorting_an_empty_or_single_part_list_is_a_no_op() {
        assert!(sorted_parts(&[]).is_empty());
        let single = vec![UploadedPart {
            number: 7,
            etag: "\"g\"".to_string(),
            size: 1,
        }];
        let sorted = sorted_parts(&single);
        let numbers: Vec<i32> = sorted.iter().map(|p| p.number).collect();
        assert_eq!(numbers, vec![7]);
    }

    #[test]
    fn file_io_error_carries_the_offending_path() {
        let err = file_io_error(std::path::Path::new("/tmp/missing.bin"), "no such file");
        assert_eq!(err.code(), "local/file-io");
        assert_eq!(
            err.params().get("path").map(String::as_str),
            Some("/tmp/missing.bin")
        );
    }

    #[test]
    fn range_header_is_inclusive_byte_range() {
        // HTTP Range is inclusive on both ends: bytes=0-8388607 is the first
        // 8MiB, not 8MiB+1. An off-by-one here silently drops or duplicates a
        // byte at every chunk boundary.
        assert_eq!(range_header(0, 8 * 1024 * 1024), "bytes=0-8388607");
        assert_eq!(range_header(8_388_608, 100), "bytes=8388608-8388707");
    }

    #[test]
    fn range_header_for_a_single_byte() {
        assert_eq!(range_header(5, 1), "bytes=5-5");
    }

    #[test]
    fn range_header_zero_length_does_not_underflow() {
        // `length == 0` used to compute `offset + length - 1`, underflowing
        // `u64` at offset 0. The saturating form must not panic, and
        // degenerates to the single start byte (this value is never
        // actually sent: `get_range` short-circuits length 0 before this
        // is ever called).
        assert_eq!(range_header(0, 0), "bytes=0-0");
        assert_eq!(range_header(5, 0), "bytes=5-5");
    }
}
