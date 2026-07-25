//! Aliyun OSS's *native* `ListBuckets` API (`GET /` against the native,
//! non-`s3.`-prefixed OSS endpoint).
//!
//! Unlike the S3-compatible `ListBuckets` [`crate::provider::s3::S3Provider`]
//! otherwise calls through `aws-sdk-s3`, this native endpoint is
//! account-level and cross-region: called against any one region's
//! endpoint, it still reports every bucket the account owns, each with its
//! own `<Region>` -- exactly the information a BucketCat connection (which
//! only has one configured region) needs to detect and eventually route
//! around OSS's "wrong region" `PermanentRedirect` failure (see
//! [`crate::error::AppError::WrongRegion`]).
//!
//! Requests are signed with [`crate::provider::oss_sign`] and sent with a
//! small `hyper` + `hyper-rustls` client (see that module's Cargo.toml
//! comment for why this isn't built on `reqwest`).

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use quick_xml::events::{BytesText, Event};
use quick_xml::reader::Reader;

use crate::error::{AppError, AppResult};
use crate::provider::oss_sign;
use crate::provider::s3::classify_error_code;

/// The OSS date/time format `x-oss-date` uses: `20250411T064124Z`. Built
/// from [`std::time::SystemTime`] rather than `chrono` (task constraint: no
/// new dependency-graph nodes, and `chrono` isn't already resolved).
const OSS_DATE_FORMAT_LEN: usize = "20250411T064124Z".len();

/// One bucket as reported by OSS's native `ListBuckets`, region included.
/// Deliberately a plain DTO (mirrors [`crate::provider::Bucket`]'s own
/// discipline) -- no `quick_xml`/`hyper` type crosses this boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OssBucket {
    pub name: String,
    pub region: String,
    pub creation_date: Option<String>,
}

/// Decodes a `quick_xml` text event to a plain `String`: charset-decode
/// (`BytesText::decode`) then XML-entity-unescape (`&amp;` -> `&`, etc. --
/// `quick_xml::escape::unescape`, since `BytesText` itself has no
/// `unescape()` method in this quick-xml version).
fn xml_text(t: &BytesText<'_>) -> AppResult<String> {
    let decoded = t.decode().map_err(|e| AppError::Internal {
        message: format!("oss list-buckets: xml decode error: {e}"),
    })?;
    let unescaped = quick_xml::escape::unescape(&decoded).map_err(|e| AppError::Internal {
        message: format!("oss list-buckets: xml unescape error: {e}"),
    })?;
    Ok(unescaped.into_owned())
}

/// Parses OSS's `ListAllMyBucketsResult` XML body into [`OssBucket`]s.
///
/// Pure and network-free -- exercised directly by this module's tests
/// against fixture XML strings, no live OSS account required.
///
/// - A `<Bucket>` missing `<Region>` falls back to its `<Location>` (e.g.
///   `oss-cn-beijing`) with the `oss-` prefix stripped; a bucket with
///   neither gets an empty-string region.
/// - Anything that isn't a `ListAllMyBucketsResult` document (in particular
///   an OSS `<Error>` response body) is rejected as `Err` -- callers must
///   check the HTTP status before calling this, but this is a second,
///   independent guard against silently treating an error body as an empty
///   bucket list.
pub fn parse_list_all_my_buckets(xml: &str) -> AppResult<Vec<OssBucket>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buckets = Vec::new();
    let mut saw_root = false;

    let mut in_bucket = false;
    let mut current_tag: Vec<u8> = Vec::new();
    let mut name: Option<String> = None;
    let mut location: Option<String> = None;
    let mut region: Option<String> = None;
    let mut creation_date: Option<String> = None;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let tag = e.name().as_ref().to_vec();
                match tag.as_slice() {
                    b"ListAllMyBucketsResult" => saw_root = true,
                    b"Bucket" => {
                        in_bucket = true;
                        name = None;
                        location = None;
                        region = None;
                        creation_date = None;
                    }
                    _ => {}
                }
                current_tag = tag;
            }
            Ok(Event::Text(t)) if in_bucket => {
                let text = xml_text(&t)?;
                match current_tag.as_slice() {
                    b"Name" => name = Some(text),
                    b"Location" => location = Some(text),
                    b"Region" => region = Some(text),
                    b"CreationDate" => creation_date = Some(text),
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"Bucket" && in_bucket {
                    let Some(bucket_name) = name.take() else {
                        return Err(AppError::Internal {
                            message: "oss list-buckets: <Bucket> missing <Name>".to_string(),
                        });
                    };
                    let resolved_region = region.take().unwrap_or_default();
                    let resolved_region = if resolved_region.is_empty() {
                        location
                            .take()
                            .map(|loc| loc.strip_prefix("oss-").map(str::to_string).unwrap_or(loc))
                            .unwrap_or_default()
                    } else {
                        resolved_region
                    };
                    buckets.push(OssBucket {
                        name: bucket_name,
                        region: resolved_region,
                        creation_date: creation_date.take(),
                    });
                    in_bucket = false;
                }
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(AppError::Internal {
                    message: format!("oss list-buckets: xml parse error: {e}"),
                })
            }
            _ => {}
        }
        buf.clear();
    }

    if !saw_root {
        return Err(AppError::Internal {
            message: "oss list-buckets: unexpected xml response (no ListAllMyBucketsResult root)"
                .to_string(),
        });
    }

    Ok(buckets)
}

/// Extracts `<Code>` and `<Message>` from an OSS `<Error>` XML body (best
/// effort -- returns `None`/`None` for anything that doesn't parse or has
/// no `<Code>`). Never logs or otherwise surfaces the full body: only these
/// two already-public-by-design fields are pulled out of it.
fn parse_error_fields(xml: &str) -> (Option<String>, Option<String>) {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut code: Option<String> = None;
    let mut message: Option<String> = None;
    let mut current_tag: Vec<u8> = Vec::new();

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => current_tag = e.name().as_ref().to_vec(),
            Ok(Event::Text(t)) => {
                let Ok(text) = xml_text(&t) else { continue };
                match current_tag.as_slice() {
                    b"Code" if code.is_none() => code = Some(text),
                    b"Message" if message.is_none() => message = Some(text),
                    _ => {}
                }
            }
            Ok(Event::End(_)) => current_tag.clear(),
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    (code, message)
}

/// Formats the current UTC time as OSS's `x-oss-date` format
/// (`%Y%m%dT%H%M%SZ`) plus its leading `yyyymmdd` date component, using only
/// [`std::time::SystemTime`] (no `chrono` -- see this module's Cargo.toml
/// comment on the dependency-graph constraint). A tiny hand-rolled
/// Gregorian civil-calendar conversion; UTC only, no timezone handling
/// needed since OSS always signs against UTC.
fn now_oss_date() -> (String, String) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let (y, m, d, hh, mm, ss) = civil_from_unix_seconds(now.as_secs() as i64);
    let x_oss_date = format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z");
    debug_assert_eq!(x_oss_date.len(), OSS_DATE_FORMAT_LEN);
    let date = format!("{y:04}{m:02}{d:02}");
    (x_oss_date, date)
}

/// Howard Hinnant's `civil_from_days` algorithm (public domain), extended
/// with the trivial seconds-of-day breakdown, to convert a Unix timestamp
/// into `(year, month, day, hour, minute, second)` in UTC without pulling in
/// a date/time crate. <http://howardhinnant.github.io/date_algorithms.html>
fn civil_from_unix_seconds(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (
        (sod / 3600) as u32,
        ((sod / 60) % 60) as u32,
        (sod % 60) as u32,
    );

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };

    (y, m, d, hh, mm, ss)
}

/// Builds the `hyper` HTTPS client used for the one-shot native `ListBuckets`
/// request. Explicit `builder_with_provider` (rather than relying on a
/// process-wide default `CryptoProvider`) so this never depends on some
/// other component (e.g. `aws-sdk-s3`'s own transport) having already
/// installed one -- and never races it if not.
fn build_https_client() -> AppResult<
    Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        Empty<Bytes>,
    >,
> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let connector = HttpsConnectorBuilder::new()
        .with_provider_and_native_roots(provider)
        .map_err(|e| AppError::Internal {
            message: format!("oss list-buckets: tls setup failed: {e}"),
        })?
        .https_only()
        .enable_http1()
        .build();
    Ok(Client::builder(TokioExecutor::new()).build(connector))
}

/// Calls OSS's native `GET {endpoint}/` (`ListBuckets`), account-level and
/// cross-region -- see the module doc comment. `endpoint` must be the
/// *native* OSS endpoint (e.g. `https://oss-cn-hangzhou.aliyuncs.com`), NOT
/// the `s3.`-prefixed S3-compatible one `S3Provider`'s `aws-sdk-s3` client
/// uses.
///
/// **Security**: `sk` (the `AccessKeySecret`) is used only to derive a
/// signing key ([`oss_sign::signing_key`]) and is never logged; neither is
/// the response body/headers on failure (they may carry account details).
pub async fn list_buckets(
    endpoint: &str,
    region: &str,
    ak: &str,
    sk: &str,
) -> AppResult<Vec<OssBucket>> {
    let (x_oss_date, date) = now_oss_date();

    let signed_headers: [(&str, &str); 2] = [
        ("x-oss-date", x_oss_date.as_str()),
        ("x-oss-content-sha256", "UNSIGNED-PAYLOAD"),
    ];
    let cr = oss_sign::canonical_request("GET", "/", "", &signed_headers, &[], "UNSIGNED-PAYLOAD");
    let cr_hash = oss_sign::sha256_hex(cr.as_bytes());
    let sts = oss_sign::string_to_sign(&cr_hash, &x_oss_date, &date, region);
    let key = oss_sign::signing_key(sk, &date, region);
    let sig = oss_sign::signature_hex(&key, &sts);
    let authorization = oss_sign::authorization_header(ak, &date, region, &sig, &[]);

    let url = format!("{}/", endpoint.trim_end_matches('/'));
    let uri: http::Uri = url.parse().map_err(|e| AppError::Internal {
        message: format!("oss list-buckets: invalid endpoint: {e}"),
    })?;

    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(uri)
        .header("x-oss-date", &x_oss_date)
        .header("x-oss-content-sha256", "UNSIGNED-PAYLOAD")
        .header("Authorization", &authorization)
        .body(Empty::<Bytes>::new())
        .map_err(|e| AppError::Internal {
            message: format!("oss list-buckets: failed to build request: {e}"),
        })?;

    let client = build_https_client()?;
    let response = client.request(request).await.map_err(|e| {
        let msg = e.to_string();
        let lower = msg.to_lowercase();
        if lower.contains("timed out") || lower.contains("timeout") {
            AppError::Timeout
        } else {
            AppError::Unreachable
        }
    })?;

    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| AppError::Internal {
            message: format!("oss list-buckets: failed to read response body: {e}"),
        })?
        .to_bytes();
    let body_text = String::from_utf8_lossy(&body);

    if !status.is_success() {
        let (code, message) = parse_error_fields(&body_text);
        if let Some(app_err) = code
            .as_deref()
            .and_then(|c| classify_error_code(c, message.as_deref()))
        {
            return Err(app_err);
        }
        return Err(AppError::Internal {
            message: format!(
                "oss list-buckets: request failed with status {}",
                status.as_u16()
            ),
        });
    }

    parse_list_all_my_buckets(&body_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_buckets_with_region() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListAllMyBucketsResult>
  <Owner><ID>1234</ID><DisplayName>1234</DisplayName></Owner>
  <Buckets>
    <Bucket>
      <Name>bucket-hz</Name>
      <CreationDate>2023-07-01T10:00:00.000Z</CreationDate>
      <Location>oss-cn-hangzhou</Location>
      <Region>cn-hangzhou</Region>
      <ExtranetEndpoint>oss-cn-hangzhou.aliyuncs.com</ExtranetEndpoint>
      <IntranetEndpoint>oss-cn-hangzhou-internal.aliyuncs.com</IntranetEndpoint>
      <StorageClass>Standard</StorageClass>
    </Bucket>
    <Bucket>
      <Name>bucket-bj</Name>
      <CreationDate>2023-08-02T11:00:00.000Z</CreationDate>
      <Location>oss-cn-beijing</Location>
      <Region>cn-beijing</Region>
      <ExtranetEndpoint>oss-cn-beijing.aliyuncs.com</ExtranetEndpoint>
      <IntranetEndpoint>oss-cn-beijing-internal.aliyuncs.com</IntranetEndpoint>
      <StorageClass>Standard</StorageClass>
    </Bucket>
  </Buckets>
</ListAllMyBucketsResult>"#;

        let buckets = parse_list_all_my_buckets(xml).unwrap();
        assert_eq!(
            buckets,
            vec![
                OssBucket {
                    name: "bucket-hz".to_string(),
                    region: "cn-hangzhou".to_string(),
                    creation_date: Some("2023-07-01T10:00:00.000Z".to_string()),
                },
                OssBucket {
                    name: "bucket-bj".to_string(),
                    region: "cn-beijing".to_string(),
                    creation_date: Some("2023-08-02T11:00:00.000Z".to_string()),
                },
            ]
        );
    }

    #[test]
    fn falls_back_to_location_stripped_of_oss_prefix_when_region_missing() {
        let xml = r#"<ListAllMyBucketsResult>
  <Buckets>
    <Bucket>
      <Name>legacy-bucket</Name>
      <CreationDate>2020-01-01T00:00:00.000Z</CreationDate>
      <Location>oss-cn-shanghai</Location>
    </Bucket>
  </Buckets>
</ListAllMyBucketsResult>"#;

        let buckets = parse_list_all_my_buckets(xml).unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].name, "legacy-bucket");
        assert_eq!(buckets[0].region, "cn-shanghai");
    }

    #[test]
    fn bucket_with_neither_region_nor_location_gets_empty_region() {
        let xml = r#"<ListAllMyBucketsResult>
  <Buckets>
    <Bucket>
      <Name>no-region-bucket</Name>
      <CreationDate>2020-01-01T00:00:00.000Z</CreationDate>
    </Bucket>
  </Buckets>
</ListAllMyBucketsResult>"#;

        let buckets = parse_list_all_my_buckets(xml).unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].region, "");
    }

    #[test]
    fn empty_buckets_element_yields_empty_vec() {
        let xml = r#"<ListAllMyBucketsResult>
  <Owner><ID>1234</ID><DisplayName>1234</DisplayName></Owner>
  <Buckets/>
</ListAllMyBucketsResult>"#;

        let buckets = parse_list_all_my_buckets(xml).unwrap();
        assert!(buckets.is_empty());
    }

    #[test]
    fn error_xml_is_rejected() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>PermanentRedirect</Code>
  <Message>Bucket not found in this region.</Message>
  <RequestId>abc123</RequestId>
</Error>"#;

        assert!(parse_list_all_my_buckets(xml).is_err());
    }

    #[test]
    fn garbage_input_is_rejected() {
        assert!(parse_list_all_my_buckets("not xml at all").is_err());
    }

    #[test]
    fn parse_error_fields_extracts_code_and_message() {
        let xml = r#"<Error>
  <Code>PermanentRedirect</Code>
  <Message>Bucket not found in this region.</Message>
</Error>"#;
        let (code, message) = parse_error_fields(xml);
        assert_eq!(code.as_deref(), Some("PermanentRedirect"));
        assert_eq!(message.as_deref(), Some("Bucket not found in this region."));
    }

    #[test]
    fn parse_error_fields_returns_none_for_unparseable_body() {
        let (code, message) = parse_error_fields("not xml");
        assert!(code.is_none());
        assert!(message.is_none());
    }

    // --- civil_from_unix_seconds --------------------------------------

    #[test]
    fn civil_from_unix_seconds_matches_known_timestamp() {
        // 2025-04-11T06:41:24Z (the KAT-1 x-oss-date value's instant).
        let unix = 1_744_353_684_i64;
        assert_eq!(civil_from_unix_seconds(unix), (2025, 4, 11, 6, 41, 24));
    }

    #[test]
    fn civil_from_unix_seconds_handles_epoch() {
        assert_eq!(civil_from_unix_seconds(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn now_oss_date_produces_well_formed_strings() {
        let (x_oss_date, date) = now_oss_date();
        assert_eq!(x_oss_date.len(), OSS_DATE_FORMAT_LEN);
        assert!(x_oss_date.ends_with('Z'));
        assert!(x_oss_date.starts_with(&date));
        assert_eq!(date.len(), 8);
        assert!(date.bytes().all(|b| b.is_ascii_digit()));
    }
}
