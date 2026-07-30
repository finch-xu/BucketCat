//! Backblaze B2 specifics that are pure string work.
//!
//! Everything here is network-free and unit-tested; the one live call to
//! Backblaze's native API lives in [`crate::provider::b2_admin`], and the S3
//! data plane needs no B2-specific code at all (verified live on 2026-07-30 --
//! see `tests/b2_e2e.rs`).
//!
//! ## The region problem this module exists to solve
//!
//! A B2 account's region is fixed when the account is created; the user cannot
//! choose it and often does not know it. Picking the wrong one is not a
//! friendly failure: verified live on 2026-07-30, signing with a `004`-cluster
//! key against `s3.us-east-005.backblazeb2.com` returns
//! `403 InvalidAccessKeyId / "The key '004...' is not valid"`. A user reading
//! "the key is not valid" goes off rebuilding credentials and never finds the
//! real cause. So BucketCat derives the region instead of asking for it.
//!
//! Two sources, in increasing order of authority:
//!
//! 1. [`b2_region_from_key_id`] -- instant, offline, but built on a convention
//!    **Backblaze has never documented** (see that function's own comment).
//! 2. [`crate::provider::b2_admin::authorize_account`] -- one HTTPS GET
//!    returning the account's own `s3ApiUrl`. Official, and future-proof in a
//!    way the table cannot be: a region Backblaze launches after this build
//!    shipped still comes back correctly, which is why [`parse_s3_api_url`]
//!    deliberately accepts regions absent from [`B2_CLUSTERS`].
//!
//! ## Why the frontend has a copy
//!
//! `src/lib/b2-regions.ts` mirrors the table and both derivations. The
//! connection form re-derives on every keystroke, so it has to run locally
//! rather than over IPC. Unlike R2 there is no *credential* derivation to keep
//! single-sourced here -- both halves are pure lookups over the same seven-row
//! table, and the tests on each side pin the same seven rows.

/// The hostname template every B2 S3 endpoint follows. Used only to *parse*
/// (see [`parse_s3_api_url`]); endpoints handed to the SDK always come from
/// [`B2_CLUSTERS`] or straight from Backblaze's own `s3ApiUrl`, never from
/// concatenation.
const B2_HOST_PREFIX: &str = "s3.";
const B2_HOST_SUFFIX: &str = ".backblazeb2.com";

/// `(cluster number, region id)` for every region Backblaze runs.
///
/// Verified 2026-07-31 by DNS: all seven resolve to real Backblaze gateways,
/// while `s3.us-west-003...`, `s3.ca-central-001...` and a made-up region name
/// are all NXDOMAIN -- Backblaze runs no wildcard DNS, so these seven are the
/// complete set as of that date.
///
/// The Rust twin of `B2_REGIONS` in `src/lib/b2-regions.ts`; the two are
/// pinned to the same seven rows by tests on both sides.
pub const B2_CLUSTERS: [(&str, &str); 7] = [
    ("000", "us-west-000"),
    ("001", "us-west-001"),
    ("002", "us-west-002"),
    ("003", "eu-central-003"),
    ("004", "us-west-004"),
    ("005", "us-east-005"),
    ("006", "ca-east-006"),
];

/// Length of a master application key id: a bare account id, 12 hex chars.
///
/// This is the gate [`b2_region_from_key_id`] uses, and it is load-bearing
/// rather than cosmetic -- see that function.
const ACCOUNT_ID_LEN: usize = 12;

/// Whether `key_id` has the shape of a **master** application key id: a bare
/// account id, exactly 12 hex characters with no cluster prefix.
///
/// Backblaze states outright that *"You cannot use your master application key
/// with the S3-Compatible API"*, and live testing on 2026-07-30 confirms the
/// failure mode is `403 InvalidAccessKeyId / "Malformed Access Key Id"` -- yet
/// another message that sends users looking in the wrong place. Detecting the
/// shape up front lets the form say what is actually wrong.
///
/// Deliberately conservative: it recognizes exactly one definite shape and
/// answers `false` whenever unsure, so a legitimate key is never blocked on a
/// guess. A wrong `false` merely means the user sees Backblaze's own error,
/// which is the status quo; a wrong `true` would refuse a working credential.
pub fn looks_like_master_key_id(key_id: &str) -> bool {
    let id = key_id.trim();
    id.len() == ACCOUNT_ID_LEN && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Derives a region id from an application key id, by table lookup.
///
/// `None` means "cannot tell" -- a truncated entry, a master key id, or a
/// cluster [`B2_CLUSTERS`] doesn't know. Callers must fall back to asking the
/// user (or to [`crate::provider::b2_admin`]) rather than guessing.
///
/// ## The convention this rests on, and its status
///
/// A B2 application key id is `{cluster}{account_id}{counter}` -- e.g.
/// `004024147e7d0760000000001` is cluster `004`, account `024147e7d076`,
/// counter `0000000001`. **Backblaze has never documented this**; both the
/// application-keys guide and the master-key help page are silent on the
/// format. It is inferred from evidence:
///
/// - A real key `004...` whose `b2_authorize_account` reports
///   `s3ApiUrl: https://s3.us-west-004.backblazeb2.com`, matching the endpoint
///   the account's own console displays (verified 2026-07-30).
/// - Backblaze's own API docs use sample ids beginning `002...` in a document
///   set whose endpoint is `s3.us-west-002.backblazeb2.com`.
///
/// Because the rule is undocumented it is used only as an instant preview;
/// `b2_probe_key` corrects it against Backblaze's authoritative answer.
///
/// ## Why the length gate is not optional
///
/// A master key id is 12 hex characters, and an account id may perfectly well
/// begin with three digits (`004abc...`). Matching on the prefix alone would
/// read such a master key as `us-west-004` and hand the user a plausible,
/// wrong endpoint -- on top of a credential the S3 API rejects anyway.
pub fn b2_region_from_key_id(key_id: &str) -> Option<&'static str> {
    let id = key_id.trim();
    if id.len() <= ACCOUNT_ID_LEN {
        return None;
    }
    let cluster = id.get(..3)?;
    if !cluster.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    B2_CLUSTERS
        .iter()
        .find(|(c, _)| *c == cluster)
        .map(|(_, region)| *region)
}

/// The S3 endpoint for a region **this build knows**, by table lookup.
///
/// `None` for anything outside [`B2_CLUSTERS`]. Callers holding a region that
/// came from Backblaze itself (via [`parse_s3_api_url`]) should use the URL
/// Backblaze returned rather than reaching for this -- that is the whole point
/// of the probe path staying authoritative.
pub fn b2_endpoint_for_region(region: &str) -> Option<String> {
    let wanted = region.trim().to_ascii_lowercase();
    B2_CLUSTERS
        .iter()
        .find(|(_, r)| *r == wanted)
        .map(|(_, r)| format!("https://{B2_HOST_PREFIX}{r}{B2_HOST_SUFFIX}"))
}

/// Recovers `(region, endpoint)` from an `s3ApiUrl` as Backblaze reports it.
///
/// **Accepts regions absent from [`B2_CLUSTERS`] on purpose.** This is the one
/// path that keeps working when Backblaze opens a region after this build
/// shipped: the probe returns `https://s3.us-west-007.backblazeb2.com`, this
/// yields `("us-west-007", "https://s3.us-west-007.backblazeb2.com")`, and the
/// connection saves and works without an app update. Validating against the
/// table here would throw that away for no gain -- Backblaze is the authority
/// on its own endpoints, not this list.
///
/// The returned endpoint is normalized to `https://{host}` (scheme, no path,
/// no port), which is exactly the shape `Connection::endpoint` holds. `None`
/// for any host that isn't `s3.<something>.backblazeb2.com`.
pub fn parse_s3_api_url(url: &str) -> Option<(String, String)> {
    let trimmed = url.trim();
    let without_scheme = trimmed.split_once("://").map_or(trimmed, |(_, rest)| rest);
    // Drop any path/query, then any `:port`, mirroring `is_aws_endpoint`'s
    // approach in `provider::s3`.
    let authority = without_scheme.split('/').next().unwrap_or("");
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    };
    let host = host.trim().to_ascii_lowercase();

    let region = host
        .strip_prefix(B2_HOST_PREFIX)?
        .strip_suffix(B2_HOST_SUFFIX)?;
    // A region label is a single hostname label. A deeper name is not a shape
    // Backblaze produces, so reject rather than guess -- silently reading
    // `a` out of `s3.a.b.backblazeb2.com` would build a working-looking
    // endpoint pointing somewhere else.
    if region.is_empty() || region.contains('.') {
        return None;
    }

    Some((
        region.to_string(),
        format!("https://{B2_HOST_PREFIX}{region}{B2_HOST_SUFFIX}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- the table --------------------------------------------------------

    /// Pins the seven regions, in order. The frontend twin
    /// (`src/lib/b2-regions.test.ts`) asserts the same seven; if Backblaze
    /// opens an eighth, both sides must be updated together.
    #[test]
    fn has_exactly_the_seven_regions_dns_confirmed() {
        assert_eq!(
            B2_CLUSTERS.map(|(_, r)| r),
            [
                "us-west-000",
                "us-west-001",
                "us-west-002",
                "eu-central-003",
                "us-west-004",
                "us-east-005",
                "ca-east-006",
            ]
        );
    }

    /// Every region id ends with its own cluster number. This does **not**
    /// license deriving one from the other by string surgery -- the table
    /// stays the single source of truth -- it only makes a mistyped row fail
    /// here instead of at a user's first request.
    #[test]
    fn every_region_id_ends_with_its_cluster_number() {
        for (cluster, region) in B2_CLUSTERS {
            assert!(
                region.ends_with(&format!("-{cluster}")),
                "region `{region}` does not end with cluster `{cluster}`"
            );
        }
    }

    #[test]
    fn cluster_numbers_are_unique() {
        let mut seen: Vec<&str> = B2_CLUSTERS.iter().map(|(c, _)| *c).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(
            before,
            seen.len(),
            "duplicate cluster number in B2_CLUSTERS"
        );
    }

    // --- looks_like_master_key_id -----------------------------------------

    /// The real account id behind the key this feature was developed against.
    /// Not a credential: an account id is public-ish (it is the `Owner/ID` in
    /// every `ListBuckets` response) and cannot authenticate anything on its
    /// own.
    #[test]
    fn recognizes_a_bare_account_id_as_a_master_key_id() {
        assert!(looks_like_master_key_id("024147e7d076"));
        assert!(looks_like_master_key_id("  024147e7d076  "));
        assert!(looks_like_master_key_id("024147E7D076"));
    }

    #[test]
    fn a_full_application_key_id_is_not_a_master_key_id() {
        assert!(!looks_like_master_key_id("004024147e7d0760000000001"));
    }

    #[test]
    fn non_hex_and_wrong_length_are_not_master_key_ids() {
        for id in [
            "",
            "024147e7d07",
            "024147e7d0760",
            "024147e7d07g",
            "hello world!",
        ] {
            assert!(
                !looks_like_master_key_id(id),
                "`{id}` must not be read as a master key id"
            );
        }
    }

    // --- b2_region_from_key_id --------------------------------------------

    /// The exact key this feature was developed against. Only its **id** is
    /// here (never the secret), and the id alone authenticates nothing.
    #[test]
    fn derives_us_west_004_from_a_real_key_id() {
        assert_eq!(
            b2_region_from_key_id("004024147e7d0760000000001"),
            Some("us-west-004")
        );
    }

    #[test]
    fn derives_every_cluster_in_the_table() {
        for (cluster, region) in B2_CLUSTERS {
            let key_id = format!("{cluster}024147e7d0760000000001");
            assert_eq!(
                b2_region_from_key_id(&key_id),
                Some(region),
                "cluster `{cluster}` should derive `{region}`"
            );
        }
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            b2_region_from_key_id("  004024147e7d0760000000001\n"),
            Some("us-west-004")
        );
    }

    /// The gate that makes the length check load-bearing: a 12-hex master key
    /// id whose account id happens to begin `004`. Reading the prefix alone
    /// would hand back `us-west-004` for a credential the S3 API rejects
    /// outright.
    #[test]
    fn a_master_key_id_starting_with_digits_does_not_derive_a_region() {
        assert_eq!(b2_region_from_key_id("004147e7d076"), None);
        assert!(looks_like_master_key_id("004147e7d076"));
    }

    #[test]
    fn an_unknown_cluster_does_not_derive_a_region() {
        assert_eq!(b2_region_from_key_id("999024147e7d0760000000001"), None);
    }

    #[test]
    fn a_non_numeric_prefix_does_not_derive_a_region() {
        assert_eq!(b2_region_from_key_id("abc024147e7d0760000000001"), None);
    }

    #[test]
    fn input_no_longer_than_a_master_key_id_does_not_derive_a_region() {
        for id in ["", "0", "004", "004024147e7d"] {
            assert_eq!(
                b2_region_from_key_id(id),
                None,
                "`{id}` is no longer than an account id and must not derive a region"
            );
        }
    }

    /// Deliberate: the cluster number is complete after three characters, so
    /// the region resolves the moment it is unambiguous rather than waiting
    /// for a full 25-character id. The only value the length gate has to
    /// exclude is a master key id, which is exactly [`ACCOUNT_ID_LEN`]
    /// characters -- one more than that and the prefix can be trusted.
    #[test]
    fn resolves_as_soon_as_the_id_outgrows_an_account_id() {
        let thirteen = &"004024147e7d0760000000001"[..ACCOUNT_ID_LEN + 1];
        assert_eq!(thirteen.len(), 13);
        assert_eq!(b2_region_from_key_id(thirteen), Some("us-west-004"));
    }

    // --- b2_endpoint_for_region -------------------------------------------

    #[test]
    fn builds_the_endpoint_for_every_known_region() {
        for (_, region) in B2_CLUSTERS {
            assert_eq!(
                b2_endpoint_for_region(region),
                Some(format!("https://s3.{region}.backblazeb2.com"))
            );
        }
    }

    #[test]
    fn region_lookup_is_case_insensitive_and_trimmed() {
        assert_eq!(
            b2_endpoint_for_region("  US-West-004 "),
            Some("https://s3.us-west-004.backblazeb2.com".to_string())
        );
    }

    #[test]
    fn an_unknown_region_has_no_table_endpoint() {
        for region in ["us-west-007", "us-west-003", "", "auto", "us-east-1"] {
            assert_eq!(
                b2_endpoint_for_region(region),
                None,
                "`{region}` is not in the table and must not produce an endpoint"
            );
        }
    }

    // --- parse_s3_api_url --------------------------------------------------

    /// The exact string `b2_authorize_account` returned on 2026-07-30.
    #[test]
    fn parses_the_s3_api_url_backblaze_actually_returns() {
        assert_eq!(
            parse_s3_api_url("https://s3.us-west-004.backblazeb2.com"),
            Some((
                "us-west-004".to_string(),
                "https://s3.us-west-004.backblazeb2.com".to_string()
            ))
        );
    }

    /// The point of the whole probe path: a region this build has never heard
    /// of still parses, so a connection to a newly-launched Backblaze region
    /// works without an app update.
    #[test]
    fn parses_a_region_that_is_not_in_the_table() {
        assert_eq!(
            parse_s3_api_url("https://s3.us-west-007.backblazeb2.com"),
            Some((
                "us-west-007".to_string(),
                "https://s3.us-west-007.backblazeb2.com".to_string()
            ))
        );
        assert_eq!(b2_endpoint_for_region("us-west-007"), None);
    }

    #[test]
    fn parsing_ignores_scheme_case_path_port_and_whitespace() {
        let expected = Some((
            "us-west-004".to_string(),
            "https://s3.us-west-004.backblazeb2.com".to_string(),
        ));
        assert_eq!(
            parse_s3_api_url("  HTTP://S3.US-West-004.BackblazeB2.com/some/path?x=1  "),
            expected
        );
        assert_eq!(
            parse_s3_api_url("s3.us-west-004.backblazeb2.com:443"),
            expected
        );
    }

    /// Round-trips with [`b2_endpoint_for_region`] for every region in the
    /// table -- the two must not drift apart.
    #[test]
    fn round_trips_with_b2_endpoint_for_region() {
        for (_, region) in B2_CLUSTERS {
            let built = b2_endpoint_for_region(region).expect("region is in the table");
            assert_eq!(
                parse_s3_api_url(&built),
                Some((region.to_string(), built.clone())),
                "round-trip failed for `{region}` via `{built}`"
            );
        }
    }

    #[test]
    fn rejects_hosts_that_are_not_b2_endpoints() {
        for host in [
            "https://s3.amazonaws.com",
            "https://oss-cn-hangzhou.aliyuncs.com",
            "https://abc.r2.cloudflarestorage.com",
            "https://api.backblazeb2.com",
            "https://f004.backblazeb2.com",
            "https://backblazeb2.com",
            "http://127.0.0.1:9000",
            "",
        ] {
            assert_eq!(
                parse_s3_api_url(host),
                None,
                "`{host}` is not a B2 S3 endpoint and must not parse as one"
            );
        }
    }

    /// `s3..backblazeb2.com` has an empty region label, and
    /// `s3.a.b.backblazeb2.com` has two -- neither is a shape Backblaze
    /// produces, so both are rejected rather than guessed at.
    #[test]
    fn rejects_empty_and_multi_label_regions() {
        assert_eq!(parse_s3_api_url("https://s3..backblazeb2.com"), None);
        assert_eq!(parse_s3_api_url("https://s3.a.b.backblazeb2.com"), None);
    }
}
