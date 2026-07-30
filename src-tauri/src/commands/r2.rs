//! Cloudflare R2 commands: the connection form's token probe, and the
//! per-bucket metadata panel.
//!
//! Kept out of [`super::connection`] because these are a separate concern with
//! a separate backend ([`crate::provider::r2_admin`], which talks to
//! `api.cloudflare.com` rather than to any S3 endpoint) -- and because
//! `connection.rs` is already the largest command module.
//!
//! **Security**: `r2_probe_token` takes a live Bearer credential as a command
//! argument, because the user has just typed it into the form and nothing is
//! saved yet. It is used for exactly one round trip and never logged, echoed
//! back, or persisted by this command -- persisting it is `add_connection`'s
//! job, through the encrypted store.

use serde::Serialize;
use tauri::State;

use crate::commands::connection::AppState;
use crate::error::{AppError, AppResult};
use crate::provider::r2::parse_r2_endpoint;
use crate::provider::r2_admin::{
    self, R2BucketMeta, R2CustomDomain, R2ManagedDomain, R2TokenProbe, R2Usage,
};

/// Probes an R2 API token the user has just pasted into the connection form,
/// **before** any connection exists to save it against.
///
/// Returns the token's id -- which *is* R2's S3 Access Key ID -- plus
/// whichever accounts the token can enumerate, so the form can fill in both
/// the access key and the account id (and from it the endpoint) from a single
/// paste.
///
/// `accounts` coming back **empty is a success**, not a failure: an R2
/// object-scoped token verifies fine and reports its id, but `GET /accounts`
/// answers `200 []` for it rather than 403 (verified live on 2026-07-30). The
/// form must then ask the user for the account id instead of rejecting the
/// token, so this command must not turn that case into an error.
#[tauri::command]
pub async fn r2_probe_token(token: String) -> AppResult<R2TokenProbe> {
    r2_admin::probe_token(&token).await
}

/// `api_error` value for a connection whose endpoint is not an R2 host at all
/// -- a custom domain, or one edited by hand. There is no account id to query
/// the Cloudflare API with, so the API-sourced fields are unavailable.
///
/// Deliberately *not* an [`AppError`] code: nothing failed and nothing is
/// broken, the connection simply cannot be mapped onto a Cloudflare account.
/// The frontend resolves it through the same `errors.*` dictionary, which
/// falls back to generic copy for keys a build doesn't recognize.
const ENDPOINT_NOT_R2: &str = "r2/endpoint-not-r2";

/// Everything the bucket-info panel shows, with each piece independently
/// optional.
///
/// The shape is deliberately "many optionals" rather than one nested
/// `Option<ApiInfo>`, because the sources have genuinely different
/// availability:
///
/// - `location` comes from the **S3** plane and is available at every
///   privilege tier, including tokens the Cloudflare API refuses outright.
/// - Everything below it needs a Cloudflare API token. Each call is made
///   independently, so one refusal never blanks the rest.
///
/// `api_error` is an i18n key explaining why the API-sourced fields are
/// absent, so the UI can say *why* rather than just showing dashes: either an
/// [`AppError::code`] from a failed call (`auth/access-denied` being by far
/// the most common) or [`ENDPOINT_NOT_R2`]. It is `None` when nothing went
/// wrong -- including the case where no token is configured at all, which
/// `has_api_token: false` already describes.
#[derive(Debug, Clone, Serialize)]
pub struct R2BucketInfo {
    pub bucket: String,
    pub location: Option<String>,
    pub has_api_token: bool,
    pub meta: Option<R2BucketMeta>,
    pub usage: Option<R2Usage>,
    pub managed_domain: Option<R2ManagedDomain>,
    pub custom_domains: Option<Vec<R2CustomDomain>>,
    pub api_error: Option<String>,
}

/// Collects a bucket's R2 metadata for the info panel.
///
/// **Never fails because metadata is unavailable.** The only errors this
/// returns are structural (no such connection). A missing token, a refused
/// token, an endpoint that isn't an R2 host -- all of those come back as a
/// populated `R2BucketInfo` with `api_error` set, because the panel's job is
/// to show what it can and explain the rest. Making them hard errors would
/// hide the location hint, which is available regardless.
#[tauri::command]
pub async fn r2_bucket_info(
    state: State<'_, AppState>,
    connection_id: String,
    bucket: String,
) -> AppResult<R2BucketInfo> {
    let hub = state.hub();
    let provider = hub.provider(&connection_id).await?;

    // The S3-plane half. A failure here is non-fatal too: an object-scoped
    // token that cannot even call GetBucketLocation should still get a panel
    // showing whatever the API half returns.
    let location = provider.bucket_location(&bucket).await.unwrap_or(None);

    let connection = hub
        .connections()
        .await?
        .into_iter()
        .find(|c| c.id == connection_id)
        .ok_or_else(|| AppError::ConnectionNotFound {
            id: connection_id.clone(),
        })?;

    let mut info = R2BucketInfo {
        bucket: bucket.clone(),
        location,
        has_api_token: connection.api_token.is_some(),
        meta: None,
        usage: None,
        managed_domain: None,
        custom_domains: None,
        api_error: None,
    };

    let Some(token) = connection.api_token.as_deref() else {
        return Ok(info);
    };

    // The account id and jurisdiction both come from the endpoint. A
    // connection pointed at something that isn't an R2 host (a custom domain,
    // a hand-edited endpoint) has no account to query, which is a reportable
    // state rather than an error.
    let Some((account_id, jurisdiction)) = parse_r2_endpoint(&connection.endpoint) else {
        info.api_error = Some(ENDPOINT_NOT_R2.to_string());
        return Ok(info);
    };

    // Four independent calls. `tokio::join!` runs them concurrently on this
    // task without spawning, and each result is unwrapped separately so one
    // refusal cannot blank the others -- the whole point of the flat shape
    // above.
    let (meta, usage, managed, custom) = tokio::join!(
        r2_admin::bucket_meta(token, &account_id, &jurisdiction, &bucket),
        r2_admin::bucket_usage(token, &account_id, &jurisdiction, &bucket),
        r2_admin::managed_domain(token, &account_id, &jurisdiction, &bucket),
        r2_admin::custom_domains(token, &account_id, &jurisdiction, &bucket),
    );

    // The first error seen becomes the reported reason. In practice all four
    // fail identically (they share one auth boundary), so reporting more than
    // one would be noise.
    let mut first_error: Option<String> = None;
    let mut record = |e: AppError| {
        first_error.get_or_insert_with(|| e.code().to_string());
    };

    match meta {
        Ok(v) => info.meta = Some(v),
        Err(e) => record(e),
    }
    match usage {
        Ok(v) => info.usage = Some(v),
        Err(e) => record(e),
    }
    match managed {
        Ok(v) => info.managed_domain = Some(v),
        Err(e) => record(e),
    }
    match custom {
        Ok(v) => info.custom_domains = Some(v),
        Err(e) => record(e),
    }
    info.api_error = first_error;

    Ok(info)
}
