//! Object storage provider abstraction.
//!
//! Wraps the AWS S3 SDK client (and, eventually, other S3-compatible
//! providers) behind a small trait so [`crate::commands`] can talk to
//! "a bucket store" without depending on `aws-sdk-s3` types directly.
