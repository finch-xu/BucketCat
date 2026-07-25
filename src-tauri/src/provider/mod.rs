//! Object storage provider abstraction.
//!
//! Wraps the AWS S3 SDK client (and, eventually, other S3-compatible
//! providers) behind a small trait so [`crate::commands`] can talk to
//! "a bucket store" without depending on `aws-sdk-s3` types directly.

pub mod hub;
pub mod oss_admin;
pub mod oss_sign;
pub mod s3;

pub use hub::ProviderHub;
pub use s3::{from_connection, is_aws_endpoint, S3Provider};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::AppResult;

/// Clamps a caller-requested presigned-URL lifetime to a range S3 itself
/// enforces: at least 1 second, at most 604,800 seconds (7 days -- the hard
/// maximum `aws_sdk_s3::presigning::PresigningConfig::expires_in` accepts).
/// Called by [`S3Provider::presign_get`] before building the presigning
/// config, so a caller-supplied `0` or an absurdly large value never reaches
/// the SDK as a `PresigningConfigError` -- it's silently bounded into the
/// valid range instead. Pure, unit-tested below.
pub fn clamp_expiry(secs: u64) -> u64 {
    secs.clamp(1, 604_800)
}

/// A bucket as shown to the frontend.
///
/// Deliberately a plain DTO -- no `aws_sdk_s3` type ever crosses this
/// boundary. `creation_date` is a pre-formatted string (RFC 3339) rather
/// than an SDK `DateTime`/`chrono` value so the frontend never needs an
/// AWS-flavored date parser.
///
/// `region` is `Some(..)` only for Aliyun OSS connections, whose native
/// `ListBuckets` API (see [`oss_admin::list_buckets`]) reports each
/// bucket's own region -- unlike every other provider here, where a
/// connection's buckets are all implicitly in the connection's configured
/// region, so there's nothing per-bucket to report. `Some("")` is possible
/// (an OSS bucket whose `<Region>`/`<Location>` were both absent) and is
/// distinct from `None` ("this provider doesn't report per-bucket region at
/// all").
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Bucket {
    pub name: String,
    pub creation_date: Option<String>,
    pub region: Option<String>,
}

/// One row in an object listing: a real object (`is_prefix == false`) or a
/// "folder" — a common prefix produced by delimiter-`/` listing
/// (`is_prefix == true`, `key` ends with `/`).
///
/// Like [`Bucket`], this is a plain DTO: `last_modified` is a pre-formatted
/// RFC 3339 string, `size` is `None` for prefixes (and for the rare object
/// the server reports without a size), and no `aws_sdk_s3` type ever
/// appears here. `name` is the display name — the final path segment of
/// `key` — computed server-side so the frontend never re-implements key
/// parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObjectEntry {
    pub key: String,
    pub name: String,
    pub size: Option<u64>,
    pub last_modified: Option<String>,
    pub storage_class: Option<String>,
    pub is_prefix: bool,
}

/// One page of a delimiter listing. `next_token` is the opaque continuation
/// token for the next page (`None` when this is the last page); the
/// frontend feeds it back verbatim via `list_objects`' `token` argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListPage {
    pub entries: Vec<ObjectEntry>,
    pub next_token: Option<String>,
}

/// A single failed key inside a batch operation. `code` is an
/// [`crate::error::AppError::code`]-style i18n key (e.g.
/// `"auth/access-denied"`, falling back to `"internal"`), NOT a raw S3
/// error code — the frontend renders it through the same `errors.*`
/// dictionary as top-level errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailedKey {
    pub key: String,
    pub code: String,
}

/// Outcome of a batch operation that must not abort on individual
/// failures (design §7):「成功 N / 失败 M」+ the failure list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchResult {
    pub succeeded: u32,
    pub failed: Vec<FailedKey>,
}

/// One part the server has accepted, as needed to complete (or resume) a
/// multipart upload. `etag` is echoed back verbatim, quotes included -- S3
/// compares it byte-for-byte on `CompleteMultipartUpload`. `Deserialize` is
/// needed alongside `Serialize` because M4c's checkpoint file embeds a
/// `Vec<UploadedPart>` (via `MultipartState`) and must read it back on
/// startup, not just write it out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadedPart {
    pub number: i32,
    pub etag: String,
    pub size: u64,
}

/// Result of a `HeadObject` — just what a download needs. `etag` is captured
/// for M4c's cross-restart resume (it will re-`head` and compare); M4b's
/// in-session resume does not consult it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObjectHead {
    pub size: u64,
    pub etag: Option<String>,
    pub content_type: Option<String>,
}

/// Admin-plane operations against an object storage backend.
///
/// Implementations (currently just [`S3Provider`]) must not leak any
/// `aws_sdk_s3` type through this trait -- only [`Bucket`] and
/// [`crate::error::AppError`] cross the boundary, so callers in
/// [`crate::commands`] never need to depend on the AWS SDK directly.
#[async_trait]
pub trait Provider {
    /// Cheapest possible call that proves the connection's credentials and
    /// endpoint are usable. Implemented as `list_buckets().map(|_| ())`.
    async fn test_connection(&self) -> AppResult<()>;

    /// Lists every bucket visible to the connection's credentials.
    async fn list_buckets(&self) -> AppResult<Vec<Bucket>>;

    /// Creates a new bucket with the given name.
    async fn create_bucket(&self, name: &str) -> AppResult<()>;

    /// Deletes the (must be empty) bucket with the given name.
    async fn delete_bucket(&self, name: &str) -> AppResult<()>;

    /// Lists one page of objects and common prefixes ("folders") directly
    /// under `prefix`, using delimiter `/` semantics. `prefix` may be `""`
    /// (bucket root) or any string — it need not end in `/`, which is what
    /// makes the search box's "current path + typed text" prefix search
    /// work. `token` resumes a previous page's `next_token`; `max_keys`
    /// caps files + folders per page.
    async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        token: Option<&str>,
        max_keys: i32,
    ) -> AppResult<ListPage>;

    /// Deletes many objects in server-side batches of up to 1000, never
    /// aborting the whole batch on per-key failures (design §7). The
    /// returned [`BatchResult`] carries success count + per-key failures.
    async fn delete_objects(&self, bucket: &str, keys: &[String]) -> AppResult<BatchResult>;

    /// Recursively deletes every object under `prefix` — **including** the
    /// zero-byte `prefix/` folder-marker object itself — by walking
    /// `list_objects_flat` to enumerate the whole subtree, then batching
    /// the keys through [`Provider::delete_objects`] (≤1000 per request) and
    /// aggregating one [`BatchResult`] (design §7: partial failures reported,
    /// never abort). `prefix` must be non-empty: a `""` prefix would
    /// enumerate and delete the entire bucket, which is never a UI gesture,
    /// so implementations reject it. This is the only path that can remove an
    /// *empty* in-app folder: `list_objects` (delimiter `/`) rolls that
    /// folder's marker into a CommonPrefix and filters it, while
    /// `list_objects_flat` surfaces it as a real key (closes M3 gap I4).
    async fn delete_prefix(&self, bucket: &str, prefix: &str) -> AppResult<BatchResult>;

    /// Renames one object as copy-then-delete (object stores have no
    /// native rename). A missing `from_key` fails with
    /// `storage/key-not-found`; the source is only deleted after the copy
    /// succeeded.
    async fn rename_object(&self, bucket: &str, from_key: &str, to_key: &str) -> AppResult<()>;

    /// Creates a "folder": a zero-byte object at `prefix` normalized to
    /// end with exactly one `/`.
    async fn create_folder(&self, bucket: &str, prefix: &str) -> AppResult<()>;

    // ---- Transfer plane (design §5) ----
    //
    // These take a local path plus a byte range rather than a request body,
    // so no `aws_sdk_s3` / `aws_smithy_types` type crosses this boundary
    // (design §3 principle 3). Building the body inside the implementation
    // also lets it use a *rewindable* file stream, which the SDK needs in
    // order to replay a request during its own internal retries.

    /// Uploads the first `length` bytes of `path` as a single `PutObject`.
    /// Used for files below the multipart threshold.
    async fn put_object_from_file(
        &self,
        bucket: &str,
        key: &str,
        path: &std::path::Path,
        length: u64,
    ) -> AppResult<()>;

    /// Starts a multipart upload, returning the server's `upload_id`.
    async fn multipart_init(&self, bucket: &str, key: &str) -> AppResult<String>;

    /// Uploads `path[offset .. offset + length]` as part `part_number`
    /// (1-based), returning the part's ETag.
    #[allow(clippy::too_many_arguments)]
    async fn upload_part_from_file(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: i32,
        path: &std::path::Path,
        offset: u64,
        length: u64,
    ) -> AppResult<String>;

    /// Assembles the object from `parts`. The implementation sorts by part
    /// number before sending -- S3 rejects an out-of-order list.
    async fn multipart_complete(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
        parts: &[UploadedPart],
    ) -> AppResult<()>;

    /// Discards a multipart upload and its server-side fragments. Called when
    /// the user cancels (design §5) -- an abandoned upload otherwise keeps
    /// billing for storage indefinitely.
    async fn multipart_abort(&self, bucket: &str, key: &str, upload_id: &str) -> AppResult<()>;

    /// Lists the parts the server has actually accepted for an in-progress
    /// multipart upload (`ListParts`, paginated). M4c's cross-restart resume
    /// treats this as the authoritative "already done" set -- a checkpoint
    /// file can lie (write interrupted, disk truncated), but the server's own
    /// record of what it received cannot.
    async fn multipart_list(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> AppResult<Vec<UploadedPart>>;

    /// Cheapest possible metadata read: object size (needed to plan the
    /// download) plus ETag/content-type. `storage/key-not-found` if absent.
    async fn head_object(&self, bucket: &str, key: &str) -> AppResult<ObjectHead>;

    /// Reads exactly `[offset, offset+length)` of `key`. Returns the bytes
    /// (bounded by the caller's chunk size — never the whole object at once),
    /// so no `aws_sdk_s3` stream type crosses this boundary (design §3).
    async fn get_range(
        &self,
        bucket: &str,
        key: &str,
        offset: u64,
        length: u64,
    ) -> AppResult<Vec<u8>>;

    /// Like `list_objects` but with NO delimiter, so it returns every object
    /// under `prefix` recursively (each row `is_prefix == false`, including
    /// the `prefix/` folder-marker object itself). Used for recursive folder
    /// download and recursive delete. `list_objects`' delimiter-`/` shape is
    /// pinned to the frontend and is deliberately left untouched.
    async fn list_objects_flat(
        &self,
        bucket: &str,
        prefix: &str,
        token: Option<&str>,
        max_keys: i32,
    ) -> AppResult<ListPage>;

    /// A time-limited, unauthenticated GET URL for `key` -- the foundation of
    /// the Share feature. `expires_secs` is bounded via [`clamp_expiry`]
    /// before being handed to the SDK, so a caller-supplied `0` or an
    /// absurdly large value never reaches it as a `PresigningConfigError`.
    /// The returned URL carries a live signature: callers MUST NOT log it
    /// (logging `key` is fine).
    async fn presign_get(&self, bucket: &str, key: &str, expires_secs: u64) -> AppResult<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // These serialization tests pin the wire contract the frontend's
    // `src/lib/api.ts` mirrors field-for-field. Renaming a field here is a
    // frontend-breaking change and must be caught.

    #[test]
    fn object_entry_serializes_with_contract_field_names() {
        let entry = ObjectEntry {
            key: "docs/readme.md".to_string(),
            name: "readme.md".to_string(),
            size: Some(1234),
            last_modified: Some("2026-07-18T09:22:00Z".to_string()),
            storage_class: Some("STANDARD".to_string()),
            is_prefix: false,
        };
        let v = serde_json::to_value(&entry).unwrap();
        assert_eq!(v["key"], "docs/readme.md");
        assert_eq!(v["name"], "readme.md");
        assert_eq!(v["size"], 1234);
        assert_eq!(v["last_modified"], "2026-07-18T09:22:00Z");
        assert_eq!(v["storage_class"], "STANDARD");
        assert_eq!(v["is_prefix"], false);
    }

    #[test]
    fn prefix_entry_serializes_null_metadata() {
        let entry = ObjectEntry {
            key: "docs/".to_string(),
            name: "docs".to_string(),
            size: None,
            last_modified: None,
            storage_class: None,
            is_prefix: true,
        };
        let v = serde_json::to_value(&entry).unwrap();
        assert!(v["size"].is_null());
        assert!(v["last_modified"].is_null());
        assert!(v["storage_class"].is_null());
        assert_eq!(v["is_prefix"], true);
    }

    #[test]
    fn list_page_serializes_entries_and_token() {
        let page = ListPage {
            entries: vec![],
            next_token: Some("tok-1".to_string()),
        };
        let v = serde_json::to_value(&page).unwrap();
        assert!(v["entries"].as_array().unwrap().is_empty());
        assert_eq!(v["next_token"], "tok-1");
    }

    #[test]
    fn batch_result_serializes_failed_list() {
        let result = BatchResult {
            succeeded: 2,
            failed: vec![FailedKey {
                key: "a.txt".to_string(),
                code: "auth/access-denied".to_string(),
            }],
        };
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["succeeded"], 2);
        assert_eq!(v["failed"][0]["key"], "a.txt");
        assert_eq!(v["failed"][0]["code"], "auth/access-denied");
    }
}
