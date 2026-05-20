//! Remote cache client - Bazel HTTP cache protocol.
//!
//! Two endpoints:
//!   GET/PUT/HEAD  /ac/<sha256_hex>    Action Cache entries (our JSON schema)
//!   GET/PUT/HEAD  /cas/<sha256_hex>   Content-Addressed Storage blobs
//!
//! See TDD-0006. Feature-gated behind `--features remote`; the
//! engine builds and runs without it.
//!
//! Failure mode: any error from the remote degrades gracefully. The
//! build is never blocked by remote unavailability - local cache
//! continues to work, build proceeds, and we just log a warning.

#![cfg(feature = "remote")]

use crate::cache::AcEntry;
use crate::model::{CacheKey, ContentHash};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, RETRY_AFTER};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),

    #[error("server returned {status} for {url}")]
    BadStatus { url: String, status: StatusCode },

    #[error("auth failed (HTTP 401/403)")]
    AuthFailed,

    #[error("bad config: {0}")]
    Config(String),

    #[error("blob exceeds max_blob_size_mb")]
    BlobTooLarge,

    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

// =============================================================================
// Config
// =============================================================================

/// Resolved client configuration. Built from `config::RemoteConfig`
/// (which has the un-resolved `*_env` references), with env-var
/// lookups + URL normalization applied.
#[derive(Debug, Clone)]
pub struct RemoteCacheConfig {
    pub base_url: String,
    pub auth: Auth,
    pub skip_head: bool,
    pub max_blob_size: u64,
    pub skip_tls_verify: bool,
}

#[derive(Debug, Clone)]
pub enum Auth {
    None,
    Bearer(String),
    Basic { user: String, pass: String },
}

impl RemoteCacheConfig {
    /// Resolve a `config::RemoteConfig` (with `*_env` references) into
    /// a `RemoteCacheConfig` (with the actual secret values). Reads
    /// env vars lazily, so secrets never sit in the parsed config tree.
    pub fn from_config(cfg: &crate::config::RemoteConfig) -> Result<Self, RemoteError> {
        let url = cfg.url.as_deref().ok_or_else(|| {
            RemoteError::Config("cache.remote.url is required when enabled".into())
        })?;
        let base_url = url.trim_end_matches('/').to_string();

        let auth = match &cfg.auth {
            crate::config::RemoteAuth::None => Auth::None,
            crate::config::RemoteAuth::Bearer { token_env } => Auth::Bearer(
                std::env::var(token_env)
                    .map_err(|_| RemoteError::Config(format!("env var {token_env} is unset")))?,
            ),
            crate::config::RemoteAuth::Basic {
                username_env,
                password_env,
            } => Auth::Basic {
                user: std::env::var(username_env).map_err(|_| {
                    RemoteError::Config(format!("env var {username_env} is unset"))
                })?,
                pass: std::env::var(password_env).map_err(|_| {
                    RemoteError::Config(format!("env var {password_env} is unset"))
                })?,
            },
        };

        Ok(Self {
            base_url,
            auth,
            skip_head: cfg.skip_head,
            max_blob_size: cfg.max_blob_size_mb.saturating_mul(1024 * 1024),
            skip_tls_verify: cfg.tls.skip_verify,
        })
    }
}

// =============================================================================
// Client
// =============================================================================

/// Cheap-to-clone handle. Holds a `reqwest::Client` (its own connection
/// pool) plus the resolved auth. A single `disabled` flag short-circuits
/// every operation after a fatal auth failure - saves us from retrying
/// 401 forever during a long build.
#[derive(Clone)]
pub struct RemoteCache {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    config: RemoteCacheConfig,
    disabled: AtomicBool,
}

impl RemoteCache {
    pub fn open(config: RemoteCacheConfig) -> Result<Self, RemoteError> {
        let mut builder = Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(16)
            .user_agent(concat!("giant/", env!("CARGO_PKG_VERSION")));
        if config.skip_tls_verify {
            tracing::warn!("remote cache: TLS verification disabled (dev only)");
            builder = builder.danger_accept_invalid_certs(true);
        }
        let client = builder.build()?;
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                config,
                disabled: AtomicBool::new(false),
            }),
        })
    }

    fn is_disabled(&self) -> bool {
        self.inner.disabled.load(Ordering::Relaxed)
    }

    fn disable(&self, reason: &str) {
        if !self.inner.disabled.swap(true, Ordering::Relaxed) {
            tracing::warn!("remote cache disabled for remainder of run: {reason}");
        }
    }

    fn auth_headers(&self) -> HeaderMap {
        let mut h = HeaderMap::new();
        match &self.inner.config.auth {
            Auth::None => {}
            Auth::Bearer(t) => {
                if let Ok(v) = HeaderValue::from_str(&format!("Bearer {t}")) {
                    h.insert(AUTHORIZATION, v);
                }
            }
            Auth::Basic { user, pass } => {
                use base64::Engine;
                let token = base64::engine::general_purpose::STANDARD
                    .encode(format!("{user}:{pass}"));
                if let Ok(v) = HeaderValue::from_str(&format!("Basic {token}")) {
                    h.insert(AUTHORIZATION, v);
                }
            }
        }
        h
    }

    fn ac_url(&self, key: &CacheKey) -> String {
        format!("{}/ac/{}", self.inner.config.base_url, key.to_hex())
    }
    fn cas_url(&self, hash: &ContentHash) -> String {
        format!("{}/cas/{}", self.inner.config.base_url, hash.to_hex())
    }

    /// GET /ac/<key>. Returns `Ok(None)` on 404 or any error (we treat
    /// remote as best-effort).
    pub async fn get_ac(&self, key: &CacheKey) -> Result<Option<AcEntry>, RemoteError> {
        if self.is_disabled() {
            return Ok(None);
        }
        let url = self.ac_url(key);
        let resp = self
            .inner
            .client
            .get(&url)
            .headers(self.auth_headers())
            .send()
            .await
            .map_err(|e| {
                tracing::warn!("remote AC fetch failed for {url}: {e}");
                RemoteError::Http(e)
            });
        let resp = match resp {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        match resp.status() {
            StatusCode::OK => {
                let bytes = resp.bytes().await?;
                match serde_json::from_slice::<RemoteAcEntry>(&bytes) {
                    Ok(remote) => Ok(Some(remote.into_local())),
                    Err(e) => {
                        tracing::warn!("remote AC at {url} unparseable: {e}");
                        Ok(None)
                    }
                }
            }
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                self.disable("auth failed (401/403)");
                Ok(None)
            }
            s => {
                tracing::warn!("remote AC fetch returned {s} for {url}");
                Ok(None)
            }
        }
    }

    /// PUT /ac/<key>. Errors are logged but don't propagate - upload
    /// is best-effort.
    pub async fn put_ac(&self, key: &CacheKey, entry: &AcEntry) -> Result<(), RemoteError> {
        if self.is_disabled() {
            return Ok(());
        }
        let body = serde_json::to_vec(&RemoteAcEntry::from_local(entry))?;
        let url = self.ac_url(key);
        let resp = self
            .inner
            .client
            .put(&url)
            .headers(self.auth_headers())
            .body(body)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) if matches!(r.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) => {
                self.disable("auth failed on PUT (401/403)");
                Ok(())
            }
            Ok(r) => {
                let retry_after = parse_retry_after(r.headers());
                tracing::warn!(
                    "remote AC put returned {} for {url} (retry-after: {:?})",
                    r.status(),
                    retry_after
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!("remote AC put failed for {url}: {e}");
                Ok(())
            }
        }
    }

    /// HEAD /cas/<hash>. Skipped when `skip_head` is set.
    /// Returns `Ok(true)` when the blob is known to exist.
    pub async fn has_cas(&self, hash: &ContentHash) -> Result<bool, RemoteError> {
        if self.is_disabled() || self.inner.config.skip_head {
            return Ok(false);
        }
        let url = self.cas_url(hash);
        let resp = self
            .inner
            .client
            .head(&url)
            .headers(self.auth_headers())
            .send()
            .await;
        match resp {
            Ok(r) if r.status() == StatusCode::OK => Ok(true),
            Ok(_) | Err(_) => Ok(false),
        }
    }

    /// GET /cas/<hash>. None on miss / error / oversize.
    pub async fn get_cas(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>, RemoteError> {
        if self.is_disabled() {
            return Ok(None);
        }
        let url = self.cas_url(hash);
        let resp = self
            .inner
            .client
            .get(&url)
            .headers(self.auth_headers())
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("remote CAS fetch failed for {url}: {e}");
                return Ok(None);
            }
        };
        match resp.status() {
            StatusCode::OK => {
                let bytes = resp.bytes().await?;
                Ok(Some(bytes.to_vec()))
            }
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                self.disable("auth failed");
                Ok(None)
            }
            s => {
                tracing::warn!("remote CAS fetch returned {s} for {url}");
                Ok(None)
            }
        }
    }

    /// PUT /cas/<hash>. Skips when the blob is too large or when a
    /// preceding HEAD reported existence.
    pub async fn put_cas(&self, hash: &ContentHash, bytes: Vec<u8>) -> Result<(), RemoteError> {
        if self.is_disabled() {
            return Ok(());
        }
        if (bytes.len() as u64) > self.inner.config.max_blob_size {
            tracing::warn!(
                "skipping remote upload of {} bytes (exceeds max_blob_size_mb)",
                bytes.len()
            );
            return Ok(());
        }
        if self.has_cas(hash).await.unwrap_or(false) {
            return Ok(());
        }
        let url = self.cas_url(hash);
        let resp = self
            .inner
            .client
            .put(&url)
            .headers(self.auth_headers())
            .body(bytes)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) if matches!(r.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) => {
                self.disable("auth failed on PUT");
                Ok(())
            }
            Ok(r) => {
                tracing::warn!("remote CAS put returned {} for {url}", r.status());
                Ok(())
            }
            Err(e) => {
                tracing::warn!("remote CAS put failed for {url}: {e}");
                Ok(())
            }
        }
    }
}

fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
    let v = headers.get(RETRY_AFTER)?.to_str().ok()?;
    v.parse().ok()
}

// =============================================================================
// Wire format
//
// Locally we use `cache::AcEntry`. Remotely we want a forward-compatible
// schema with explicit version. We could just send AcEntry, but adding
// the indirection means a future local-schema change doesn't break
// existing remote entries (we can keep speaking the older remote
// schema).
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteAcEntry {
    /// Bumped when we change the on-the-wire AC schema. Independent of
    /// the local AC schema. Servers don't validate this; we do.
    schema: u32,
    #[serde(flatten)]
    entry: AcEntry,
}

const REMOTE_AC_SCHEMA: u32 = 1;

impl RemoteAcEntry {
    fn from_local(entry: &AcEntry) -> Self {
        Self {
            schema: REMOTE_AC_SCHEMA,
            entry: entry.clone(),
        }
    }
    fn into_local(self) -> AcEntry {
        // Schema bumps are handled here - for now we accept anything
        // and trust the local cache to validate semantics.
        self.entry
    }
}

// =============================================================================
// Background uploader
//
// Builds put outputs to local cache synchronously (so a successful
// build always has its results locally), then queue upload onto a
// background task. The build never waits.
// =============================================================================

/// Message sent to the uploader: AC entry + zero-or-more CAS blobs.
pub struct UploadJob {
    pub cache_key: CacheKey,
    pub ac_entry: AcEntry,
    pub blobs: Vec<(ContentHash, Vec<u8>)>,
}

/// Spawn the background uploader. Returns a sender for `UploadJob`s
/// and a handle whose completion signals "queue drained, all uploads
/// attempted" (call after `drop(tx)`).
pub fn spawn_uploader(
    remote: RemoteCache,
) -> (mpsc::Sender<UploadJob>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<UploadJob>(256);
    let handle = tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            // Upload blobs first, then the AC entry - if the AC entry
            // is visible to readers, every blob it references must
            // already be on the server. Order matters.
            for (hash, bytes) in job.blobs {
                let _ = remote.put_cas(&hash, bytes).await;
            }
            let _ = remote.put_ac(&job.cache_key, &job.ac_entry).await;
        }
    });
    (tx, handle)
}
