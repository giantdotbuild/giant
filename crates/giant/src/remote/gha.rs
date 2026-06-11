//! GitHub Actions cache service backend (the v2 Twirp API).
//!
//! The runner's own cache: three Twirp calls (plain JSON over HTTP) to
//! locate/reserve entries, with the bytes living in Azure Blob Storage
//! behind signed URLs. Entries are immutable per (key, version) - a fit
//! for content-addressed data - and GitHub scopes them by branch:
//! default-branch entries are readable from every branch, branch writes
//! are visible only to that branch.
//!
//! The endpoint and token come from `ACTIONS_RESULTS_URL` /
//! `ACTIONS_RUNTIME_TOKEN`, which the runner exposes to JS actions but
//! NOT to plain `run:` steps - workflows export them with a two-line
//! `actions/github-script` step (see the remote-cache guide).

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::model::{CacheKey, ContentHash};

const SERVICE: &str = "twirp/github.actions.results.api.v1.CacheService";

#[derive(Debug, thiserror::Error)]
pub(super) enum GhaError {
    #[error("auth failed (HTTP 401/403)")]
    Auth,

    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),

    #[error("{method} returned {status}: {body}")]
    Twirp {
        method: &'static str,
        status: StatusCode,
        body: String,
    },
}

/// Resolved backend config. `version` namespaces every entry the same way
/// `actions/cache` versions its keys; it folds in the remote AC schema so
/// a wire-format bump can never read old entries as new ones.
#[derive(Debug, Clone)]
pub struct GhaConfig {
    results_url: String,
    token: String,
    version: String,
}

impl GhaConfig {
    /// From the env the Actions runner provides. The error spells out the
    /// export step because the variables are invisible to `run:` steps by
    /// default and this is everyone's first failure.
    pub fn from_env(remote_schema: u32) -> Result<Self, String> {
        let read = |name: &str| {
            std::env::var(name).map_err(|_| {
                format!(
                    "{name} is unset. The Actions runner only exposes it to JS actions; \
                     export it to the job env first - see the remote cache guide \
                     (https://giant.build/guides/remote-cache/)"
                )
            })
        };
        let results_url = read("ACTIONS_RESULTS_URL")?;
        let token = read("ACTIONS_RUNTIME_TOKEN")?;
        Ok(Self::new(results_url, token, remote_schema))
    }

    pub fn new(results_url: String, token: String, remote_schema: u32) -> Self {
        // The namespace salt segregates entries beyond the wire schema. Bump
        // it to abandon a poisoned namespace - e.g. dangling reservations
        // left by a crashed or buggy uploader block re-creates of the same
        // (key, version) until GitHub expires them.
        const NAMESPACE: u32 = 2;
        let version = const_hex::encode(Sha256::digest(format!(
            "giant remote cache schema {remote_schema} namespace {NAMESPACE}"
        )));
        Self {
            results_url: results_url.trim_end_matches('/').to_string(),
            token,
            version,
        }
    }
}

pub(super) fn ac_key(key: &CacheKey) -> String {
    format!("giant-ac-{}", key.to_hex())
}

pub(super) fn cas_key(hash: &ContentHash) -> String {
    format!("giant-cas-{}", hash.to_hex())
}

// --- Twirp wire shapes ------------------------------------------------------
//
// The service emits original proto field names (snake_case), not the
// canonical proto3-JSON lowerCamelCase - matching the field names the
// BuildKit client uses against the same endpoints.

#[derive(Serialize)]
struct EntryRequest<'a> {
    key: &'a str,
    version: &'a str,
}

/// Lookup request. `restore_keys` repeats the key: exact-match is all we
/// want, but the service's lookup path is exercised by mainstream clients
/// with this field populated, so match their shape.
#[derive(Serialize)]
struct LookupRequest<'a> {
    key: &'a str,
    restore_keys: [&'a str; 1],
    version: &'a str,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DownloadUrlResponse {
    ok: bool,
    signed_download_url: String,
    matched_key: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CreateEntryResponse {
    ok: bool,
    signed_upload_url: String,
}

#[derive(Serialize)]
struct FinalizeRequest<'a> {
    key: &'a str,
    version: &'a str,
    size_bytes: i64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FinalizeResponse {
    ok: bool,
}

/// Attempts per Twirp call: the service rate-limits bursts, and uploads run
/// in the background where a few seconds of backoff cost nothing.
const ATTEMPTS: u32 = 3;

async fn twirp<T: DeserializeOwned>(
    client: &Client,
    cfg: &GhaConfig,
    method: &'static str,
    body: &impl Serialize,
) -> Result<T, GhaError> {
    let url = format!("{}/{SERVICE}/{method}", cfg.results_url);
    let mut backoff = std::time::Duration::from_secs(1);
    for attempt in 1..=ATTEMPTS {
        let resp = client
            .post(&url)
            .bearer_auth(&cfg.token)
            .json(body)
            .send()
            .await?;
        match resp.status() {
            StatusCode::OK => return Ok(resp.json::<T>().await?),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Err(GhaError::Auth),
            StatusCode::TOO_MANY_REQUESTS if attempt < ATTEMPTS => {
                let wait = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok())
                    .map(std::time::Duration::from_secs)
                    .unwrap_or(backoff);
                tokio::time::sleep(wait).await;
                backoff *= 4;
            }
            status => {
                return Err(GhaError::Twirp {
                    method,
                    status,
                    body: resp.text().await.unwrap_or_default(),
                });
            }
        }
    }
    // Not reached: the final attempt's 429 falls through to the `status`
    // arm above. Kept as an error so no panic path exists.
    Err(GhaError::Twirp {
        method,
        status: StatusCode::TOO_MANY_REQUESTS,
        body: "rate limited after retries".into(),
    })
}

async fn lookup(
    client: &Client,
    cfg: &GhaConfig,
    key: &str,
) -> Result<DownloadUrlResponse, GhaError> {
    let resp: DownloadUrlResponse = twirp(
        client,
        cfg,
        "GetCacheEntryDownloadURL",
        &LookupRequest {
            key,
            restore_keys: [key],
            version: &cfg.version,
        },
    )
    .await?;
    tracing::info!(
        "remote lookup {key}: ok={} matched={}",
        resp.ok,
        if resp.matched_key.is_empty() {
            "-"
        } else {
            &resp.matched_key
        }
    );
    Ok(resp)
}

/// Whether an entry exists for `key`, without downloading it.
pub(super) async fn exists(client: &Client, cfg: &GhaConfig, key: &str) -> Result<bool, GhaError> {
    Ok(lookup(client, cfg, key).await?.ok)
}

/// Fetch the entry stored under `key`. `None` on a miss.
pub(super) async fn fetch(
    client: &Client,
    cfg: &GhaConfig,
    key: &str,
) -> Result<Option<Vec<u8>>, GhaError> {
    let resp = lookup(client, cfg, key).await?;
    if !resp.ok || resp.signed_download_url.is_empty() {
        return Ok(None);
    }
    // The signed URL carries its own auth; a bearer header would be rejected.
    let blob = client.get(&resp.signed_download_url).send().await?;
    if blob.status() != StatusCode::OK {
        return Ok(None);
    }
    Ok(Some(blob.bytes().await?.to_vec()))
}

/// Store `bytes` under `key`: reserve, upload to the signed URL, finalize.
/// `Ok(false)` means the service declined the reservation - the entry
/// already exists or another job holds it - which is success for our
/// purposes (content-addressed data is the same bytes either way).
pub(super) async fn store(
    client: &Client,
    cfg: &GhaConfig,
    key: &str,
    bytes: Vec<u8>,
) -> Result<bool, GhaError> {
    let created: CreateEntryResponse = match twirp(
        client,
        cfg,
        "CreateCacheEntry",
        &EntryRequest {
            key,
            version: &cfg.version,
        },
    )
    .await
    {
        Ok(c) => c,
        // An `already_exists` reservation conflict is success for our
        // purposes - the entry holds the same content-addressed bytes.
        // Every other error propagates so the caller can log it; a cache
        // that silently stores nothing must not look healthy.
        Err(GhaError::Twirp { body, .. }) if body.contains("already_exists") => {
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    if !created.ok || created.signed_upload_url.is_empty() {
        // The service declines creates for keys someone else holds - an
        // existing entry, a concurrent upload, or a dangling reservation
        // from a crashed uploader. Say so: a cache that quietly stores
        // nothing looks identical to a healthy one otherwise.
        tracing::info!("remote create declined for {key} (held elsewhere)");
        return Ok(false);
    }

    let size = bytes.len() as i64;
    let put = client
        .put(&created.signed_upload_url)
        .header("x-ms-blob-type", "BlockBlob")
        .body(bytes)
        .send()
        .await?;
    if !put.status().is_success() {
        return Err(GhaError::Twirp {
            method: "blob upload",
            status: put.status(),
            body: String::new(),
        });
    }

    let fin: FinalizeResponse = twirp(
        client,
        cfg,
        "FinalizeCacheEntryUpload",
        &FinalizeRequest {
            key,
            version: &cfg.version,
            size_bytes: size,
        },
    )
    .await?;
    Ok(fin.ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_stable_and_schema_dependent() {
        let a = GhaConfig::new("https://x".into(), "t".into(), 1);
        let b = GhaConfig::new("https://x".into(), "t".into(), 1);
        let c = GhaConfig::new("https://x".into(), "t".into(), 2);
        assert_eq!(a.version, b.version);
        assert_ne!(a.version, c.version);
        assert_eq!(a.version.len(), 64, "sha256 hex");
    }

    #[test]
    fn results_url_trailing_slash_is_normalized() {
        let c = GhaConfig::new("https://x/".into(), "t".into(), 1);
        assert_eq!(c.results_url, "https://x");
    }
}
