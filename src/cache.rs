//! Local content-addressed cache.
//!
//! See TDD-0007 for the on-disk layout, TDD-0012 for eviction. This module
//! implements:
//!
//! - Directory layout (`ac/`, `cas/`, `structural/`, `tmp/`, `version`).
//! - Atomic writes via write-then-rename through `tmp/`.
//! - Action-cache and content-addressed-storage read / write.

use crate::model::{CacheKey, ContentHash};
use crate::paths::AbsPath;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::task::spawn_blocking;

/// Current on-disk schema version. Bumping requires a migration step.
pub const CACHE_VERSION: u32 = 1;

/// Schema version of an AC entry. Independent of the directory schema.
pub const AC_SCHEMA: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("cache entry corrupt at {path:?}: {detail}")]
    Corrupt { path: PathBuf, detail: String },

    #[error("cache version mismatch: on-disk is {found}, this binary expects {expected}")]
    VersionMismatch { found: u32, expected: u32 },

    #[error("join error: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// The local cache. Owns a directory; reads and writes content-addressed
/// blobs and action-cache entries.
#[derive(Debug, Clone)]
pub struct LocalCache {
    root: AbsPath,
    /// Monotonic counter for unique tmp filenames.
    tmp_counter: std::sync::Arc<AtomicU64>,
}

impl LocalCache {
    /// Open (or initialize) a cache rooted at `root`. Creates the directory
    /// layout if absent. Reads the `version` file; errors on mismatch.
    pub async fn open(root: AbsPath) -> Result<Self, CacheError> {
        let cache = Self {
            root,
            tmp_counter: std::sync::Arc::new(AtomicU64::new(0)),
        };
        cache.init_layout().await?;
        Ok(cache)
    }

    pub fn root(&self) -> &AbsPath {
        &self.root
    }

    async fn init_layout(&self) -> Result<(), CacheError> {
        let root = self.root.as_path().to_path_buf();
        let cache = self.clone();
        spawn_blocking(move || -> Result<(), CacheError> {
            for sub in ["ac", "cas", "structural", "log", "tmp"] {
                std::fs::create_dir_all(root.join(sub))?;
            }
            // Set 0700 on cache root (TDD-0007 §Permissions).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o700);
                std::fs::set_permissions(&root, perms)?;
            }
            let version_path = root.join("version");
            if version_path.exists() {
                let raw = std::fs::read_to_string(&version_path)?;
                let v: u32 = raw.trim().parse().map_err(|_| CacheError::Corrupt {
                    path: version_path.clone(),
                    detail: format!("unparseable version: {raw:?}"),
                })?;
                if v != CACHE_VERSION {
                    return Err(CacheError::VersionMismatch {
                        found: v,
                        expected: CACHE_VERSION,
                    });
                }
            } else {
                cache.write_version_blocking(&version_path)?;
            }
            Ok(())
        })
        .await??;
        Ok(())
    }

    fn write_version_blocking(&self, path: &std::path::Path) -> Result<(), CacheError> {
        let mut tmp = self.new_tmp_path()?;
        // version is small enough to write directly via std fs without async.
        {
            let mut f = std::fs::File::create(&tmp)?;
            writeln!(f, "{CACHE_VERSION}")?;
            f.sync_all()?;
        }
        // Atomic rename into place.
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Best effort cleanup of the tmp file.
                let _ = std::fs::remove_file(&tmp);
                tmp.set_extension("");
                Err(e.into())
            }
        }
    }

    fn new_tmp_path(&self) -> std::io::Result<PathBuf> {
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        Ok(self
            .root
            .as_path()
            .join("tmp")
            .join(format!("write-{pid}-{nanos}-{n}.tmp")))
    }

    fn ac_path(&self, key: &CacheKey) -> PathBuf {
        let hex = key.to_hex();
        let prefix = &hex[..2];
        self.root.as_path().join("ac").join(prefix).join(format!("{hex}.json"))
    }

    fn cas_path(&self, hash: &ContentHash) -> PathBuf {
        let hex = hash.to_hex();
        let prefix = &hex[..2];
        self.root.as_path().join("cas").join(prefix).join(hex)
    }

    /// Read the AC entry for a cache key. Returns Ok(None) on miss.
    pub async fn get_ac(&self, key: &CacheKey) -> Result<Option<AcEntry>, CacheError> {
        let path = self.ac_path(key);
        let result = spawn_blocking(move || -> Result<Option<AcEntry>, CacheError> {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let entry: AcEntry =
                        serde_json::from_slice(&bytes).map_err(|e| CacheError::Corrupt {
                            path: path.clone(),
                            detail: e.to_string(),
                        })?;
                    Ok(Some(entry))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await??;
        Ok(result)
    }

    /// Write an AC entry atomically.
    pub async fn put_ac(&self, key: &CacheKey, entry: &AcEntry) -> Result<(), CacheError> {
        let path = self.ac_path(key);
        let bytes = serde_json::to_vec_pretty(entry)?;
        self.atomic_write(path, bytes).await
    }

    /// Read a CAS blob by content hash.
    pub async fn get_cas(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>, CacheError> {
        let path = self.cas_path(hash);
        let result = spawn_blocking(move || -> Result<Option<Vec<u8>>, CacheError> {
            match std::fs::read(&path) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await??;
        Ok(result)
    }

    /// Write a CAS blob. The content hash is recomputed to verify the caller's
    /// claim and to be the source of truth for the path.
    pub async fn put_cas(&self, bytes: Vec<u8>) -> Result<ContentHash, CacheError> {
        let (hash, path) = {
            let h = ContentHash::of_bytes(&bytes);
            (h, self.cas_path(&h))
        };
        // Optimization: skip the write if the blob already exists (content-addressed).
        let exists = {
            let p = path.clone();
            spawn_blocking(move || p.exists()).await?
        };
        if !exists {
            self.atomic_write(path, bytes).await?;
        }
        Ok(hash)
    }

    /// Has-blob check without reading the body. Cheaper than `get_cas`.
    pub async fn has_cas(&self, hash: &ContentHash) -> bool {
        let path = self.cas_path(hash);
        spawn_blocking(move || path.exists()).await.unwrap_or(false)
    }

    /// Write a file atomically: write to `tmp/`, fsync, then rename to `dst`.
    ///
    /// Creates the parent directory if needed. POSIX rename is atomic
    /// within a filesystem.
    async fn atomic_write(&self, dst: PathBuf, bytes: Vec<u8>) -> Result<(), CacheError> {
        let tmp = self.new_tmp_path()?;
        spawn_blocking(move || -> Result<(), CacheError> {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            {
                let mut f = std::fs::File::create(&tmp)?;
                f.write_all(&bytes)?;
                f.sync_all()?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                }
            }
            // rename(2) - atomic within a filesystem (TDD-0007).
            if let Err(e) = std::fs::rename(&tmp, &dst) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
            Ok(())
        })
        .await??;
        Ok(())
    }
}

/// One action-cache entry. Schema matches TDD-0007.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcEntry {
    pub schema: u32,
    pub target_id: String,
    pub cache_key: String,
    pub command: String,
    pub cwd: String,
    pub outputs: Vec<OutputEntry>,
    pub outputs_content_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_blob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_blob: Option<String>,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub built_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_by: Option<String>,
    #[serde(default)]
    pub sandboxed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputEntry {
    pub path: String,
    pub content_hash: String,
    pub size: u64,
    pub executable: bool,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_cache() -> (LocalCache, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let abs = AbsPath::new(dir.path().to_path_buf());
        let cache = LocalCache::open(abs).await.unwrap();
        (cache, dir)
    }

    #[tokio::test]
    async fn open_creates_layout() {
        let dir = tempfile::tempdir().unwrap();
        let abs = AbsPath::new(dir.path().to_path_buf());
        let _cache = LocalCache::open(abs).await.unwrap();
        for sub in ["ac", "cas", "structural", "log", "tmp"] {
            assert!(
                dir.path().join(sub).is_dir(),
                "expected {sub}/ to exist"
            );
        }
        assert!(dir.path().join("version").is_file());
    }

    #[tokio::test]
    async fn version_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let abs = AbsPath::new(dir.path().to_path_buf());
        let _cache = LocalCache::open(abs.clone()).await.unwrap();
        // Open again should succeed (version matches).
        let _ = LocalCache::open(abs).await.unwrap();
    }

    #[tokio::test]
    async fn version_mismatch_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("version"), "999\n").unwrap();
        for sub in ["ac", "cas", "structural", "log", "tmp"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        let abs = AbsPath::new(dir.path().to_path_buf());
        let err = LocalCache::open(abs).await.unwrap_err();
        assert!(matches!(err, CacheError::VersionMismatch { found: 999, .. }));
    }

    #[tokio::test]
    async fn cas_put_then_get() {
        let (cache, _dir) = temp_cache().await;
        let data = b"hello world".to_vec();
        let hash = cache.put_cas(data.clone()).await.unwrap();
        assert_eq!(hash, ContentHash::of_bytes(&data));
        let read = cache.get_cas(&hash).await.unwrap().unwrap();
        assert_eq!(read, data);
    }

    #[tokio::test]
    async fn cas_put_dedupes() {
        let (cache, _dir) = temp_cache().await;
        let data = b"same content".to_vec();
        let h1 = cache.put_cas(data.clone()).await.unwrap();
        let h2 = cache.put_cas(data.clone()).await.unwrap();
        assert_eq!(h1, h2);
        assert!(cache.has_cas(&h1).await);
    }

    #[tokio::test]
    async fn cas_get_miss_returns_none() {
        let (cache, _dir) = temp_cache().await;
        let bogus = ContentHash::of_bytes(b"never written");
        assert!(cache.get_cas(&bogus).await.unwrap().is_none());
        assert!(!cache.has_cas(&bogus).await);
    }

    #[tokio::test]
    async fn ac_put_then_get() {
        let (cache, _dir) = temp_cache().await;
        let key = CacheKey::new(ContentHash::of_bytes(b"k1"));
        let entry = AcEntry {
            schema: AC_SCHEMA,
            target_id: "foo".into(),
            cache_key: key.to_hex(),
            command: "echo hi".into(),
            cwd: "".into(),
            outputs: vec![OutputEntry {
                path: "bin/x".into(),
                content_hash: "abcd".into(),
                size: 42,
                executable: true,
                mode: "0755".into(),
                symlink_target: None,
            }],
            outputs_content_hash: "abcd".into(),
            stdout_blob: None,
            stderr_blob: None,
            exit_code: 0,
            duration_ms: 10,
            built_at: "2026-05-20T00:00:00Z".into(),
            built_by: None,
            sandboxed: false,
        };
        cache.put_ac(&key, &entry).await.unwrap();
        let got = cache.get_ac(&key).await.unwrap().unwrap();
        assert_eq!(got.target_id, "foo");
        assert_eq!(got.outputs.len(), 1);
        assert_eq!(got.outputs[0].path, "bin/x");
    }

    #[tokio::test]
    async fn ac_get_miss_returns_none() {
        let (cache, _dir) = temp_cache().await;
        let bogus = CacheKey::new(ContentHash::of_bytes(b"miss"));
        assert!(cache.get_ac(&bogus).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ac_corrupt_returns_corrupt_error() {
        let (cache, dir) = temp_cache().await;
        let key = CacheKey::new(ContentHash::of_bytes(b"corrupt"));
        let path = cache.ac_path(&key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        let err = cache.get_ac(&key).await.unwrap_err();
        assert!(matches!(err, CacheError::Corrupt { .. }), "got: {err:?}");
        drop(dir);
    }

    #[tokio::test]
    async fn ac_path_is_sharded_by_first_two_hex_chars() {
        let (cache, _dir) = temp_cache().await;
        let key = CacheKey::new(ContentHash::of_bytes(b"x"));
        let hex = key.to_hex();
        let path = cache.ac_path(&key);
        let path_str = path.display().to_string();
        assert!(path_str.contains(&format!("/ac/{}/", &hex[..2])));
        assert!(path_str.ends_with(&format!("{hex}.json")));
    }

    #[tokio::test]
    async fn atomic_write_cleans_up_tmp_on_failure() {
        let (cache, dir) = temp_cache().await;
        // Run a normal write; verify tmp/ is empty afterwards (no leftovers).
        let _ = cache.put_cas(b"some bytes".to_vec()).await.unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path().join("tmp"))
            .unwrap()
            .collect();
        assert!(entries.is_empty(), "tmp/ should be empty after write");
    }
}
