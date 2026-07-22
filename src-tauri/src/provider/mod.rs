//! Object storage provider abstraction.
//!
//! Wraps the AWS S3 SDK client (and, eventually, other S3-compatible
//! providers) behind a small trait so [`crate::commands`] can talk to
//! "a bucket store" without depending on `aws-sdk-s3` types directly.

pub mod s3;

pub use s3::{from_connection, is_aws_endpoint, S3Provider};

use async_trait::async_trait;
use serde::Serialize;

use crate::error::AppResult;

/// A bucket as shown to the frontend.
///
/// Deliberately a plain DTO -- no `aws_sdk_s3` type ever crosses this
/// boundary. `creation_date` is a pre-formatted string (RFC 3339) rather
/// than an SDK `DateTime`/`chrono` value so the frontend never needs an
/// AWS-flavored date parser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Bucket {
    pub name: String,
    pub creation_date: Option<String>,
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

    /// Renames one object as copy-then-delete (object stores have no
    /// native rename). A missing `from_key` fails with
    /// `storage/key-not-found`; the source is only deleted after the copy
    /// succeeded.
    async fn rename_object(&self, bucket: &str, from_key: &str, to_key: &str) -> AppResult<()>;

    /// Creates a "folder": a zero-byte object at `prefix` normalized to
    /// end with exactly one `/`.
    async fn create_folder(&self, bucket: &str, prefix: &str) -> AppResult<()>;
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
