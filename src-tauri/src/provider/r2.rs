//! Cloudflare R2 specifics that are pure string/hash work.
//!
//! Everything here is network-free and unit-tested; the live Cloudflare API
//! calls live in [`crate::provider::r2_admin`], and the S3 data plane needs no
//! R2-specific code at all (verified live on 2026-07-30 -- see
//! `tests/r2_e2e.rs`).
//!
//! ## The one credential
//!
//! An R2 API token is the *only* secret a connection needs. Cloudflare
//! projects it into the three values its dashboard shows:
//!
//! - **Access Key ID** = the token's own id, which only
//!   [`crate::provider::r2_admin::verify_token`] can report (it is not
//!   derivable from the token value).
//! - **Secret Access Key** = `sha256(token value)`, computed by
//!   [`r2_secret_from_token`] with no network at all.
//! - **Bearer token** for `api.cloudflare.com` = the token value itself.
//!
//! The hash is one-way, which is exactly why `Connection::api_token` has to
//! store the token value separately: the secret access key cannot be turned
//! back into the Bearer credential the Cloudflare API requires.
//!
//! ## Jurisdictions are not regions
//!
//! R2's `eu` and `fedramp` endpoints are **separate namespaces**, not
//! alternate routes to the same data. Verified live: the same key lists two
//! buckets on the default endpoint and zero on the `eu` one, and asking the
//! `eu` endpoint for a default-jurisdiction bucket returns `404 NoSuchBucket`
//! (not a redirect, not a 403). So R2 deliberately gets **no**
//! [`crate::provider::s3::RegionRouting`]: there is nothing to route between,
//! and a bucket in another jurisdiction is simply invisible to this
//! connection. The SigV4 region is `auto` and R2 does not validate it at all
//! (signing for `us-east-1` or `wnam` against the same host both return 200).

use sha2::{Digest, Sha256};

/// The R2 endpoint hostname suffix every jurisdiction shares.
const R2_HOST_SUFFIX: &str = ".r2.cloudflarestorage.com";

/// The SigV4 region R2 documents. R2 ignores the value entirely (verified
/// live), but this is what the connection form stores so the UI reads
/// correctly and so a future R2 change to *start* validating it finds the
/// documented value already in place.
pub const R2_REGION: &str = "auto";

/// Every jurisdiction id BucketCat offers, in the order the picker renders
/// them.
///
/// The empty id is the default (worldwide) jurisdiction, whose endpoint
/// carries no jurisdiction label at all. Labels are i18n keys on the frontend,
/// not stored here -- this list exists so [`parse_r2_endpoint`]'s round-trip
/// test can enumerate what the app actually offers.
pub const R2_JURISDICTIONS: [&str; 3] = ["", "eu", "fedramp"];

/// Derives the S3 Secret Access Key from an R2 API token value:
/// lowercase-hex SHA-256, no network.
///
/// Verified against two real tokens of different privilege tiers on
/// 2026-07-30 -- the derivation is identical regardless of what the token is
/// allowed to do.
pub fn r2_secret_from_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.trim().as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Builds R2's S3 endpoint for an account id and jurisdiction.
///
/// The default jurisdiction (empty string) gets no label:
/// `https://{account}.r2.cloudflarestorage.com`. Every other jurisdiction
/// becomes its own hostname label:
/// `https://{account}.eu.r2.cloudflarestorage.com`.
///
/// Inputs are trimmed and lowercased -- account ids are hex and jurisdiction
/// ids are lowercase ASCII, so there is no information in their case, and a
/// pasted value with stray whitespace is a normal copy/paste artifact rather
/// than a distinct endpoint.
pub fn r2_endpoint(account_id: &str, jurisdiction: &str) -> String {
    let account = account_id.trim().to_ascii_lowercase();
    let juris = jurisdiction.trim().to_ascii_lowercase();
    if juris.is_empty() {
        format!("https://{account}{R2_HOST_SUFFIX}")
    } else {
        format!("https://{account}.{juris}{R2_HOST_SUFFIX}")
    }
}

/// The inverse of [`r2_endpoint`]: recovers `(account_id, jurisdiction)` from
/// a saved endpoint so the edit form can prefill its account/jurisdiction
/// controls.
///
/// Returns `None` for anything that isn't an R2 endpoint -- a custom domain, a
/// typo, another provider's host. Callers must treat that as "this connection
/// has an endpoint I can't model", the same way
/// [`crate::provider::s3::s3_compat_endpoint`] leaves unrecognized hosts
/// alone, and fall back to a free-text endpoint field rather than guessing.
///
/// The jurisdiction is returned **verbatim** rather than validated against
/// [`R2_JURISDICTIONS`]: a jurisdiction Cloudflare adds after this build
/// shipped should still round-trip through the form instead of being silently
/// rewritten to the default. Callers that need to render a picker check
/// membership themselves.
pub fn parse_r2_endpoint(endpoint: &str) -> Option<(String, String)> {
    let without_scheme = endpoint
        .trim()
        .split_once("://")
        .map_or(endpoint.trim(), |(_, rest)| rest);
    // Drop any path/query, then any `:port`, mirroring `is_aws_endpoint`'s
    // approach in `provider::s3`.
    let authority = without_scheme.split('/').next().unwrap_or("");
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    };
    let host = host.trim().to_ascii_lowercase();

    let head = host.strip_suffix(R2_HOST_SUFFIX)?;
    if head.is_empty() {
        return None;
    }

    match head.split_once('.') {
        // `{account}.{jurisdiction}` -- both labels must be non-empty, and
        // there must be exactly one dot between them. A deeper name is not an
        // endpoint shape R2 produces, so it is rejected rather than guessed at.
        Some((account, juris)) => {
            if account.is_empty() || juris.is_empty() || juris.contains('.') {
                None
            } else {
                Some((account.to_string(), juris.to_string()))
            }
        }
        // `{account}` alone -- the default jurisdiction.
        None => Some((head.to_string(), String::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- r2_secret_from_token ---------------------------------------------

    /// Pins the derivation to **standard** SHA-256 using FIPS 180-4's own
    /// published test vector for `"abc"`, not to whatever `sha2` happens to
    /// compute. That distinction is the whole point: Cloudflare's contract is
    /// "the secret access key is the SHA-256 of the token value", so a
    /// dependency swapping in a different digest (or this function growing a
    /// salt/prefix) must fail here rather than at a user's first signed
    /// request.
    ///
    /// No real token appears in this repo. The live derivation was verified
    /// against two real Cloudflare tokens of different privilege tiers on
    /// 2026-07-30, and `tests/r2_e2e.rs` re-verifies it end to end by signing
    /// an actual request with a derived secret.
    #[test]
    fn derives_standard_sha256() {
        assert_eq!(
            r2_secret_from_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn secret_is_lowercase_hex_of_exactly_32_bytes() {
        let secret = r2_secret_from_token("cfut_anything");
        assert_eq!(secret.len(), 64, "SHA-256 hex is always 64 characters");
        assert!(
            secret
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "the secret must be lowercase hex, got {secret}"
        );
    }

    /// A pasted token routinely carries a trailing newline or space. Hashing
    /// that whitespace would produce a secret that silently fails to sign,
    /// with no way for the user to tell why.
    #[test]
    fn whitespace_around_the_token_is_trimmed_before_hashing() {
        let clean = r2_secret_from_token("cfut_token");
        assert_eq!(r2_secret_from_token("  cfut_token"), clean);
        assert_eq!(r2_secret_from_token("cfut_token\n"), clean);
        assert_eq!(r2_secret_from_token(" cfut_token \r\n"), clean);
    }

    #[test]
    fn different_tokens_hash_differently() {
        assert_ne!(
            r2_secret_from_token("cfut_a"),
            r2_secret_from_token("cfut_b")
        );
    }

    // --- r2_endpoint ------------------------------------------------------

    #[test]
    fn default_jurisdiction_has_no_label() {
        assert_eq!(
            r2_endpoint("a1b2c3d4e5f60718293a4b5c6d7e8f90", ""),
            "https://a1b2c3d4e5f60718293a4b5c6d7e8f90.r2.cloudflarestorage.com"
        );
    }

    #[test]
    fn eu_jurisdiction_gets_its_own_label() {
        assert_eq!(
            r2_endpoint("a1b2c3d4e5f60718293a4b5c6d7e8f90", "eu"),
            "https://a1b2c3d4e5f60718293a4b5c6d7e8f90.eu.r2.cloudflarestorage.com"
        );
    }

    #[test]
    fn fedramp_jurisdiction_gets_its_own_label() {
        assert_eq!(
            r2_endpoint("abc", "fedramp"),
            "https://abc.fedramp.r2.cloudflarestorage.com"
        );
    }

    #[test]
    fn account_and_jurisdiction_are_trimmed_and_lowercased() {
        assert_eq!(
            r2_endpoint("  ABC123 ", " EU "),
            "https://abc123.eu.r2.cloudflarestorage.com"
        );
    }

    // --- parse_r2_endpoint -------------------------------------------------

    #[test]
    fn parses_a_default_jurisdiction_endpoint() {
        assert_eq!(
            parse_r2_endpoint("https://a1b2c3d4e5f60718293a4b5c6d7e8f90.r2.cloudflarestorage.com"),
            Some((
                "a1b2c3d4e5f60718293a4b5c6d7e8f90".to_string(),
                String::new()
            ))
        );
    }

    #[test]
    fn parses_an_eu_jurisdiction_endpoint() {
        assert_eq!(
            parse_r2_endpoint(
                "https://a1b2c3d4e5f60718293a4b5c6d7e8f90.eu.r2.cloudflarestorage.com"
            ),
            Some((
                "a1b2c3d4e5f60718293a4b5c6d7e8f90".to_string(),
                "eu".to_string()
            ))
        );
    }

    /// The single most useful paste in the whole form: users copy the endpoint
    /// straight out of the R2 dashboard, which shows it without a scheme.
    #[test]
    fn parses_a_schemeless_host() {
        assert_eq!(
            parse_r2_endpoint("abc.r2.cloudflarestorage.com"),
            Some(("abc".to_string(), String::new()))
        );
    }

    #[test]
    fn parsing_ignores_scheme_case_path_port_and_whitespace() {
        let expected = Some(("abc".to_string(), "eu".to_string()));
        assert_eq!(
            parse_r2_endpoint("  HTTPS://ABC.EU.R2.CloudflareStorage.com/some/path?x=1  "),
            expected
        );
        assert_eq!(
            parse_r2_endpoint("http://abc.eu.r2.cloudflarestorage.com:443"),
            expected
        );
    }

    /// Round-trips with [`r2_endpoint`] for every jurisdiction the app offers
    /// -- the two functions are each other's inverse and must not drift apart.
    #[test]
    fn round_trips_with_r2_endpoint_for_every_jurisdiction() {
        for juris in R2_JURISDICTIONS {
            let built = r2_endpoint("a1b2c3d4e5f60718293a4b5c6d7e8f90", juris);
            assert_eq!(
                parse_r2_endpoint(&built),
                Some((
                    "a1b2c3d4e5f60718293a4b5c6d7e8f90".to_string(),
                    juris.to_string()
                )),
                "round-trip failed for jurisdiction `{juris}` via `{built}`"
            );
        }
    }

    /// An unknown jurisdiction label round-trips verbatim rather than being
    /// normalized away -- see the function's doc comment for why.
    #[test]
    fn an_unrecognized_jurisdiction_label_is_returned_verbatim() {
        assert_eq!(
            parse_r2_endpoint("https://abc.apac.r2.cloudflarestorage.com"),
            Some(("abc".to_string(), "apac".to_string()))
        );
    }

    #[test]
    fn rejects_hosts_that_are_not_r2_endpoints() {
        for host in [
            "https://s3.amazonaws.com",
            "https://oss-cn-hangzhou.aliyuncs.com",
            "https://files.example.com",
            "https://cdn.example.com",
            "http://127.0.0.1:9000",
            "",
        ] {
            assert_eq!(
                parse_r2_endpoint(host),
                None,
                "`{host}` is not an R2 endpoint and must not parse as one"
            );
        }
    }

    /// The suffix alone, with no account label, is not a usable endpoint.
    #[test]
    fn rejects_the_bare_suffix() {
        assert_eq!(parse_r2_endpoint("https://r2.cloudflarestorage.com"), None);
    }

    /// R2 never produces a three-label name, so this is rejected rather than
    /// guessed at -- silently reading `a` as the account would build a working
    /// -looking endpoint that points somewhere else entirely.
    #[test]
    fn rejects_a_host_with_too_many_labels() {
        assert_eq!(
            parse_r2_endpoint("https://a.b.c.r2.cloudflarestorage.com"),
            None
        );
    }
}
