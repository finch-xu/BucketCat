//! The small `hyper` + `hyper-rustls` HTTPS client shared by the provider
//! modules that talk to an API `aws-sdk-s3` cannot reach.
//!
//! Three callers today, for unrelated reasons:
//!
//! - [`crate::provider::oss_admin`] signs and sends Aliyun OSS's *native*
//!   `ListBuckets`, which is not an S3 operation at all.
//! - [`crate::provider::r2_admin`] calls `api.cloudflare.com` for R2 bucket
//!   metadata the S3 API simply does not expose (usage counts, public-access
//!   domains, jurisdiction).
//! - [`crate::provider::b2_admin`] calls `api.backblazeb2.com` for the one
//!   thing no S3 call can report: which region a B2 account lives in (you
//!   would need the endpoint already to ask over S3).
//!
//! Deliberately **not** `reqwest`: see the long comment in `Cargo.toml` for
//! the full reasoning, but in short, every `reqwest` TLS feature drags in
//! `rustls-platform-verifier` and seven new `Cargo.lock` entries, while the
//! crates used here are already resolved as dependencies of `aws-sdk-s3`'s own
//! transport. Adding either caller costs zero new dependency-graph nodes.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Empty;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use crate::error::{AppError, AppResult};

/// The concrete client type both callers use: HTTPS-only, HTTP/1, empty
/// request bodies (every request either module makes is a GET).
pub type HttpsClient = Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Empty<Bytes>,
>;

/// Builds the HTTPS client. `context` prefixes the error message on TLS setup
/// failure so a caller's log line still says which subsystem failed.
///
/// Uses an explicit `CryptoProvider` (rather than relying on a process-wide
/// default) so this never depends on some other component -- e.g.
/// `aws-sdk-s3`'s own transport -- having already installed one, and never
/// races it if not.
///
/// `https_only()`, not `https_or_http()`: both callers talk to a public cloud
/// API over TLS, and silently falling back to plaintext would send a live
/// credential (an OSS signature, or a Cloudflare Bearer token) in the clear.
pub fn build_https_client(context: &str) -> AppResult<HttpsClient> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let connector = HttpsConnectorBuilder::new()
        .with_provider_and_native_roots(provider)
        .map_err(|e| AppError::Internal {
            message: format!("{context}: tls setup failed: {e}"),
        })?
        .https_only()
        .enable_http1()
        .build();
    Ok(Client::builder(TokioExecutor::new()).build(connector))
}

/// Maps a transport-level `hyper` failure to [`AppError::Timeout`] or
/// [`AppError::Unreachable`].
///
/// The error's own `Display` is inspected rather than its type: `hyper_util`'s
/// legacy client wraps the underlying cause, and there is no stable typed
/// variant to match on. The error string is never propagated to the caller --
/// only the classification is -- so nothing from the request (URI included)
/// can leak into an `AppError`.
pub fn classify_transport_error(e: &hyper_util::client::legacy::Error) -> AppError {
    let lower = e.to_string().to_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        AppError::Timeout
    } else {
        AppError::Unreachable
    }
}
