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
}
