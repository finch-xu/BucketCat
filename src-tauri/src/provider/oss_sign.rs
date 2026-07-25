//! Aliyun OSS's `OSS4-HMAC-SHA256` V4 request signer.
//!
//! Pure, network-free functions only -- everything here operates on plain
//! strings/bytes and returns a value; nothing in this module builds an HTTP
//! request or touches the network (that's [`crate::provider::oss_admin`]).
//! That split is what makes the algorithm directly testable against the two
//! official known-answer vectors in this module's tests: Alibaba's own
//! documentation example (`kat_1_*`) and the official Go SDK v2's test
//! vector (`kat_2_*`), both reproduced byte-for-byte.
//!
//! The algorithm (CanonicalRequest layout, StringToSign, the chained-HMAC
//! SigningKey derivation, and the Authorization header shape) is specified
//! by the M5b task-1 brief and has already been cross-checked by the task
//! author against Alibaba's official docs, the official Go SDK's test
//! vector, and a live OSS account -- it is implemented here exactly as
//! written. If a KAT assertion below ever fails, the implementation is
//! wrong; the expected value must never be adjusted to match it.
//!
//! **Security**: every value derived from an `AccessKeySecret` in this
//! module (the chained HMAC `SigningKey`, the final signature) is plain
//! `[u8; 32]` / `String` data -- there is no long-lived struct here to carry
//! a secret across calls, so there is nothing to redact `Debug` for. Callers
//! (in particular [`crate::provider::s3`]'s `S3Provider`) are responsible
//! for never logging the `AccessKeySecret` they pass into
//! [`signing_key`], and for never `Debug`-printing the `x-oss-date`-adjacent
//! `Authorization` header value this module produces (it embeds the live
//! signature).

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Percent-encodes `input` byte-by-byte: RFC 3986 unreserved characters
/// (`A-Za-z0-9-._~`) -- and `/` when `keep_slash` is set -- pass through
/// unchanged; everything else becomes `%XX` with uppercase hex digits, one
/// escape per input *byte* (so multi-byte UTF-8 characters, e.g. Chinese
/// object names, encode correctly as multiple `%XX` triplets).
fn percent_encode(input: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        let is_unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if is_unreserved || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Lowercase-hex encoding of `bytes` (used for the SHA256/HMAC digests this
/// module produces -- OSS's signature material is always lowercase hex,
/// unlike the uppercase hex [`percent_encode`] uses for URI escaping).
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// `CanonicalURI`: `/` + `bucket` + `/` + `key`, percent-encoded (keeping
/// `/` literal). `bucket` empty means "no bucket" (account-level operations
/// like the native `ListBuckets` this task adds), which is just `/` --
/// `key` is ignored in that case.
pub fn canonical_uri(bucket: &str, key: &str) -> String {
    if bucket.is_empty() {
        return "/".to_string();
    }
    percent_encode(&format!("/{bucket}/{key}"), true)
}

/// `CanonicalQueryString`: each raw `(key, value)` pair is percent-encoded
/// independently (no `/` exception -- unlike [`canonical_uri`], these are
/// standalone query components, not a path), then pairs are sorted
/// ascending by their *encoded* key. A pair whose value encodes to the
/// empty string contributes just its encoded key (no trailing `=`).
pub fn canonical_query_string(pairs: &[(&str, &str)]) -> String {
    let mut encoded: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (percent_encode(k, false), percent_encode(v, false)))
        .collect();
    encoded.sort_by(|a, b| a.0.cmp(&b.0));
    encoded
        .into_iter()
        .map(|(k, v)| if v.is_empty() { k } else { format!("{k}={v}") })
        .collect::<Vec<_>>()
        .join("&")
}

/// Whether header `name` (any case) is part of OSS's V4 signed-header set:
/// every `x-oss-*` header, plus `content-type` and `content-md5`. Every
/// other header -- **including `host`**, which AWS SigV4 signs but OSS's
/// V4 variant deliberately does not -- must never be signed unless the
/// caller explicitly lists it as an additional header.
pub fn is_signable_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("x-oss-") || lower == "content-type" || lower == "content-md5"
}

/// Filters `headers` down to the ones [`is_signable_header`] accepts,
/// lower-casing each name and trimming its value -- ready to hand to
/// [`canonical_request`]. Headers outside that set (arbitrary caller
/// headers like `Host`, or anything the caller didn't explicitly add to
/// `additional_headers`) are dropped, never signed.
pub fn signable_headers(headers: &[(&str, &str)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| is_signable_header(name))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect()
}

/// Builds the `CanonicalRequest` string, byte-for-byte per the M5b task-1
/// brief:
///
/// ```text
/// {METHOD}\n
/// {CanonicalURI}\n
/// {CanonicalQueryString}\n
/// {each signed header, "lowercase-name:trimmed-value\n", ascending by name}
/// \n                                                     <- blank line
/// {AdditionalHeaders: ';'-joined names ascending, empty if none}\n
/// {HashedPayload}                                        <- no trailing \n
/// ```
///
/// `signed_headers` need not already be lower-cased/trimmed/sorted -- this
/// normalizes and sorts them itself (this is the layer [`signable_headers`]
/// feeds into, but the two are independently testable). `additional_headers`
/// is likewise sorted here regardless of input order.
pub fn canonical_request(
    method: &str,
    canonical_uri: &str,
    canonical_query_string: &str,
    signed_headers: &[(&str, &str)],
    additional_headers: &[&str],
    hashed_payload: &str,
) -> String {
    let mut headers: Vec<(String, String)> = signed_headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    headers.sort_by(|a, b| a.0.cmp(&b.0));

    let mut additional: Vec<String> = additional_headers
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .collect();
    additional.sort();

    let mut out = String::new();
    out.push_str(method);
    out.push('\n');
    out.push_str(canonical_uri);
    out.push('\n');
    out.push_str(canonical_query_string);
    out.push('\n');
    for (name, value) in &headers {
        out.push_str(name);
        out.push(':');
        out.push_str(value);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&additional.join(";"));
    out.push('\n');
    out.push_str(hashed_payload);
    out
}

/// `HEX(SHA256(data))`, lowercase.
pub fn sha256_hex(data: &[u8]) -> String {
    to_hex(&Sha256::digest(data))
}

/// `StringToSign`:
/// ```text
/// OSS4-HMAC-SHA256\n
/// {x_oss_date}\n
/// {date}/{region}/oss/aliyun_v4_request\n
/// {canonical_request_hash_hex}
/// ```
pub fn string_to_sign(
    canonical_request_hash_hex: &str,
    x_oss_date: &str,
    date: &str,
    region: &str,
) -> String {
    format!(
        "OSS4-HMAC-SHA256\n{x_oss_date}\n{date}/{region}/oss/aliyun_v4_request\n{canonical_request_hash_hex}"
    )
}

/// `HMAC-SHA256(key, data)`. `hmac::Hmac::new_from_slice` accepts a key of
/// any length (it's internally padded/hashed per RFC 2104), so this never
/// fails.
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <HmacSha256 as KeyInit>::new_from_slice(key)
        .expect("HMAC-SHA256 accepts a key of any length");
    mac.update(data);
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

/// Derives the chained-HMAC `SigningKey` from the raw `AccessKeySecret`:
/// ```text
/// k1 = HMAC("aliyun_v4" + AccessKeySecret, yyyymmdd)
/// k2 = HMAC(k1, region)
/// k3 = HMAC(k2, "oss")
/// SigningKey = HMAC(k3, "aliyun_v4_request")
/// ```
///
/// **Security**: the returned bytes are signing key material derived from
/// the account secret -- callers must never `Debug`-print, log, or send it
/// anywhere except into [`signature_hex`].
pub fn signing_key(secret: &str, date: &str, region: &str) -> [u8; 32] {
    let k1 = hmac_sha256(format!("aliyun_v4{secret}").as_bytes(), date.as_bytes());
    let k2 = hmac_sha256(&k1, region.as_bytes());
    let k3 = hmac_sha256(&k2, b"oss");
    hmac_sha256(&k3, b"aliyun_v4_request")
}

/// `Signature = HEX(HMAC(SigningKey, StringToSign))`.
pub fn signature_hex(signing_key: &[u8], string_to_sign: &str) -> String {
    to_hex(&hmac_sha256(signing_key, string_to_sign.as_bytes()))
}

/// Builds the `Authorization` header value:
/// ```text
/// OSS4-HMAC-SHA256 Credential={AK}/{date}/{region}/oss/aliyun_v4_request,Signature={sig}
/// ```
/// or, when `additional_headers` is non-empty, with an `AdditionalHeaders`
/// segment inserted between `Credential` and `Signature` (';'-joined names,
/// ascending):
/// ```text
/// OSS4-HMAC-SHA256 Credential={AK}/{date}/{region}/oss/aliyun_v4_request,AdditionalHeaders={names};,Signature={sig}
/// ```
///
/// **Security**: the returned string embeds a live request signature --
/// callers must never log it.
pub fn authorization_header(
    access_key_id: &str,
    date: &str,
    region: &str,
    signature_hex: &str,
    additional_headers: &[&str],
) -> String {
    let credential = format!("{access_key_id}/{date}/{region}/oss/aliyun_v4_request");

    let mut additional: Vec<String> = additional_headers
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .collect();
    additional.sort();

    if additional.is_empty() {
        format!("OSS4-HMAC-SHA256 Credential={credential},Signature={signature_hex}")
    } else {
        let joined = additional.join(";");
        format!(
            "OSS4-HMAC-SHA256 Credential={credential},AdditionalHeaders={joined};,Signature={signature_hex}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes a lowercase-hex string into bytes. Test-only: production
    /// code never needs to parse a hex-encoded signing key back out (the
    /// KAT-1 vector hands us one as a fixture, real signing always derives
    /// it via [`signing_key`]).
    fn decode_hex(hex: &str) -> Vec<u8> {
        assert_eq!(hex.len() % 2, 0, "hex string must have an even length");
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("valid hex pair"))
            .collect()
    }

    // --- KAT-1: Alibaba's official documentation example -----------------
    // Validates CanonicalRequest normalization and StringToSign/Signature,
    // using an officially-published raw SigningKey (not derived here).

    #[test]
    fn kat_1_canonical_request_hash() {
        let canonical_uri = "/examplebucket/exampleobject";
        let canonical_query_string = "";
        let signed_headers: [(&str, &str); 6] = [
            ("content-disposition", "attachment"),
            ("content-length", "3"),
            ("content-md5", "ICy5YqxZB1uWSwcVLSNLcA=="),
            ("content-type", "text/plain"),
            ("x-oss-content-sha256", "UNSIGNED-PAYLOAD"),
            ("x-oss-date", "20250411T064124Z"),
        ];
        let additional_headers = ["content-disposition", "content-length"];

        let cr = canonical_request(
            "PUT",
            canonical_uri,
            canonical_query_string,
            &signed_headers,
            &additional_headers,
            "UNSIGNED-PAYLOAD",
        );
        let hash = sha256_hex(cr.as_bytes());

        assert_eq!(
            hash,
            "c46d96390bdbc2d739ac9363293ae9d710b14e48081fcb22cd8ad54b63136eca"
        );
    }

    #[test]
    fn kat_1_signature_with_official_signing_key() {
        let canonical_request_hash =
            "c46d96390bdbc2d739ac9363293ae9d710b14e48081fcb22cd8ad54b63136eca";
        let sts = string_to_sign(
            canonical_request_hash,
            "20250411T064124Z",
            "20250411",
            "cn-hangzhou",
        );

        let signing_key =
            decode_hex("3543b7686e65eda71e5e5ca19d548d78423c37e8ddba4dc9d83f90228b457c76");
        let sig = signature_hex(&signing_key, &sts);

        assert_eq!(
            sig,
            "053edbf550ebd239b32a9cdfd93b0b2b3f2d223083aa61f75e9ac16856d61f23"
        );
    }

    // --- KAT-2: Aliyun's official Go SDK v2 test vector --------------------
    // Validates the full chain (CanonicalURI escaping, CanonicalQueryString
    // normalization/sorting, header filtering, and the chained-HMAC
    // SigningKey derivation from a plaintext AK/SK) end-to-end down to the
    // final Authorization header.

    #[test]
    fn kat_2_canonical_uri_escapes_reserved_bytes() {
        assert_eq!(
            canonical_uri("bucket", "1234+-/123/1.txt"),
            "/bucket/1234%2B-/123/1.txt"
        );
    }

    #[test]
    fn kat_2_canonical_query_string_encodes_sorts_and_drops_bare_equals() {
        let pairs: [(&str, &str); 6] = [
            ("param1", "value1"),
            ("+param1", "value3"),
            ("|param1", "value4"),
            ("+param2", ""),
            ("|param2", ""),
            ("param2", ""),
        ];
        assert_eq!(
            canonical_query_string(&pairs),
            "%2Bparam1=value3&%2Bparam2&%7Cparam1=value4&%7Cparam2&param1=value1&param2"
        );
    }

    #[test]
    fn kat_2_signable_headers_drops_non_oss_headers() {
        // `abc`/`ZAbc`/`XYZ` are present on the real request but are not
        // `x-oss-*`/`content-type`/`content-md5` -- they must never be
        // signed. Deliberately unsorted/mixed-case input to also prove
        // `signable_headers` lower-cases names.
        let all: [(&str, &str); 7] = [
            ("XYZ", "3"),
            ("x-oss-head1", "value"),
            ("abc", "1"),
            ("content-type", "text/plain"),
            ("ZAbc", "2"),
            ("x-oss-date", "20231216T162057Z"),
            ("x-oss-content-sha256", "UNSIGNED-PAYLOAD"),
        ];
        let mut filtered = signable_headers(&all);
        filtered.sort();
        assert_eq!(
            filtered,
            vec![
                ("content-type".to_string(), "text/plain".to_string()),
                (
                    "x-oss-content-sha256".to_string(),
                    "UNSIGNED-PAYLOAD".to_string()
                ),
                ("x-oss-date".to_string(), "20231216T162057Z".to_string()),
                ("x-oss-head1".to_string(), "value".to_string()),
            ]
        );
    }

    #[test]
    fn kat_2_full_chain_produces_official_authorization_header() {
        let canonical_uri = canonical_uri("bucket", "1234+-/123/1.txt");
        let canonical_query_string = canonical_query_string(&[
            ("param1", "value1"),
            ("+param1", "value3"),
            ("|param1", "value4"),
            ("+param2", ""),
            ("|param2", ""),
            ("param2", ""),
        ]);
        let signed_headers: [(&str, &str); 4] = [
            ("content-type", "text/plain"),
            ("x-oss-content-sha256", "UNSIGNED-PAYLOAD"),
            ("x-oss-date", "20231216T162057Z"),
            ("x-oss-head1", "value"),
        ];

        let cr = canonical_request(
            "PUT",
            &canonical_uri,
            &canonical_query_string,
            &signed_headers,
            &[],
            "UNSIGNED-PAYLOAD",
        );
        let cr_hash = sha256_hex(cr.as_bytes());
        let sts = string_to_sign(&cr_hash, "20231216T162057Z", "20231216", "cn-hangzhou");
        let key = signing_key("sk", "20231216", "cn-hangzhou");
        let sig = signature_hex(&key, &sts);
        let auth = authorization_header("ak", "20231216", "cn-hangzhou", &sig, &[]);

        assert_eq!(
            auth,
            "OSS4-HMAC-SHA256 Credential=ak/20231216/cn-hangzhou/oss/aliyun_v4_request,Signature=e21d18daa82167720f9b1047ae7e7f1ce7cb77a31e8203a7d5f4624fa0284afe"
        );
    }

    // --- Additional coverage: AdditionalHeaders in the Authorization header
    // (not exercised by either official KAT -- both have an empty
    // AdditionalHeaders set -- so this pins the brief's literal template
    // for the non-empty case instead).

    #[test]
    fn authorization_header_includes_additional_headers_segment() {
        let header = authorization_header(
            "ak",
            "20231216",
            "cn-hangzhou",
            "deadbeef",
            &["x-oss-head1", "content-disposition"],
        );
        assert_eq!(
            header,
            "OSS4-HMAC-SHA256 Credential=ak/20231216/cn-hangzhou/oss/aliyun_v4_request,AdditionalHeaders=content-disposition;x-oss-head1;,Signature=deadbeef"
        );
    }

    #[test]
    fn canonical_uri_with_no_bucket_is_root() {
        assert_eq!(canonical_uri("", ""), "/");
        assert_eq!(canonical_uri("", "ignored-key"), "/");
    }

    #[test]
    fn is_signable_header_excludes_host() {
        assert!(!is_signable_header("host"));
        assert!(!is_signable_header("Host"));
        assert!(is_signable_header("X-Oss-Date"));
        assert!(is_signable_header("Content-Type"));
        assert!(is_signable_header("Content-MD5"));
    }
}
