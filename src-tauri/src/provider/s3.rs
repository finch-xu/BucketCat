//! AWS S3-compatible provider (admin plane): bucket list/create/delete plus
//! a lightweight connection check, backed by `aws-sdk-s3`.
//!
//! This module is the only place in the crate that touches `aws_sdk_s3`
//! types outside of [`crate::error`]'s `From<aws_sdk_s3::Error>` -- the
//! [`Provider`] trait and [`Bucket`] DTO in [`crate::provider`] keep every
//! other caller (in particular `crate::commands`) free of an SDK dependency.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::RwLock;

use async_trait::async_trait;
use aws_sdk_s3::config::{
    Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::DateTimeFormat;
use aws_sdk_s3::primitives::{ByteStream, Length};
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, Delete, ObjectIdentifier};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use tokio::task::JoinSet;

use crate::error::{AppError, AppResult};
use crate::provider::{
    clamp_expiry, oss_admin, BatchResult, Bucket, FailedKey, ListPage, ObjectEntry, ObjectHead,
    Provider, UploadedPart,
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

/// Whether `force_path_style` should be `true` (bucket in the URL path)
/// for this connection.
///
/// - Real AWS ([`is_aws_endpoint`] true) -> `false` (virtual-hosted): AWS
///   supports and expects bucket-subdomain addressing.
/// - Aliyun OSS (`provider` case-insensitively `"oss"`) -> `false`: OSS's
///   own docs state it supports *only* virtual-hosted addressing for
///   security reasons and rejects path-style requests outright, unlike
///   every other backend this app targets.
/// - Everything else (MinIO / R2 / COS / B2 / generic / unknown) -> `true`,
///   unchanged from the pre-OSS behavior: those backends don't support
///   wildcard DNS for bucket subdomains, so virtual-hosted addressing
///   breaks for them.
pub fn uses_path_style(provider: &str, endpoint: &str) -> bool {
    if is_aws_endpoint(endpoint) {
        return false;
    }
    if provider.eq_ignore_ascii_case("oss") {
        return false;
    }
    true
}

/// Normalizes an OSS endpoint to its S3-compatible form by prefixing the
/// hostname with `s3.`. Non-OSS connections are returned byte-for-byte
/// unchanged -- this must never touch MinIO/R2/other endpoints.
///
/// Aliyun's *native* OSS endpoint (`oss-cn-hangzhou.aliyuncs.com`, which is
/// what the frontend prefills as the OSS default and what later M5 tasks'
/// native ListBuckets call needs) is a different address from its
/// *S3-compatible* endpoint (`s3.oss-cn-hangzhou.aliyuncs.com`) -- only the
/// latter accepts S3 API calls. This function performs that one conversion,
/// conservatively: it only adds the prefix when the host starts with
/// `oss-` (Aliyun's own naming convention for both public and
/// `-internal` regional hosts), leaving any host it doesn't recognize
/// (e.g. a user-supplied private domain) untouched rather than guessing.
/// It is idempotent: a host that already starts with `s3.oss-` is left
/// alone, so calling this twice (or on an endpoint the user already saved
/// in S3-compatible form) never produces `s3.s3....`.
///
/// The `oss-` match is case-insensitive and tolerant of leading whitespace
/// on the host, mirroring [`is_aws_endpoint`]'s own case-insensitive match --
/// without that, an endpoint like `OSS-cn-hangzhou.aliyuncs.com` would fail
/// this check while [`uses_path_style`] (case-insensitive on `provider`)
/// still picked virtual-hosted addressing, pointing the SDK at OSS's
/// *native* (non-S3) endpoint. Only the match is lenient: the returned
/// string is still built from the original, un-lowercased input.
pub fn s3_compat_endpoint(provider: &str, endpoint: &str) -> String {
    if !provider.eq_ignore_ascii_case("oss") {
        return endpoint.to_string();
    }

    let (scheme, rest) = match endpoint.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, endpoint),
    };
    // Split off any path/query so the prefix check and rewrite only ever
    // touch the host, not something that happens to follow it.
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, Some(path)),
        None => (rest, None),
    };
    // Host only (strip a trailing `:port`), mirroring `is_aws_endpoint`'s
    // approach -- but the possibly-porty `authority` is what actually gets
    // rewritten below, so the port (if any) survives untouched.
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    };

    // Case-insensitive and tolerant of leading whitespace, mirroring
    // `is_aws_endpoint`'s own `to_ascii_lowercase()` match -- but only the
    // MATCH is lenient; `result` below is still built from `authority`
    // (the original, un-lowercased input), never a lowercased copy.
    if !host.trim_start().to_ascii_lowercase().starts_with("oss-") {
        return endpoint.to_string();
    }

    let mut result = String::new();
    if let Some(scheme) = scheme {
        result.push_str(scheme);
        result.push_str("://");
    }
    result.push_str("s3.");
    result.push_str(authority);
    if let Some(path) = path {
        result.push('/');
        result.push_str(path);
    }
    result
}

/// Prefixes a schemeless endpoint with `https://`.
///
/// `aws_sdk_s3::Config::endpoint_url` requires an absolute URI (a scheme);
/// bare hostnames -- which is what a user types, and what most of
/// `src/lib/providers.ts`'s wizard defaults prefill -- are rejected outright,
/// which otherwise surfaces to the user as `AppError::Unreachable`. Defaults
/// to `https://` rather than `http://`: every cloud endpoint this app
/// targets is HTTPS, and the one plaintext backend (a local MinIO dev
/// instance) already spells out `http://` explicitly.
///
/// An endpoint that already carries *any* scheme (`https://`, `http://`, or
/// otherwise) is returned unchanged -- in particular, `http://` is never
/// silently upgraded to `https://`, since the local MinIO dev endpoint and
/// the live e2e suite depend on plaintext working as configured. Input is
/// trimmed first (`from_connection` already rejects an empty/blank endpoint
/// before this is ever reached; trimming here is normalization only, not
/// validation). An empty input (only reachable by calling this fn directly,
/// e.g. from a test) returns the bare scheme `"https://"`.
pub fn with_scheme(endpoint: &str) -> String {
    let trimmed = endpoint.trim();
    if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

/// Whether this provider can use S3's Multi-Object Delete (`POST
/// /?delete`).
///
/// Aliyun OSS mandates a `Content-MD5` header on that request, but
/// aws-sdk-s3 1.139 only ever sends `x-amz-checksum-*` flexible checksums --
/// it has no way to send `Content-MD5` -- so the endpoint is unusable there
/// (confirmed live: `400 MissingArgument`/`Content-MD5`).
///
/// Rainyun ROS (`provider` case-insensitively `"rainyun"`, a MinIO backend
/// fronted by an APISIX gateway) has the identical requirement. Confirmed
/// live against a real `cn-nb1.rains3.com` endpoint on 2026-07-25 by trying
/// all three request shapes `DeleteObjects` can be sent with: a request
/// carrying `Content-MD5` succeeds (`200`); one with no checksum header at
/// all is rejected (`400 MissingContentMD5`); and -- critically -- one
/// carrying `x-amz-checksum-crc32`, which is exactly what aws-sdk-s3 sends
/// by default, is *also* rejected with the same `400 MissingContentMD5`.
/// So, like OSS, Rainyun is unusable via aws-sdk-s3's batch path: both
/// backends hard-require the legacy `Content-MD5` header on this endpoint,
/// and aws-sdk-s3 simply has no way to send it.
///
/// Connections this returns `false` for fall back to one `DeleteObject` per
/// key instead: single-object delete has no request body, so it's never
/// subject to this requirement.
pub fn supports_batch_delete(provider: &str) -> bool {
    !(provider.eq_ignore_ascii_case("oss") || provider.eq_ignore_ascii_case("rainyun"))
}

/// Rewrites an OSS endpoint from one region to another.
///
/// OSS embeds the region right in the hostname (`oss-cn-beijing.aliyuncs.com`,
/// or its intranet form `oss-cn-beijing-internal.aliyuncs.com`). This
/// substitutes `oss-{from_region}` -> `oss-{to_region}` in the *authority*
/// (host + optional port) only, so the intranet/extranet form, a custom
/// port, and any path are all preserved byte-for-byte -- and so is the
/// `s3.`-prefixed S3-compatible form ([`s3_compat_endpoint`]'s output),
/// since `oss-{from_region}` still occurs as a substring of
/// `s3.oss-{from_region}...`.
///
/// `from_region == to_region` short-circuits to the input unchanged (no
/// rewrite needed, and the empty-string edge case some callers could pass
/// for an unresolved region never reaches the substring check below).
///
/// When the authority doesn't contain `oss-{from_region}` at all -- a
/// CNAME'd custom domain, or simply a non-OSS host -- the endpoint is
/// returned byte-for-byte unchanged: such a host can't be reliably mapped
/// to another region by string substitution, and the caller (`client_for`)
/// is expected to fall back to the default client rather than guess. This
/// is also what makes the function a no-op (idempotent, harmless) on a
/// non-OSS endpoint like `http://127.0.0.1:9000`.
pub fn oss_endpoint_for_region(endpoint: &str, from_region: &str, to_region: &str) -> String {
    if from_region == to_region {
        return endpoint.to_string();
    }

    let (scheme, rest) = match endpoint.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, endpoint),
    };
    // Split off any path/query so the rewrite only ever touches the
    // authority, mirroring `s3_compat_endpoint`'s own split.
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, Some(path)),
        None => (rest, None),
    };

    let needle = format!("oss-{from_region}");
    if !authority.contains(needle.as_str()) {
        return endpoint.to_string();
    }
    let replacement = format!("oss-{to_region}");
    let new_authority = authority.replacen(needle.as_str(), replacement.as_str(), 1);

    let mut result = String::new();
    if let Some(scheme) = scheme {
        result.push_str(scheme);
        result.push_str("://");
    }
    result.push_str(&new_authority);
    if let Some(path) = path {
        result.push('/');
        result.push_str(path);
    }
    result
}

/// Rewrites a Qiniu Kodo S3 endpoint from one region to another.
///
/// Qiniu embeds the region as its own label in the hostname
/// (`s3.cn-east-1.qiniucs.com`), so this substitutes `s3.{from_region}.` ->
/// `s3.{to_region}.` in the *authority* (host + optional port) only, leaving
/// the scheme, any port and any path byte-for-byte intact. The trailing dot
/// in the needle is load-bearing: it pins the match to a whole hostname
/// label, so a region id that happens to be a prefix of another can't be
/// partially rewritten.
///
/// This is the Qiniu twin of [`oss_endpoint_for_region`] and follows the same
/// contract:
///
/// - `from_region == to_region` short-circuits to the input unchanged (no
///   rewrite needed, and the empty-string edge case some callers could pass
///   for an unresolved region never reaches the substring check below).
/// - When the authority doesn't contain `s3.{from_region}.` at all -- a
///   CNAME'd custom domain, or simply a non-Qiniu host -- the endpoint comes
///   back byte-for-byte unchanged: such a host can't be reliably mapped to
///   another region by string substitution, and the caller
///   ([`S3Provider::client_for`]) is expected to fall back to the default
///   client rather than guess.
pub fn qiniu_endpoint_for_region(endpoint: &str, from_region: &str, to_region: &str) -> String {
    if from_region == to_region {
        return endpoint.to_string();
    }

    let (scheme, rest) = match endpoint.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, endpoint),
    };
    // Split off any path/query so the rewrite only ever touches the
    // authority, mirroring `oss_endpoint_for_region`'s own split.
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, Some(path)),
        None => (rest, None),
    };

    let needle = format!("s3.{from_region}.");
    if !authority.contains(needle.as_str()) {
        return endpoint.to_string();
    }
    let replacement = format!("s3.{to_region}.");
    let new_authority = authority.replacen(needle.as_str(), replacement.as_str(), 1);

    let mut result = String::new();
    if let Some(scheme) = scheme {
        result.push_str(scheme);
        result.push_str("://");
    }
    result.push_str(&new_authority);
    if let Some(path) = path {
        result.push('/');
        result.push_str(path);
    }
    result
}

/// One `GetBucketLocation`, normalized to `Option<String>`: `None` when the
/// server reports an empty (or absent) `LocationConstraint`.
///
/// That normalization is the whole point of the function, and callers must
/// treat `None` as "unresolved" rather than as the region `""`: an empty region
/// cached as if resolved would make [`S3Provider::client_for`] compare
/// `"" != routing.region` and then try to build a client for region `""`.
/// (Real AWS reports `us-east-1` exactly this way; whether Qiniu ever does is
/// untested, hence handling it rather than assuming.)
///
/// A free function taking the client (rather than an `S3Provider` method) so
/// the `'static` closures [`S3Provider::list_buckets_qiniu`] spawns can call
/// it with a cloned client, and so the single lazy lookup in
/// [`S3Provider::resolve_region`] shares the exact same normalization instead
/// of re-deriving it.
async fn bucket_location(
    client: &aws_sdk_s3::Client,
    bucket: &str,
) -> AppResult<Option<String>> {
    let output = client
        .get_bucket_location()
        .bucket(bucket)
        .send()
        .await
        .map_err(normalize_s3_error)?;
    Ok(output
        .location_constraint()
        .map(|c| c.as_str())
        .filter(|c| !c.is_empty())
        .map(|c| c.to_string()))
}

/// Which backend's cross-region quirk [`RegionRouting`] is routing around.
///
/// Both members share the same *cause* -- the server refuses a request for a
/// bucket that lives in a different region than the endpoint it arrived at,
/// rather than routing it internally -- and therefore the same machinery.
/// They differ in exactly three places, each a `match` on this enum:
///
/// 1. **How the bucket -> region map gets filled.** OSS calls its *native*
///    `ListBuckets` ([`oss_admin::list_buckets`]), the only OSS API that
///    reports each bucket's own region. Qiniu uses the plain S3
///    `GetBucketLocation`, which -- verified live 2026-07-30 -- answers for
///    *any* bucket in the account from *any* regional endpoint. (Qiniu's own
///    `ListBuckets` response does carry a `<LocationConstraint>` per bucket,
///    but `aws-sdk-s3` 1.139 only deserializes AWS's `<BucketRegion>` tag,
///    so that field is silently dropped and can't be used.)
/// 2. **How a target region's endpoint is derived** --
///    [`oss_endpoint_for_region`] vs [`qiniu_endpoint_for_region`], dispatched
///    through [`endpoint_for_region`].
/// 3. **Which provider string [`build_client`] is given** -- see
///    [`provider_str`], which matters because `build_client` branches on it
///    for the `s3.` rewrite and path-style addressing.
///
/// The error code each backend rejects a cross-region request with also
/// differs (`PermanentRedirect` vs `IncorrectRegion`); both map to
/// [`AppError::WrongRegion`] in [`classify_error_code`], which is the
/// user-visible fallback for when routing didn't manage to kick in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionFlavor {
    Oss,
    Qiniu,
}

/// The `provider` string [`build_client`] expects for a given flavor.
///
/// Not cosmetic: `build_client` passes it to [`s3_compat_endpoint`] and
/// [`uses_path_style`], both of which branch on `"oss"`. Getting it wrong
/// would point a region-specific OSS client at the *native* (non-S3) host, or
/// silently switch Qiniu to virtual-hosted addressing.
fn provider_str(flavor: RegionFlavor) -> &'static str {
    match flavor {
        RegionFlavor::Oss => "oss",
        RegionFlavor::Qiniu => "qiniu",
    }
}

/// Derives `endpoint`'s equivalent in `to_region`, per flavor. Both arms are
/// no-ops when the endpoint doesn't match the expected shape (see each
/// function's contract), so an unrecognized custom domain comes back
/// unchanged rather than mangled.
fn endpoint_for_region(
    flavor: RegionFlavor,
    endpoint: &str,
    from_region: &str,
    to_region: &str,
) -> String {
    match flavor {
        RegionFlavor::Oss => oss_endpoint_for_region(endpoint, from_region, to_region),
        RegionFlavor::Qiniu => qiniu_endpoint_for_region(endpoint, from_region, to_region),
    }
}

/// Endpoint + credentials + routing caches, captured only for the providers
/// whose buckets can live outside the connection's configured region (Aliyun
/// OSS and Qiniu Kodo today -- see [`RegionFlavor`]). This is what lets one
/// connection reach the whole account: [`S3Provider::client_for`] resolves a
/// bucket's own region and hands back a client pointed at it.
///
/// `Debug` is hand-written (not derived), mirroring `store::Connection`'s
/// own manual `impl`, so that `S3Provider`'s derived `Debug` -- which
/// includes this field -- can never print `secret_access_key` in the clear.
struct RegionRouting {
    flavor: RegionFlavor,
    /// Endpoint with scheme. For OSS this is the *native* endpoint (e.g.
    /// `https://oss-cn-hangzhou.aliyuncs.com`) -- NOT the `s3.`-prefixed
    /// [`s3_compat_endpoint`] form `self.client` talks to -- because
    /// `oss_admin` needs the native host. Qiniu has no such split: its S3
    /// endpoint (`https://s3.cn-east-1.qiniucs.com`) is the only one, and
    /// [`s3_compat_endpoint`] leaves non-OSS endpoints untouched.
    endpoint: String,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    /// Bucket name -> region. Filled by [`S3Provider::list_buckets`]
    /// (whenever the sidebar expands this connection) and lazily by
    /// [`S3Provider::client_for`] on a cache miss. `std::sync::RwLock`, not
    /// `tokio::sync::RwLock`: every access clones what it needs and drops the
    /// guard before any `.await`, so the synchronous lock is both correct and
    /// cheaper.
    bucket_regions: RwLock<HashMap<String, String>>,
    /// Region -> a client already configured for that region's endpoint (same
    /// credentials, built through [`build_client`] -- the same helper
    /// [`from_connection`] uses, so the two constructions can never drift
    /// apart). Populated lazily by [`S3Provider::client_for`].
    region_clients: RwLock<HashMap<String, aws_sdk_s3::Client>>,
}

impl fmt::Debug for RegionRouting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegionRouting")
            .field("flavor", &self.flavor)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            // `finish_non_exhaustive` rather than listing `bucket_regions` /
            // `region_clients`: both are process-local routing caches with
            // nothing secret in them, but their content is noisy and
            // irrelevant to what this `Debug` impl exists to guard (the
            // secret above never printing in the clear), so they're
            // deliberately left out rather than dumped.
            .finish_non_exhaustive()
    }
}

/// A constructed S3 (or S3-compatible) admin-plane client for one saved
/// [`Connection`].
#[derive(Debug)]
pub struct S3Provider {
    client: aws_sdk_s3::Client,
    /// Whether this connection's backend supports S3's Multi-Object Delete
    /// (see [`supports_batch_delete`]) -- `false` for Aliyun OSS and Rainyun
    /// ROS today. A plain capability flag computed once in
    /// [`from_connection`]; never the [`Connection`] itself, and never
    /// credentials.
    batch_delete: bool,
    /// `Some` only for the providers that need per-bucket region routing
    /// (Aliyun OSS, Qiniu Kodo) -- see [`RegionRouting`]. `S3Provider` keeps
    /// `#[derive(Debug)]` safely because this field's own `Debug` impl is
    /// hand-redacted.
    routing: Option<RegionRouting>,
}

/// Builds an `aws_sdk_s3::Client` for one `(provider, endpoint, region)` +
/// credential set.
///
/// This is the **single** place `aws_sdk_s3::Config` gets built in this
/// module -- both [`from_connection`] (the connection's own default client)
/// and [`S3Provider::client_for`] (a region-specific OSS client, built from
/// the *same* credentials against a different region's endpoint) call this
/// rather than each assembling their own `Config::builder()`. That is
/// deliberate, not just deduplication: two independently-maintained copies
/// of this configuration are exactly how the class of bug this module's
/// other doc comments warn about (a client silently missing the `s3.`
/// rewrite, or the non-AWS checksum override, or virtual-hosted addressing)
/// would creep back in for one of the two call sites but not the other.
///
/// `endpoint` is the connection's *raw* endpoint (e.g. `oss-cn-hangzhou.
/// aliyuncs.com`, no `s3.` prefix, possibly no scheme) -- exactly what
/// `conn.endpoint` holds, and exactly what [`oss_endpoint_for_region`]
/// produces from [`OssNativeAuth::endpoint`] (which already carries a
/// scheme, added once by [`from_connection`]; running it through
/// [`with_scheme`] again here is a no-op, per that function's own
/// idempotence). [`s3_compat_endpoint`] and [`with_scheme`] are applied
/// here, exactly once, to whatever comes in.
///
/// Infallible: `aws_sdk_s3::Config::builder().build()` cannot fail (unlike,
/// e.g., presigning's `PresigningConfig::expires_in`).
fn build_client(
    provider: &str,
    endpoint: &str,
    region: &str,
    access_key_id: &str,
    secret_access_key: &str,
) -> aws_sdk_s3::Client {
    let is_aws = is_aws_endpoint(endpoint);

    let credentials = Credentials::new(
        access_key_id.to_string(),
        secret_access_key.to_string(),
        None,
        None,
        "bucketcat",
    );

    let mut config_builder = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .endpoint_url(with_scheme(&s3_compat_endpoint(provider, endpoint)))
        .region(Region::new(region.to_string()))
        .credentials_provider(credentials)
        .force_path_style(uses_path_style(provider, endpoint));

    // See `from_connection`'s doc comment: non-AWS endpoints must not get
    // the SDK's `WhenSupported` default, which breaks R2/older MinIO/Dell
    // ECS with `NotImplemented`, and OSS specifically with `aws-chunked`
    // being rejected outright. Real AWS endpoints are left on the SDK
    // default.
    if !is_aws {
        config_builder = config_builder
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
    }

    aws_sdk_s3::Client::from_conf(config_builder.build())
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
/// - `force_path_style` comes from [`uses_path_style`], not a plain negation
///   of [`is_aws_endpoint`] anymore: real AWS and Aliyun OSS both need
///   virtual-hosted addressing (`force_path_style(false)`) -- OSS's own docs
///   say it supports *only* virtual-hosted access for security reasons and
///   rejects path-style requests outright -- while MinIO, R2 and other
///   self-hosted backends still need path-style addressing (no wildcard DNS
///   for bucket subdomains).
/// - The endpoint handed to the SDK is [`s3_compat_endpoint`]'s output, run
///   through [`with_scheme`], not `conn.endpoint` directly: Aliyun's
///   *native* OSS endpoint (what the frontend prefills, and what later M5
///   tasks' native ListBuckets call needs) is a different host from its
///   *S3-compatible* endpoint (needs an `s3.` prefix), and only the latter
///   accepts S3 API calls; separately, `aws_sdk_s3::Config::endpoint_url`
///   requires an absolute URI, so [`with_scheme`] adds `https://` to
///   whichever of the schemeless wizard defaults (`s3`/`oss`/`r2`/`cos`/`b2`
///   in `providers.ts`) the user saved verbatim. `conn.endpoint` itself is
///   never mutated -- only these local conversions.
/// - For every non-AWS endpoint (this already includes OSS, since
///   [`is_aws_endpoint`] is false for `*.aliyuncs.com`), `request_checksum_calculation`
///   and `response_checksum_validation` are both explicitly set to
///   [`RequestChecksumCalculation::WhenRequired`] /
///   [`ResponseChecksumValidation::WhenRequired`] (design §2, 2026-07-23
///   revision, **mandatory**): aws-sdk-rust >=1.69 defaults both to
///   `WhenSupported`, which attaches a request checksum / demands response
///   checksum validation on every eligible operation. For OSS specifically
///   this is not just the pre-existing R2/older-MinIO/Dell-ECS
///   `NotImplemented` concern -- a `WhenSupported` request checksum on a
///   streamed body (every upload here reads from a file stream) is sent as
///   a trailer, which forces `aws-chunked` transfer encoding, and OSS
///   rejects `aws-chunked` outright (`InvalidArgument`), failing every OSS
///   upload. Since this branch is keyed on `!is_aws` (not "is OSS"), it
///   already covers OSS without widening what MinIO/R2/others get. A real
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

    // Captured before `region` is moved into `build_client` below -- `None`
    // for every provider that doesn't need per-bucket region routing, which
    // is all of them except OSS and Qiniu (see `RegionFlavor`). The endpoint
    // stored here is pre-`s3_compat_endpoint` (but scheme-normalized), which
    // is what OSS's native `ListBuckets` needs and what both flavors'
    // `endpoint_for_region` rewriters expect.
    let flavor = if conn.provider.eq_ignore_ascii_case("oss") {
        Some(RegionFlavor::Oss)
    } else if conn.provider.eq_ignore_ascii_case("qiniu") {
        Some(RegionFlavor::Qiniu)
    } else {
        None
    };
    let routing = flavor.map(|flavor| RegionRouting {
        flavor,
        endpoint: with_scheme(&conn.endpoint),
        region: region.clone(),
        access_key_id: conn.access_key_id.clone(),
        secret_access_key: conn.secret_access_key.clone(),
        bucket_regions: RwLock::new(HashMap::new()),
        region_clients: RwLock::new(HashMap::new()),
    });

    let client = build_client(
        &conn.provider,
        &conn.endpoint,
        &region,
        &conn.access_key_id,
        &conn.secret_access_key,
    );

    Ok(S3Provider {
        client,
        batch_delete: supports_batch_delete(&conn.provider),
        routing,
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
///
/// `pub(crate)` (not private): [`crate::provider::oss_admin::list_buckets`]
/// reuses this same mapping for OSS's native `ListBuckets` API's own error
/// XML (`<Code>...</Code>`), rather than re-deriving a second, possibly
/// drifting, code -> `AppError` table.
pub(crate) fn classify_error_code(code: &str, message: Option<&str>) -> Option<AppError> {
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
        // Aliyun OSS doesn't route cross-region requests: hitting a bucket
        // that lives in a different region than the one this connection is
        // configured for fails with this code (confirmed live -- see the
        // M5b task-1 brief), not `NoSuchBucket`. `message` isn't guaranteed
        // to actually be the bucket name (OSS's error body doesn't echo it
        // any more reliably than S3's does for the codes above), but the
        // `bucket` param is treated as best-effort context here, mirroring
        // `NoSuchBucket`/`NoSuchKey`'s own `message.unwrap_or("unknown")`
        // convention above.
        // Qiniu Kodo's equivalent of the above, and it is NOT
        // `PermanentRedirect`: a bucket reached through the wrong region's
        // `s3.{region}.qiniucs.com` host comes back as
        // `400 IncorrectRegion` ("The bucket region not match with the
        // service region"), confirmed live 2026-07-30. Normally
        // `S3Provider::client_for` routes around this before a request is
        // ever sent; this mapping is the user-readable fallback for when
        // region resolution didn't manage to (e.g. `GetBucketLocation` itself
        // failed), so the UI says "wrong region" instead of `internal`.
        "PermanentRedirect" | "IncorrectRegion" => Some(AppError::WrongRegion {
            bucket: message.unwrap_or("unknown").to_string(),
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

/// Maximum concurrent single-key `DeleteObject` requests when a connection
/// can't use Multi-Object Delete (see [`supports_batch_delete`] -- OSS and
/// Rainyun ROS today). Deleting one key per request instead of one request
/// per ≤1000 keys multiplies the round-trips a `delete_prefix` walk needs;
/// running them fully sequentially would make deleting a large OSS folder
/// painfully slow. 8 bounds the fan-out to something that still
/// meaningfully parallelizes without looking like a burst to the storage
/// backend.
const SINGLE_DELETE_CONCURRENCY: usize = 8;

/// Maximum concurrent `GetBucketLocation` requests while
/// [`S3Provider::list_buckets_qiniu`] fills in each bucket's region.
///
/// Qiniu's `ListBuckets` is account-wide but doesn't usably report regions
/// (see [`RegionFlavor`]), so one extra request per *previously unseen* bucket
/// is unavoidable. 8 matches [`SINGLE_DELETE_CONCURRENCY`] for the same
/// reason: it parallelizes meaningfully without looking like a burst to the
/// backend. The fan-out only ever happens once per connection -- already-known
/// buckets are skipped, and `ProviderHub` keeps the `S3Provider` (and with it
/// the cache) alive until the connection is edited.
const QINIU_BUCKET_LOCATION_CONCURRENCY: usize = 8;

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

/// Folds one batch of single-key `DeleteObject` outcomes (as collected off a
/// bounded-concurrency [`JoinSet`], order-independent) into a
/// [`BatchResult`]. Reuses [`AppError::code`] for a [`FailedKey`]'s `code`
/// -- the same source `chunk_failure_keys`/`failed_key` use -- rather than
/// inventing a separate error mapping for this path.
///
/// Extracted as a pure fn (no task spawning, no network) so the
/// `succeeded + failed.len() == results.len()` invariant `delete_objects`'s
/// non-batch (OSS) path must uphold is directly unit-testable. Pure,
/// unit-tested below.
fn fold_single_delete_outcomes(results: Vec<(String, Result<(), AppError>)>) -> BatchResult {
    let mut succeeded: u32 = 0;
    let mut failed: Vec<FailedKey> = Vec::new();
    for (key, result) in results {
        match result {
            Ok(()) => succeeded += 1,
            Err(err) => failed.push(FailedKey {
                key,
                code: err.code().to_string(),
            }),
        }
    }
    BatchResult { succeeded, failed }
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

impl S3Provider {
    /// Returns the client that should be used to reach `bucket`.
    ///
    /// **Providers without region routing** (`self.routing` is `None`, i.e.
    /// everything except Aliyun OSS and Qiniu Kodo -- see [`RegionFlavor`]):
    /// returns `self.client.clone()` immediately and does nothing else, since
    /// those backends either route a cross-region request server-side or have
    /// no cross-region concept at all. `Client` is `Arc`-backed internally, so
    /// cloning it is cheap.
    ///
    /// **OSS / Qiniu**: resolves `bucket`'s region through two caches on
    /// [`RegionRouting`], both `std::sync::RwLock`-guarded. **A lock guard is
    /// never held across an `.await` anywhere in this method** -- every
    /// acquisition below is scoped to a block that either returns or drops
    /// the guard before the next `await` point, cloning out whatever value
    /// (a region `String`, or an already-`Arc`-backed `Client`) is actually
    /// needed first.
    ///
    /// 1. Look up `bucket` in the bucket -> region cache. On a miss, resolve
    ///    it over the network **once** (see [`S3Provider::resolve_region`]) --
    ///    this is what lets a bucket resolve even if the user never expanded
    ///    this connection in the sidebar first (e.g. a transfer resumed right
    ///    after an app restart) -- and cache whatever came back. A resolution
    ///    failure (network/permission) is not an error from this method's
    ///    point of view: it's logged at `debug` and treated the same as an
    ///    unresolved region below, so the caller's own request against the
    ///    fallback client surfaces any real problem with better context than
    ///    this routing step ever could.
    /// 2. If the resolved region equals this connection's configured region,
    ///    or the region is still unknown after step 1, return
    ///    `self.client.clone()` unchanged.
    /// 3. Otherwise, look up (or build and cache) a client for that region in
    ///    the region -> client cache, via [`build_client`] -- the same
    ///    helper [`from_connection`] uses, so this can never end up with a
    ///    subtly different configuration than the connection's own default
    ///    client. [`endpoint_for_region`] derives the target region's
    ///    endpoint from [`RegionRouting::endpoint`]. Two concurrent callers
    ///    racing to fill the same unknown region may each build a `Client`
    ///    (construction is local, no network call -- see `build_client`'s
    ///    doc comment), but only the first to take the write lock is kept;
    ///    the other's is simply dropped. Never panics, never deadlocks.
    ///
    /// Beyond the one resolution call in step 1 on a genuine cache miss, this
    /// method never itself makes a network request.
    async fn client_for(&self, bucket: &str) -> aws_sdk_s3::Client {
        let Some(routing) = &self.routing else {
            return self.client.clone();
        };

        let mut region = {
            let cache = routing
                .bucket_regions
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.get(bucket).cloned()
        };

        if region.is_none() {
            match self.resolve_region(routing, bucket).await {
                Ok(resolved) => {
                    let mut cache = routing
                        .bucket_regions
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    for (name, bucket_region) in resolved {
                        cache.insert(name, bucket_region);
                    }
                }
                Err(err) => {
                    tracing::debug!(
                        bucket,
                        error = %err,
                        "client_for: region resolution failed for this bucket; falling back to \
                         the connection's default client"
                    );
                }
            }
            region = {
                let cache = routing
                    .bucket_regions
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cache.get(bucket).cloned()
            };
        }

        let Some(region) = region else {
            return self.client.clone();
        };
        if region == routing.region {
            return self.client.clone();
        }

        {
            let cache = routing
                .region_clients
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(client) = cache.get(&region) {
                return client.clone();
            }
        }

        let endpoint =
            endpoint_for_region(routing.flavor, &routing.endpoint, &routing.region, &region);
        let built = build_client(
            provider_str(routing.flavor),
            &endpoint,
            &region,
            &routing.access_key_id,
            &routing.secret_access_key,
        );

        let mut cache = routing
            .region_clients
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // First writer wins: if another concurrent call already inserted a
        // client for this region, keep that one and let `built` (ours) be
        // dropped -- never overwrite, never panic, never deadlock.
        cache.entry(region).or_insert(built).clone()
    }

    /// Resolves which region `bucket` lives in, returning `(name, region)`
    /// pairs to merge into [`RegionRouting::bucket_regions`]. Called only by
    /// [`S3Provider::client_for`], only on a cache miss.
    ///
    /// The two flavors differ in cost, which is why this returns a map rather
    /// than a single region:
    ///
    /// - **OSS** has no per-bucket region API, so it calls its native
    ///   `ListBuckets` ([`oss_admin::list_buckets`]) and learns *every*
    ///   bucket's region in one request. All of it is worth caching, not just
    ///   the one bucket asked about.
    /// - **Qiniu** has `GetBucketLocation`, and -- verified live 2026-07-30 --
    ///   it answers for any bucket in the account from any regional endpoint,
    ///   so one request resolves exactly the one bucket needed. Cheaper than
    ///   OSS's whole-account listing, hence the single-entry map.
    ///
    /// A Qiniu bucket whose `LocationConstraint` comes back empty (which is
    /// how real AWS reports `us-east-1`) yields **no** entry rather than an
    /// empty-string one: an empty region would be cached as "resolved", and
    /// `client_for` would then compare `"" != routing.region` and try to build
    /// a client for region `""`. Returning nothing instead leaves the bucket
    /// unresolved, which `client_for` already handles by falling back to the
    /// connection's default client.
    async fn resolve_region(
        &self,
        routing: &RegionRouting,
        bucket: &str,
    ) -> AppResult<Vec<(String, String)>> {
        match routing.flavor {
            RegionFlavor::Oss => {
                let buckets = oss_admin::list_buckets(
                    &routing.endpoint,
                    &routing.region,
                    &routing.access_key_id,
                    &routing.secret_access_key,
                )
                .await?;
                Ok(buckets
                    .into_iter()
                    .map(|b| (b.name, b.region))
                    .collect())
            }
            RegionFlavor::Qiniu => {
                // `self.client`, never `client_for` -- that would recurse, and
                // it isn't needed: Qiniu answers `GetBucketLocation` for any
                // bucket in the account regardless of which regional endpoint
                // it arrives at (verified live 2026-07-30 by resolving a
                // `cn-north-1` bucket through the `ap-southeast-1` host).
                let region = bucket_location(&self.client, bucket).await?;
                Ok(region
                    .into_iter()
                    .map(|region| (bucket.to_string(), region))
                    .collect())
            }
        }
    }

    /// [`Provider::list_buckets`] for Qiniu Kodo.
    ///
    /// Qiniu's `ListBuckets` is account-wide -- any regional endpoint returns
    /// every bucket the account owns -- so the listing itself is one request
    /// through the connection's default client. What it *doesn't* give us is
    /// the regions: the response body does carry a `<LocationConstraint>` per
    /// bucket, but `aws-sdk-s3` 1.139 only deserializes AWS's own
    /// `<BucketRegion>` tag (see `protocol_serde::shape_bucket`), so that
    /// field never reaches `Bucket::bucket_region()`. Regions therefore come
    /// from one `GetBucketLocation` per bucket, fanned out at
    /// [`QINIU_BUCKET_LOCATION_CONCURRENCY`].
    ///
    /// Two properties keep that fan-out from being a per-refresh cost:
    ///
    /// - **Already-cached buckets are skipped.** The sidebar re-lists on every
    ///   refresh; without this, each refresh would replay N requests. First
    ///   expansion pays N, subsequent ones pay 0.
    /// - **A failed lookup is not fatal.** The bucket is returned with
    ///   `region: None` and logged at `debug`. Losing a region badge (and
    ///   falling back to the default client for that bucket) is far better
    ///   than failing the whole sidebar because one bucket's location call
    ///   timed out.
    ///
    /// Regions learned here are written into [`RegionRouting::bucket_regions`],
    /// the same cache [`S3Provider::client_for`] routes through -- listing a
    /// connection's buckets is the natural fill point, since the sidebar does
    /// it well before the user opens any one of them.
    async fn list_buckets_qiniu(&self, routing: &RegionRouting) -> AppResult<Vec<Bucket>> {
        let output = self
            .client
            .list_buckets()
            .send()
            .await
            .map_err(normalize_s3_error)?;

        let listed: Vec<(String, Option<String>)> = output
            .buckets()
            .iter()
            .map(|b| {
                (
                    b.name().unwrap_or_default().to_string(),
                    b.creation_date()
                        .and_then(|d| d.fmt(DateTimeFormat::DateTime).ok()),
                )
            })
            .collect();

        // Snapshot the cache once, rather than re-reading it per bucket: the
        // guard is dropped before any `.await` below (this module's rule), and
        // one pass is enough to decide what still needs a network lookup.
        let mut regions: HashMap<String, String> = {
            let cache = routing
                .bucket_regions
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.clone()
        };

        let unknown: Vec<String> = listed
            .iter()
            .map(|(name, _)| name.clone())
            .filter(|name| !name.is_empty() && !regions.contains_key(name))
            .collect();

        for chunk in unknown.chunks(QINIU_BUCKET_LOCATION_CONCURRENCY) {
            let mut set: JoinSet<(String, AppResult<Option<String>>)> = JoinSet::new();
            for name in chunk {
                let client = self.client.clone();
                let bucket = name.clone();
                set.spawn(async move {
                    let result = bucket_location(&client, &bucket).await;
                    (bucket, result)
                });
            }
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok((bucket, Ok(Some(region)))) => {
                        regions.insert(bucket, region);
                    }
                    // An empty `LocationConstraint` is left unresolved on
                    // purpose -- see `resolve_region`'s doc comment on why
                    // caching `""` would be worse than caching nothing.
                    Ok((bucket, Ok(None))) => {
                        tracing::debug!(
                            bucket,
                            "qiniu list_buckets: empty LocationConstraint; leaving region unresolved"
                        );
                    }
                    Ok((bucket, Err(err))) => {
                        tracing::debug!(
                            bucket,
                            error = %err,
                            "qiniu list_buckets: GetBucketLocation failed; this bucket will \
                             report no region and fall back to the default client"
                        );
                    }
                    // A spawned task panicking is not expected
                    // (`get_bucket_location().send()` doesn't panic), but it
                    // must not take the whole listing down either.
                    Err(err) => {
                        tracing::debug!(
                            error = %err,
                            "qiniu list_buckets: a GetBucketLocation task failed to join"
                        );
                    }
                }
            }
        }

        // No `.await` after this point, so the write guard never crosses one.
        {
            let mut cache = routing
                .bucket_regions
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for (name, region) in &regions {
                cache.insert(name.clone(), region.clone());
            }
        }

        Ok(listed
            .into_iter()
            .map(|(name, creation_date)| Bucket {
                region: regions.get(&name).cloned(),
                name,
                creation_date,
            })
            .collect())
    }


    /// Deletes `keys` one `DeleteObject` at a time, in batches of up to
    /// [`SINGLE_DELETE_CONCURRENCY`] concurrent requests -- the fallback
    /// [`Provider::delete_objects`] dispatches to when `self.batch_delete`
    /// is `false` (OSS and Rainyun ROS today; see [`supports_batch_delete`]).
    /// `DeleteObject` (`DELETE /bucket/key`) has no request body, so unlike
    /// Multi-Object Delete it never needs the `Content-MD5` header these
    /// backends demand and aws-sdk-s3 has no way to send.
    ///
    /// Takes an already-resolved `client` (the caller, [`Provider::delete_objects`],
    /// resolves it once via [`S3Provider::client_for`]) rather than resolving
    /// one itself per spawned task -- calling `client_for` from inside a
    /// spawned task would be harmless but wasteful (repeated cache lookups
    /// for the same bucket) and, worse, would reintroduce the exact
    /// lock-across-await risk this module's docs warn against if that method
    /// ever grew a mid-resolution `.await` before returning.
    ///
    /// One batch is fully awaited before the next is spawned (no streaming
    /// scheduling) -- simple, and plenty fast enough at this concurrency
    /// cap. A single key's failure never aborts the rest (design §7,
    /// mirrored from the batch path): every outcome, success or failure, is
    /// folded into the returned [`BatchResult`] via
    /// [`fold_single_delete_outcomes`], including a spawned task's panic
    /// (never observed in practice -- `delete_object().send()` itself
    /// doesn't panic) so `succeeded + failed.len() == keys.len()` always
    /// holds.
    async fn delete_objects_one_by_one(
        client: &aws_sdk_s3::Client,
        bucket: &str,
        keys: &[String],
    ) -> AppResult<BatchResult> {
        let mut succeeded: u32 = 0;
        let mut failed: Vec<FailedKey> = Vec::new();

        for chunk in keys.chunks(SINGLE_DELETE_CONCURRENCY) {
            let mut set: JoinSet<(String, Result<(), AppError>)> = JoinSet::new();
            for key in chunk {
                let client = client.clone();
                let bucket = bucket.to_string();
                let key = key.clone();
                set.spawn(async move {
                    let result_key = key.clone();
                    let result = client
                        .delete_object()
                        .bucket(bucket)
                        .key(key)
                        .send()
                        .await
                        .map(|_| ())
                        .map_err(normalize_s3_error);
                    (result_key, result)
                });
            }

            let mut results = Vec::with_capacity(chunk.len());
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(pair) => results.push(pair),
                    Err(join_err) => results.push((
                        String::new(),
                        Err(AppError::Internal {
                            message: format!("delete_object task panicked: {join_err}"),
                        }),
                    )),
                }
            }

            let batch = fold_single_delete_outcomes(results);
            succeeded += batch.succeeded;
            failed.extend(batch.failed);
        }

        Ok(BatchResult { succeeded, failed })
    }
}

#[async_trait]
impl Provider for S3Provider {
    async fn test_connection(&self) -> AppResult<()> {
        self.list_buckets().await.map(|_| ())
    }

    async fn list_buckets(&self) -> AppResult<Vec<Bucket>> {
        // OSS connections list through the *native* `ListBuckets` API
        // instead of the S3-compatible one `self.client` calls: it's the
        // only one that reports each bucket's own region (see
        // `crate::provider::Bucket::region`'s doc comment). Qiniu uses the
        // plain S3 `ListBuckets` but has to fetch each bucket's region
        // separately (see `list_buckets_qiniu`). Every other provider is
        // unaffected -- `routing` is `None` for all of them.
        match self.routing.as_ref().map(|r| (r, r.flavor)) {
            Some((routing, RegionFlavor::Oss)) => {
                let oss_buckets = oss_admin::list_buckets(
                    &routing.endpoint,
                    &routing.region,
                    &routing.access_key_id,
                    &routing.secret_access_key,
                )
                .await?;

                // Feed the bucket -> region cache `client_for` routes through
                // -- this is the most natural fill point: the sidebar lists a
                // connection's buckets as soon as it's expanded, well before
                // the user opens any one of them. No `.await` follows this
                // block, so the write guard never crosses one.
                {
                    let mut cache = routing
                        .bucket_regions
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    for b in &oss_buckets {
                        cache.insert(b.name.clone(), b.region.clone());
                    }
                }

                return Ok(oss_buckets
                    .into_iter()
                    .map(|b| Bucket {
                        name: b.name,
                        creation_date: b.creation_date,
                        region: Some(b.region),
                    })
                    .collect());
            }
            Some((routing, RegionFlavor::Qiniu)) => {
                return self.list_buckets_qiniu(routing).await;
            }
            None => {}
        }

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
                region: None,
            })
            .collect())
    }

    async fn create_bucket(&self, name: &str) -> AppResult<()> {
        let client = self.client_for(name).await;
        client
            .create_bucket()
            .bucket(name)
            .send()
            .await
            .map_err(normalize_s3_error)?;
        Ok(())
    }

    async fn delete_bucket(&self, name: &str) -> AppResult<()> {
        let client = self.client_for(name).await;
        client
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
        let client = self.client_for(bucket).await;
        let mut request = client
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
        let client = self.client_for(bucket).await;
        if !self.batch_delete {
            return Self::delete_objects_one_by_one(&client, bucket, keys).await;
        }

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
            match client
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
        let client = self.client_for(bucket).await;
        client
            .copy_object()
            .bucket(bucket)
            .copy_source(encode_copy_source(bucket, from_key))
            .key(to_key)
            .send()
            .await
            .map_err(normalize_s3_error)?;
        client
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
        let client = self.client_for(bucket).await;
        client
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
        let client = self.client_for(bucket).await;
        client
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
        let client = self.client_for(bucket).await;
        let out = client
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
        let client = self.client_for(bucket).await;
        let out = client
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

        let client = self.client_for(bucket).await;
        client
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
        let client = self.client_for(bucket).await;
        client
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
        let client = self.client_for(bucket).await;
        let mut out = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let mut req = client
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
            if !resp.is_truncated().unwrap_or(false) {
                break;
            }
            // A truncated response is contractually obliged to carry a next
            // marker. Guard against an SDK that ever violates that (truncated
            // == true, marker == None): without advancing `marker`, the next
            // request would re-list from the start and loop forever. Continue
            // only when both truncated AND a next marker are present.
            let Some(next) = resp.next_part_number_marker() else {
                break;
            };
            marker = Some(next.to_string());
        }
        Ok(out)
    }

    async fn head_object(&self, bucket: &str, key: &str) -> AppResult<ObjectHead> {
        let client = self.client_for(bucket).await;
        let out = client
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
        let client = self.client_for(bucket).await;
        let out = client
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
        let client = self.client_for(bucket).await;
        let mut req = client
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

    async fn presign_get(&self, bucket: &str, key: &str, expires_secs: u64) -> AppResult<String> {
        // `PresigningConfig::expires_in` is the only fallible step here, and
        // it's NOT an `SdkError` (it's a config-construction error raised
        // before any request is built), so it can't go through
        // `normalize_s3_error` -- mapped by hand to `AppError::Internal`.
        // `clamp_expiry` keeps this from ever actually failing in practice
        // (it only errors above 7 days, which `clamp_expiry` already bounds
        // to), but the fallible signature is kept honest rather than
        // `unwrap`ped.
        let cfg = aws_sdk_s3::presigning::PresigningConfig::expires_in(
            std::time::Duration::from_secs(clamp_expiry(expires_secs)),
        )
        .map_err(|e| AppError::Internal {
            message: format!("presign config: {e}"),
        })?;
        let client = self.client_for(bucket).await;
        let req = client
            .get_object()
            .bucket(bucket)
            .key(key)
            .presigned(cfg)
            .await
            .map_err(normalize_s3_error)?;
        // NEVER log `req.uri()` -- it carries a live request signature.
        Ok(req.uri().to_string())
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

    fn oss_connection() -> Connection {
        Connection {
            id: "c3".to_string(),
            provider: "oss".to_string(),
            name: "aliyun".to_string(),
            // Native endpoint, exactly what `src/lib/providers.ts` prefills --
            // deliberately NOT the S3-compatible `s3.`-prefixed form, since
            // `from_connection` is what must perform that conversion.
            endpoint: "oss-cn-hangzhou.aliyuncs.com".to_string(),
            region: "cn-hangzhou".to_string(),
            access_key_id: "LTAIexample".to_string(),
            secret_access_key: "secret".to_string(),
            default_bucket: None,
        }
    }

    fn rainyun_connection() -> Connection {
        Connection {
            id: "c4".to_string(),
            provider: "rainyun".to_string(),
            name: "rainyun".to_string(),
            endpoint: "https://cn-nb1.rains3.com".to_string(),
            region: "cn-nb1".to_string(),
            access_key_id: "rainyun-ak".to_string(),
            secret_access_key: "secret".to_string(),
            default_bucket: None,
        }
    }

    fn qiniu_connection() -> Connection {
        Connection {
            id: "c5".to_string(),
            provider: "qiniu".to_string(),
            name: "qiniu".to_string(),
            // Exactly what `src/lib/qiniu-regions.ts` prefills. Unlike OSS,
            // there is no native-vs-S3-compatible split to convert: this host
            // *is* the S3 endpoint, and `s3_compat_endpoint` leaves it alone.
            endpoint: "https://s3.cn-east-1.qiniucs.com".to_string(),
            region: "cn-east-1".to_string(),
            access_key_id: "qiniu-ak".to_string(),
            secret_access_key: "secret".to_string(),
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

    // --- uses_path_style / s3_compat_endpoint (pure) ------------------------

    #[test]
    fn oss_uses_virtual_hosted_style() {
        assert!(!uses_path_style("oss", "oss-cn-hangzhou.aliyuncs.com"));
        assert!(!uses_path_style(
            "OSS",
            "https://oss-cn-beijing.aliyuncs.com"
        ));
    }

    #[test]
    fn aws_uses_virtual_hosted_style() {
        assert!(!uses_path_style("s3", "s3.amazonaws.com"));
        assert!(!uses_path_style("s3", "https://s3.us-west-2.amazonaws.com"));
    }

    #[test]
    fn other_backends_keep_path_style() {
        assert!(uses_path_style("minio", "http://127.0.0.1:9000"));
        assert!(uses_path_style(
            "r2",
            "https://acct.r2.cloudflarestorage.com"
        ));
        assert!(uses_path_style("generic", "https://storage.example.com"));
        assert!(uses_path_style(
            "cos",
            "https://cos.ap-guangzhou.myqcloud.com"
        ));
    }

    #[test]
    fn oss_endpoint_gets_the_s3_compat_prefix() {
        assert_eq!(
            s3_compat_endpoint("oss", "oss-cn-hangzhou.aliyuncs.com"),
            "s3.oss-cn-hangzhou.aliyuncs.com"
        );
        assert_eq!(
            s3_compat_endpoint("oss", "https://oss-cn-hangzhou.aliyuncs.com"),
            "https://s3.oss-cn-hangzhou.aliyuncs.com"
        );
        assert_eq!(
            s3_compat_endpoint("oss", "https://oss-cn-shenzhen-internal.aliyuncs.com"),
            "https://s3.oss-cn-shenzhen-internal.aliyuncs.com"
        );
    }

    #[test]
    fn s3_compat_prefix_is_idempotent() {
        assert_eq!(
            s3_compat_endpoint("oss", "s3.oss-cn-hangzhou.aliyuncs.com"),
            "s3.oss-cn-hangzhou.aliyuncs.com"
        );
        assert_eq!(
            s3_compat_endpoint("oss", "https://s3.oss-cn-hangzhou.aliyuncs.com"),
            "https://s3.oss-cn-hangzhou.aliyuncs.com"
        );
    }

    #[test]
    fn s3_compat_endpoint_host_match_is_case_insensitive() {
        // `is_aws_endpoint` lowercases before matching; this host check must
        // not be stricter than that sibling function. An upper/mixed-case
        // `OSS-` host must still get the `s3.` rewrite -- built from the
        // ORIGINAL (non-lowercased) input, not a lowercased copy.
        assert_eq!(
            s3_compat_endpoint("oss", "OSS-cn-hangzhou.aliyuncs.com"),
            "s3.OSS-cn-hangzhou.aliyuncs.com"
        );
        assert_eq!(
            s3_compat_endpoint("oss", "https://OSS-cn-beijing.aliyuncs.com"),
            "https://s3.OSS-cn-beijing.aliyuncs.com"
        );
    }

    #[test]
    fn s3_compat_endpoint_host_match_tolerates_leading_whitespace() {
        // A schemeless host with leading whitespace must still be recognized
        // as an OSS host -- the detection is lenient, but the rewrite still
        // prefixes the ORIGINAL (whitespace-and-all) authority, since this
        // function's contract is "detection only" leniency, not input
        // normalization.
        assert_eq!(
            s3_compat_endpoint("oss", " oss-cn-hangzhou.aliyuncs.com"),
            "s3. oss-cn-hangzhou.aliyuncs.com"
        );
    }

    #[test]
    fn non_oss_endpoints_are_untouched() {
        assert_eq!(
            s3_compat_endpoint("minio", "http://127.0.0.1:9000"),
            "http://127.0.0.1:9000"
        );
        assert_eq!(
            s3_compat_endpoint("s3", "https://s3.amazonaws.com"),
            "https://s3.amazonaws.com"
        );
    }

    // --- with_scheme (pure) -------------------------------------------------

    #[test]
    fn with_scheme_adds_https_when_missing() {
        assert_eq!(with_scheme("s3.amazonaws.com"), "https://s3.amazonaws.com");
        assert_eq!(
            with_scheme("oss-cn-hangzhou.aliyuncs.com"),
            "https://oss-cn-hangzhou.aliyuncs.com"
        );
    }

    #[test]
    fn with_scheme_leaves_an_existing_scheme_alone() {
        assert_eq!(
            with_scheme("https://s3.amazonaws.com"),
            "https://s3.amazonaws.com"
        );
        // Plaintext must survive -- the local MinIO dev endpoint (and the
        // live e2e suite) depend on it never being silently upgraded.
        assert_eq!(
            with_scheme("http://127.0.0.1:9000"),
            "http://127.0.0.1:9000"
        );
    }

    #[test]
    fn with_scheme_trims_surrounding_whitespace() {
        assert_eq!(
            with_scheme("  s3.amazonaws.com  "),
            "https://s3.amazonaws.com"
        );
        assert_eq!(
            with_scheme("  https://s3.amazonaws.com  "),
            "https://s3.amazonaws.com"
        );
    }

    #[test]
    fn with_scheme_of_empty_string_returns_bare_https_scheme() {
        // `from_connection` already rejects an empty/blank endpoint before
        // this is ever called in practice; pinned here only to document the
        // chosen (harmless) behavior of this function in isolation.
        assert_eq!(with_scheme(""), "https://");
        assert_eq!(with_scheme("   "), "https://");
    }

    // --- supports_batch_delete (pure) ---------------------------------------

    #[test]
    fn oss_cannot_use_multi_object_delete() {
        assert!(!supports_batch_delete("oss"));
        assert!(!supports_batch_delete("OSS"));
    }
    #[test]
    fn rainyun_cannot_use_multi_object_delete() {
        // Confirmed live 2026-07-25 against a real cn-nb1.rains3.com
        // endpoint: aws-sdk-s3's default `x-amz-checksum-crc32` gets
        // `400 MissingContentMD5`, same as sending no checksum header at
        // all -- only a request carrying legacy `Content-MD5` succeeds.
        assert!(!supports_batch_delete("rainyun"));
        assert!(!supports_batch_delete("RAINYUN"));
    }
    #[test]
    fn other_backends_keep_batch_delete() {
        assert!(supports_batch_delete("s3"));
        assert!(supports_batch_delete("minio"));
        assert!(supports_batch_delete("r2"));
        assert!(supports_batch_delete("generic"));
    }

    // --- oss_endpoint_for_region (pure) --------------------------------------

    #[test]
    fn oss_endpoint_for_region_rewrites_the_extranet_host() {
        assert_eq!(
            oss_endpoint_for_region(
                "https://oss-cn-beijing.aliyuncs.com",
                "cn-beijing",
                "cn-shanghai"
            ),
            "https://oss-cn-shanghai.aliyuncs.com"
        );
    }

    #[test]
    fn oss_endpoint_for_region_preserves_the_intranet_suffix() {
        assert_eq!(
            oss_endpoint_for_region(
                "https://oss-cn-beijing-internal.aliyuncs.com",
                "cn-beijing",
                "cn-shanghai"
            ),
            "https://oss-cn-shanghai-internal.aliyuncs.com"
        );
    }

    #[test]
    fn oss_endpoint_for_region_works_without_a_scheme() {
        assert_eq!(
            oss_endpoint_for_region("oss-cn-beijing.aliyuncs.com", "cn-beijing", "cn-shanghai"),
            "oss-cn-shanghai.aliyuncs.com"
        );
    }

    #[test]
    fn oss_endpoint_for_region_same_region_is_a_no_op() {
        assert_eq!(
            oss_endpoint_for_region(
                "https://oss-cn-beijing.aliyuncs.com",
                "cn-beijing",
                "cn-beijing"
            ),
            "https://oss-cn-beijing.aliyuncs.com"
        );
    }

    #[test]
    fn oss_endpoint_for_region_leaves_a_custom_domain_untouched() {
        // A CNAME'd custom domain doesn't contain `oss-{from_region}` at
        // all -- this can't be reliably mapped to another region by string
        // substitution, so the caller (`client_for`) is expected to fall
        // back to the default client instead.
        assert_eq!(
            oss_endpoint_for_region("https://cdn.example.com", "cn-beijing", "cn-shanghai"),
            "https://cdn.example.com"
        );
    }

    #[test]
    fn oss_endpoint_for_region_is_a_no_op_on_a_non_oss_host() {
        // Idempotent / harmless on a host this function was never meant to
        // touch -- e.g. the local MinIO dev endpoint. `client_for` must
        // never mangle a non-OSS connection's endpoint.
        assert_eq!(
            oss_endpoint_for_region("http://127.0.0.1:9000", "cn-beijing", "cn-shanghai"),
            "http://127.0.0.1:9000"
        );
    }

    #[test]
    fn oss_endpoint_for_region_preserves_a_custom_port_and_path() {
        assert_eq!(
            oss_endpoint_for_region(
                "https://oss-cn-beijing.aliyuncs.com:8443/some/path",
                "cn-beijing",
                "cn-shanghai"
            ),
            "https://oss-cn-shanghai.aliyuncs.com:8443/some/path"
        );
    }

    #[test]
    fn oss_endpoint_for_region_rewrites_the_s3_compat_prefixed_host_too() {
        // `client_for` always feeds this the *native* `OssNativeAuth::endpoint`
        // (never the `s3.`-prefixed form), but the rewrite must also work on
        // an already-`s3.`-prefixed host: `oss-{from_region}` still occurs
        // as a substring of `s3.oss-{from_region}...`.
        assert_eq!(
            oss_endpoint_for_region(
                "https://s3.oss-cn-beijing.aliyuncs.com",
                "cn-beijing",
                "cn-shanghai"
            ),
            "https://s3.oss-cn-shanghai.aliyuncs.com"
        );
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
    fn valid_oss_connection_builds_successfully() {
        assert!(from_connection(&oss_connection()).is_ok());
    }

    #[test]
    fn from_connection_sets_batch_delete_false_for_oss_and_rainyun() {
        // OSS and Rainyun ROS can't use Multi-Object Delete (see
        // `supports_batch_delete`) -- `from_connection` must record that so
        // `delete_objects` knows to fall back to one `DeleteObject` per key
        // for these connections.
        let oss = from_connection(&oss_connection()).unwrap();
        assert!(!oss.batch_delete);
        let rainyun = from_connection(&rainyun_connection()).unwrap();
        assert!(!rainyun.batch_delete);

        // Every other backend keeps the batch path.
        let aws = from_connection(&aws_connection()).unwrap();
        assert!(aws.batch_delete);
        let minio = from_connection(&minio_connection()).unwrap();
        assert!(minio.batch_delete);
    }

    #[test]
    fn from_connection_captures_region_routing_only_for_oss_and_qiniu() {
        // OSS's native `ListBuckets` (called from `S3Provider::list_buckets`)
        // needs the *native* endpoint -- pre `s3_compat_endpoint` rewrite,
        // but still with a scheme -- alongside the plain credentials.
        let oss = from_connection(&oss_connection()).unwrap();
        let routing = oss
            .routing
            .as_ref()
            .expect("oss connections must capture region routing");
        assert_eq!(routing.flavor, RegionFlavor::Oss);
        assert_eq!(routing.endpoint, "https://oss-cn-hangzhou.aliyuncs.com");
        assert_eq!(routing.region, "cn-hangzhou");
        assert_eq!(routing.access_key_id, "LTAIexample");
        assert_eq!(routing.secret_access_key, "secret");

        // Qiniu needs the same routing state, just a different flavor -- its
        // endpoint has no native/S3-compatible split, so it is stored as-is
        // (scheme-normalized only).
        let qiniu = from_connection(&qiniu_connection()).unwrap();
        let routing = qiniu
            .routing
            .as_ref()
            .expect("qiniu connections must capture region routing");
        assert_eq!(routing.flavor, RegionFlavor::Qiniu);
        assert_eq!(routing.endpoint, "https://s3.cn-east-1.qiniucs.com");
        assert_eq!(routing.region, "cn-east-1");

        // Every other backend has nothing to capture.
        assert!(from_connection(&aws_connection()).unwrap().routing.is_none());
        assert!(from_connection(&minio_connection())
            .unwrap()
            .routing
            .is_none());
        assert!(from_connection(&rainyun_connection())
            .unwrap()
            .routing
            .is_none());
    }

    #[test]
    fn region_routing_debug_redacts_secret_access_key() {
        let mut conn = oss_connection();
        conn.secret_access_key = "super-secret-value".to_string();
        let oss = from_connection(&conn).unwrap();
        let routing = oss.routing.as_ref().unwrap();

        let debugged = format!("{routing:?}");

        assert!(!debugged.contains("super-secret-value"));
        assert!(debugged.contains("<redacted>"));
        // Sanity: other fields still show up normally.
        assert!(debugged.contains("LTAIexample"));
    }

    #[test]
    fn s3_provider_debug_never_leaks_the_oss_secret_through_its_derived_impl() {
        // `S3Provider` keeps `#[derive(Debug)]` -- this proves that's safe:
        // the derive calls `RegionRouting`'s own hand-redacted `Debug`, so
        // the secret can't leak even though nothing here special-cases
        // `S3Provider`'s own impl.
        let mut conn = oss_connection();
        conn.secret_access_key = "super-secret-value".to_string();
        let oss = from_connection(&conn).unwrap();

        let debugged = format!("{oss:?}");

        assert!(!debugged.contains("super-secret-value"));
        assert!(debugged.contains("<redacted>"));
    }

    // `aws_sdk_s3::Config` has no public getter for `force_path_style` or
    // `endpoint_url` (only `region`/the two checksum settings are exposed,
    // per the vendored 1.139.0 source), so the wiring is instead verified
    // black-box through a locally-computed presigned URL below: presigning
    // resolves the endpoint and chooses path- vs virtual-hosted addressing
    // exactly like a real request would, but never touches the network.
    #[tokio::test]
    async fn oss_connection_presigned_url_is_virtual_hosted_on_the_s3_compat_host() {
        // Scheme included -- see `oss_connection_without_a_scheme_is_a_known_pre_existing_gap`
        // below for the (out-of-scope, pre-existing) scheme-less case.
        let mut conn = oss_connection();
        conn.endpoint = "https://oss-cn-hangzhou.aliyuncs.com".to_string();

        let provider = from_connection(&conn).unwrap();
        let url = provider
            .presign_get("my-bucket", "docs/readme.md", 60)
            .await
            .expect("presigning is local-only, no network involved");

        // Virtual-hosted: the bucket is a subdomain of the *S3-compatible*
        // (`s3.`-prefixed) host, not a path segment on the native one.
        assert!(
            url.starts_with("https://my-bucket.s3.oss-cn-hangzhou.aliyuncs.com/"),
            "unexpected presigned URL: {url}"
        );
        // The stored connection itself must never be mutated by
        // `from_connection` -- the native endpoint is still needed by
        // later M5 tasks' native OSS ListBuckets call.
        assert_eq!(conn.endpoint, "https://oss-cn-hangzhou.aliyuncs.com");
    }

    #[tokio::test]
    async fn oss_connection_without_a_scheme_gets_https() {
        // Companion to `oss_connection_presigned_url_is_virtual_hosted_on_the_s3_compat_host`
        // above, but exercising the actual schemeless default the
        // add-connection wizard prefills from `providers.ts`
        // ("oss-cn-hangzhou.aliyuncs.com", no scheme). This used to fail --
        // `aws_sdk_s3::Config::endpoint_url` requires an absolute URI and
        // rejected the bare hostname with `AppError::Unreachable` -- until
        // `from_connection` started running the endpoint through
        // `with_scheme`.
        let conn = oss_connection();
        assert_eq!(conn.endpoint, "oss-cn-hangzhou.aliyuncs.com");

        let provider = from_connection(&conn).unwrap();
        let url = provider
            .presign_get("my-bucket", "docs/readme.md", 60)
            .await
            .expect("schemeless endpoint must get https:// so the SDK accepts it");

        // Virtual-hosted, on the https-defaulted S3-compat host.
        assert!(
            url.starts_with("https://my-bucket.s3.oss-cn-hangzhou.aliyuncs.com/"),
            "unexpected presigned URL: {url}"
        );
        // The stored connection itself must never be mutated.
        assert_eq!(conn.endpoint, "oss-cn-hangzhou.aliyuncs.com");
    }

    #[tokio::test]
    async fn aws_connection_without_a_scheme_gets_https() {
        // Same gap, but for the flagship AWS S3 provider: `providers.ts`'s
        // "s3" default endpoint is also schemeless ("s3.amazonaws.com").
        let mut conn = aws_connection();
        conn.endpoint = "s3.amazonaws.com".to_string();

        let provider = from_connection(&conn).unwrap();
        let url = provider
            .presign_get("my-bucket", "docs/readme.md", 60)
            .await
            .expect("schemeless endpoint must get https:// so the SDK accepts it");

        assert!(
            url.starts_with("https://my-bucket.s3.amazonaws.com/"),
            "unexpected presigned URL: {url}"
        );
        assert_eq!(conn.endpoint, "s3.amazonaws.com");
    }

    // --- client_for ----------------------------------------------------------
    //
    // These only ever exercise `client_for`'s CACHE HIT paths -- the bucket
    // -> region cache is seeded directly (`mod tests` can reach
    // `RegionRouting`'s private fields, being a child module of `s3`), never
    // left empty. That's deliberate, not just convenient: an empty cache
    // sends `client_for` down its lazy-fill branch, which calls the real
    // `S3Provider::resolve_region` -- an actual network request against
    // whatever `routing.endpoint` is configured to (`oss_admin::list_buckets`
    // for OSS, `GetBucketLocation` for Qiniu). This crate's tests must never
    // make a live request to a real Aliyun/Qiniu endpoint (see
    // `tests/oss_e2e.rs`'s safety rules), so that branch is deliberately
    // left to code review / the live e2e suite rather than a unit test here.
    //
    // `presign_get` is the vehicle for observing which client `client_for`
    // picked, exactly like the existing `oss_connection_presigned_url_is_..`
    // tests above: presigning is local-only computation (no request is ever
    // sent), so the resulting URL's host reveals the endpoint the returned
    // `Client` was configured with, with zero network I/O.

    /// An `S3Provider` for `conn` whose bucket -> region cache already has one
    /// entry -- a cache HIT, so `client_for` never reaches its
    /// network-calling fallback branch.
    fn provider_with_cached_region(conn: &Connection, bucket: &str, region: &str) -> S3Provider {
        let provider = from_connection(conn).unwrap();
        let routing = provider
            .routing
            .as_ref()
            .expect("this connection must produce region routing");
        routing
            .bucket_regions
            .write()
            .unwrap()
            .insert(bucket.to_string(), region.to_string());
        provider
    }

    /// An OSS `S3Provider` (region `cn-hangzhou`, see `oss_connection`) with a
    /// pre-seeded region cache.
    fn oss_provider_with_cached_region(bucket: &str, region: &str) -> S3Provider {
        provider_with_cached_region(&oss_connection(), bucket, region)
    }

    /// A Qiniu `S3Provider` (region `cn-east-1`, see `qiniu_connection`) with a
    /// pre-seeded region cache.
    fn qiniu_provider_with_cached_region(bucket: &str, region: &str) -> S3Provider {
        provider_with_cached_region(&qiniu_connection(), bucket, region)
    }

    #[tokio::test]
    async fn client_for_returns_the_default_client_unchanged_for_providers_without_routing() {
        // Providers that don't need region routing have `routing == None`, so
        // `client_for` must return `self.client.clone()` immediately -- no
        // cache lookup, no possibility of ever calling `resolve_region`.
        let provider = from_connection(&minio_connection()).unwrap();

        let client = provider.client_for("any-bucket").await;

        assert_eq!(
            client.config().region().map(|r| r.as_ref()),
            provider.client.config().region().map(|r| r.as_ref()),
        );
    }

    #[tokio::test]
    async fn client_for_returns_the_default_client_when_the_bucket_is_in_the_connection_region() {
        // `oss_connection()`'s configured region is "cn-hangzhou" -- caching
        // that same region for the bucket must short-circuit to the default
        // client rather than building a redundant "different" one.
        let provider = oss_provider_with_cached_region("same-region-bucket", "cn-hangzhou");

        let url = provider
            .presign_get("same-region-bucket", "docs/readme.md", 60)
            .await
            .expect("presigning is local-only, no network involved");

        assert!(
            url.starts_with("https://same-region-bucket.s3.oss-cn-hangzhou.aliyuncs.com/"),
            "unexpected presigned URL: {url}"
        );
    }

    #[tokio::test]
    async fn client_for_routes_to_a_region_specific_client_for_a_cross_region_bucket() {
        let provider = oss_provider_with_cached_region("shanghai-bucket", "cn-shanghai");

        let url = provider
            .presign_get("shanghai-bucket", "docs/readme.md", 60)
            .await
            .expect("presigning is local-only, no network involved");

        // Virtual-hosted, on the *cn-shanghai* S3-compat host -- proves
        // `client_for` picked a client built via `oss_endpoint_for_region` +
        // `build_client` for the bucket's own region, not the connection's
        // configured `cn-hangzhou` default.
        assert!(
            url.starts_with("https://shanghai-bucket.s3.oss-cn-shanghai.aliyuncs.com/"),
            "unexpected presigned URL: {url}"
        );
    }

    #[tokio::test]
    async fn client_for_caches_the_built_region_client_and_reuses_it() {
        let provider = oss_provider_with_cached_region("shanghai-bucket", "cn-shanghai");

        let _ = provider.client_for("shanghai-bucket").await;
        let _ = provider.client_for("shanghai-bucket").await;

        let routing = provider.routing.as_ref().unwrap();
        let cache = routing.region_clients.read().unwrap();
        assert_eq!(
            cache.len(),
            1,
            "a second call for an already-cached region must reuse the cached client, not \
             build (and cache) a new one"
        );
    }

    #[tokio::test]
    async fn client_for_shares_one_cached_client_across_buckets_in_the_same_non_default_region() {
        // The region -> client cache is keyed by REGION, not by bucket: two
        // different buckets that both resolve to "cn-shanghai" must share
        // exactly one cached client.
        let provider = from_connection(&oss_connection()).unwrap();
        {
            let routing = provider.routing.as_ref().unwrap();
            let mut cache = routing.bucket_regions.write().unwrap();
            cache.insert("bucket-a".to_string(), "cn-shanghai".to_string());
            cache.insert("bucket-b".to_string(), "cn-shanghai".to_string());
        }

        let _ = provider.client_for("bucket-a").await;
        let _ = provider.client_for("bucket-b").await;

        let routing = provider.routing.as_ref().unwrap();
        let cache = routing.region_clients.read().unwrap();
        assert_eq!(
            cache.len(),
            1,
            "two buckets resolving to the same region must share one cached client"
        );
    }

    // --- Qiniu region routing -----------------------------------------------
    //
    // Same vehicle as the OSS tests above (a pre-seeded cache + a locally
    // computed presigned URL), and the same reason: an empty cache would send
    // `client_for` into `resolve_region`, which for Qiniu means a real
    // `GetBucketLocation` against a live endpoint. Note the URLs below are
    // path-style (`host/bucket/key`), not virtual-hosted -- Qiniu supports
    // both, and `uses_path_style` keeps it on path-style like every other
    // non-AWS, non-OSS backend.

    #[tokio::test]
    async fn qiniu_client_for_returns_the_default_client_when_the_bucket_is_in_the_connection_region()
    {
        let provider = qiniu_provider_with_cached_region("same-region-bucket", "cn-east-1");

        let url = provider
            .presign_get("same-region-bucket", "docs/readme.md", 60)
            .await
            .expect("presigning is local-only, no network involved");

        assert!(
            url.starts_with("https://s3.cn-east-1.qiniucs.com/same-region-bucket/"),
            "unexpected presigned URL: {url}"
        );
    }

    #[tokio::test]
    async fn qiniu_client_for_routes_to_a_region_specific_client_for_a_cross_region_bucket() {
        // The exact scenario the live account exhibits: the connection is
        // configured for one region while the bucket lives in another. Before
        // region routing existed for Qiniu this produced `400 IncorrectRegion`.
        let provider = qiniu_provider_with_cached_region("north-bucket", "cn-north-1");

        let url = provider
            .presign_get("north-bucket", "docs/readme.md", 60)
            .await
            .expect("presigning is local-only, no network involved");

        // On the *cn-north-1* host -- proves `client_for` built a client via
        // `qiniu_endpoint_for_region` + `build_client` for the bucket's own
        // region, not the connection's configured `cn-east-1` default.
        assert!(
            url.starts_with("https://s3.cn-north-1.qiniucs.com/north-bucket/"),
            "unexpected presigned URL: {url}"
        );
    }

    #[tokio::test]
    async fn qiniu_client_for_caches_the_built_region_client_and_reuses_it() {
        let provider = qiniu_provider_with_cached_region("north-bucket", "cn-north-1");

        let _ = provider.client_for("north-bucket").await;
        let _ = provider.client_for("north-bucket").await;

        let routing = provider.routing.as_ref().unwrap();
        let cache = routing.region_clients.read().unwrap();
        assert_eq!(cache.len(), 1);
    }

    // --- qiniu_endpoint_for_region ------------------------------------------

    #[test]
    fn qiniu_endpoint_for_region_rewrites_the_region_label() {
        assert_eq!(
            qiniu_endpoint_for_region("https://s3.cn-east-1.qiniucs.com", "cn-east-1", "cn-north-1"),
            "https://s3.cn-north-1.qiniucs.com"
        );
    }

    #[test]
    fn qiniu_endpoint_for_region_is_a_noop_when_the_region_is_unchanged() {
        assert_eq!(
            qiniu_endpoint_for_region("https://s3.cn-east-1.qiniucs.com", "cn-east-1", "cn-east-1"),
            "https://s3.cn-east-1.qiniucs.com"
        );
    }

    #[test]
    fn qiniu_endpoint_for_region_is_a_noop_for_an_empty_target_region() {
        // `client_for` never calls this with an unresolved region (it returns
        // the default client first), but an empty `to_region` must not be able
        // to produce the nonsense host `s3..qiniucs.com`.
        assert_eq!(
            qiniu_endpoint_for_region("https://s3.cn-east-1.qiniucs.com", "cn-east-1", ""),
            "https://s3..qiniucs.com",
            "documented shape: substitution is mechanical, which is exactly why `client_for` \
             must short-circuit an unresolved region before reaching here"
        );
    }

    #[test]
    fn qiniu_endpoint_for_region_leaves_a_custom_domain_untouched() {
        // A CNAME'd domain can't be mapped to another region by substitution;
        // `client_for` is expected to fall back to the default client.
        assert_eq!(
            qiniu_endpoint_for_region("https://files.example.com", "cn-east-1", "cn-north-1"),
            "https://files.example.com"
        );
    }

    #[test]
    fn qiniu_endpoint_for_region_preserves_scheme_port_and_path() {
        assert_eq!(
            qiniu_endpoint_for_region(
                "http://s3.cn-east-1.qiniucs.com:8080/prefix",
                "cn-east-1",
                "cn-south-1"
            ),
            "http://s3.cn-south-1.qiniucs.com:8080/prefix"
        );
    }

    #[test]
    fn qiniu_endpoint_for_region_only_rewrites_the_authority() {
        // A path segment that happens to spell the source region must survive
        // untouched -- only the host's region label is a routing decision.
        assert_eq!(
            qiniu_endpoint_for_region(
                "https://s3.cn-east-1.qiniucs.com/s3.cn-east-1.",
                "cn-east-1",
                "cn-north-1"
            ),
            "https://s3.cn-north-1.qiniucs.com/s3.cn-east-1."
        );
    }

    #[test]
    fn qiniu_uses_path_style_and_keeps_batch_delete() {
        // Two sentinels against a future "let's align qiniu with oss" change:
        //
        // - Path-style: verified live 2026-07-30 that Qiniu accepts both
        //   path-style and virtual-hosted, and this app stays on path-style
        //   like every other non-AWS, non-OSS backend.
        // - Batch delete: OSS and Rainyun both hard-require the legacy
        //   `Content-MD5` header on Multi-Object Delete, which aws-sdk-s3
        //   cannot send. Qiniu does NOT -- it accepts the
        //   `x-amz-checksum-crc32` that aws-sdk-s3 actually sends (verified
        //   live 2026-07-30: 200, while the same request with no checksum
        //   header at all got `400 ... Content-MD5`). So Qiniu keeps the fast
        //   batch path.
        assert!(uses_path_style("qiniu", "https://s3.cn-east-1.qiniucs.com"));
        assert!(supports_batch_delete("qiniu"));
    }

    #[test]
    fn s3_compat_endpoint_leaves_qiniu_endpoints_untouched() {
        // The `s3.` rewrite is OSS-only. Qiniu hosts already start with `s3.`,
        // so a provider-blind rewrite would produce `s3.s3.cn-east-1...`.
        assert_eq!(
            s3_compat_endpoint("qiniu", "https://s3.cn-east-1.qiniucs.com"),
            "https://s3.cn-east-1.qiniucs.com"
        );
    }

    #[test]
    fn incorrect_region_maps_to_wrong_region() {
        // Qiniu's cross-region rejection code, distinct from OSS's
        // `PermanentRedirect` (verified live 2026-07-30).
        let app_err = classify_error_code("IncorrectRegion", Some("mybucket"));
        assert!(
            matches!(app_err, Some(AppError::WrongRegion { ref bucket }) if bucket == "mybucket"),
            "unexpected classification: {app_err:?}"
        );
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

    #[test]
    fn classify_error_code_maps_permanent_redirect_to_wrong_region() {
        // Real OSS behavior (see the M5b task-1 brief): a cross-region
        // request against the wrong region's endpoint fails with
        // `PermanentRedirect`, not `NoSuchBucket`.
        let app_err = classify_error_code("PermanentRedirect", Some("mybucket"));
        assert!(matches!(
            app_err,
            Some(AppError::WrongRegion { ref bucket }) if bucket == "mybucket"
        ));
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

    // --- fold_single_delete_outcomes (pure) ----------------------------------
    //
    // Extracted from `delete_objects`'s non-batch (OSS) path so the
    // `succeeded + failed.len() == keys.len()` invariant it must uphold is
    // unit-testable without a live network call or spawning any tasks.

    #[test]
    fn fold_single_delete_outcomes_invariant_succeeded_plus_failed_equals_total() {
        let results: Vec<(String, Result<(), AppError>)> = vec![
            ("a.txt".to_string(), Ok(())),
            ("b.txt".to_string(), Err(AppError::AccessDenied)),
            ("c.txt".to_string(), Ok(())),
            (
                "d.txt".to_string(),
                Err(AppError::Internal {
                    message: String::new(),
                }),
            ),
            ("e.txt".to_string(), Ok(())),
        ];
        let total = results.len();

        let batch = fold_single_delete_outcomes(results);

        assert_eq!(batch.succeeded as usize + batch.failed.len(), total);
        assert_eq!(batch.succeeded, 3);
        let failed_keys: Vec<&str> = batch.failed.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(failed_keys, vec!["b.txt", "d.txt"]);
        assert_eq!(batch.failed[0].code, "auth/access-denied");
        assert_eq!(batch.failed[1].code, "internal");
    }

    #[test]
    fn fold_single_delete_outcomes_all_succeeded() {
        let results: Vec<(String, Result<(), AppError>)> =
            vec![("a.txt".to_string(), Ok(())), ("b.txt".to_string(), Ok(()))];

        let batch = fold_single_delete_outcomes(results);

        assert_eq!(batch.succeeded, 2);
        assert!(batch.failed.is_empty());
    }

    #[test]
    fn fold_single_delete_outcomes_all_failed() {
        let results: Vec<(String, Result<(), AppError>)> = vec![
            ("a.txt".to_string(), Err(AppError::Timeout)),
            ("b.txt".to_string(), Err(AppError::Unreachable)),
        ];

        let batch = fold_single_delete_outcomes(results);

        assert_eq!(batch.succeeded, 0);
        assert_eq!(batch.failed.len(), 2);
    }

    #[test]
    fn fold_single_delete_outcomes_empty_input() {
        let batch = fold_single_delete_outcomes(Vec::new());
        assert_eq!(batch.succeeded, 0);
        assert!(batch.failed.is_empty());
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

    // --- clamp_expiry (pure) -------------------------------------------------

    #[test]
    fn clamp_expiry_bounds_to_one_and_seven_days() {
        assert_eq!(clamp_expiry(0), 1);
        assert_eq!(clamp_expiry(3600), 3600);
        assert_eq!(clamp_expiry(999_999_999), 604_800);
    }
}
