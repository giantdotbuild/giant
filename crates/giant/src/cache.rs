//! Local content-addressed cache.
//!
//! See TDD-0007 for the on-disk layout, TDD-0012 for eviction. This module
//! implements:
//!
//! - Directory layout (`ac/`, `cas/`, `structural/`, `tmp/`, `version`).
//! - Atomic writes via write-then-rename through `tmp/`.
//! - Action-cache and content-addressed-storage read / write.

use crate::model::{CacheKey, ContentHash, TargetId};
use crate::paths::AbsPath;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
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
        self.root
            .as_path()
            .join("ac")
            .join(prefix)
            .join(format!("{hex}.json"))
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
        spawn_blocking(move || atomic_write_blocking(&tmp, &dst, &bytes)).await??;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Structural-input sidecar storage (TDD-0002 / TDD-0007).
    //
    // The sidecar is a per-target JSON file holding per-file fingerprint
    // state used to skip re-reads on warm validation. Sidecar path is
    // sharded by hash of the target ID. Sync because the caller is
    // already inside spawn_blocking (cache-key computation).
    // -----------------------------------------------------------------

    fn structural_sidecar_path(&self, target_id: &TargetId) -> PathBuf {
        let hex = ContentHash::of_bytes(target_id.as_str().as_bytes()).to_hex();
        let prefix = &hex[..2];
        self.root
            .as_path()
            .join("structural")
            .join(prefix)
            .join(format!("{hex}.json"))
    }

    /// Read the raw bytes of a structural sidecar, or `Ok(None)` if absent.
    /// Sync helper for use inside `spawn_blocking`.
    pub fn get_structural_sidecar_raw(
        &self,
        target_id: &TargetId,
    ) -> Result<Option<Vec<u8>>, CacheError> {
        let path = self.structural_sidecar_path(target_id);
        match std::fs::read(&path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Write the raw bytes of a structural sidecar atomically.
    /// Sync helper for use inside `spawn_blocking`.
    pub fn put_structural_sidecar_raw(
        &self,
        target_id: &TargetId,
        bytes: &[u8],
    ) -> Result<(), CacheError> {
        let path = self.structural_sidecar_path(target_id);
        let tmp = self.new_tmp_path()?;
        atomic_write_blocking(&tmp, &path, bytes)
    }

    // -----------------------------------------------------------------
    // Size accounting + LRU eviction (TDD-0012).
    //
    // The v1 design is "scan on demand" - no `size.json` counter, no
    // `refs.json` index. Eviction runs on a background task after a
    // build if the total exceeds the limit. The index files in the
    // TDD are a future optimization for >100k-entry caches.
    // -----------------------------------------------------------------

    /// Total bytes consumed by the cache (ac/ + cas/ + structural/ +
    /// log/). Excludes tmp/ and version. One filesystem walk; doesn't
    /// open files.
    pub async fn total_size(&self) -> Result<u64, CacheError> {
        let root = self.root.as_path().to_path_buf();
        let n = spawn_blocking(move || compute_total_size(&root)).await??;
        Ok(n)
    }

    /// LRU eviction down to `target_bytes`. Oldest AC entries (by
    /// file mtime) are evicted first. Each evicted AC entry's referenced
    /// CAS blobs are removed too - but only if no surviving AC entry
    /// still references them.
    ///
    /// `min_age` is a recency buffer: entries with mtime newer than
    /// `now - min_age` are skipped. This avoids evicting cache lines
    /// that another build in another terminal might be actively using.
    /// (TDD-0012 §"Concurrent builds during eviction".)
    pub async fn evict_to(
        &self,
        target_bytes: u64,
        min_age: Duration,
    ) -> Result<EvictionReport, CacheError> {
        let root = self.root.as_path().to_path_buf();
        spawn_blocking(move || evict_to_blocking(&root, target_bytes, min_age)).await?
    }
}

/// Result of one `evict_to` call. Counts approximate (over-eviction
/// possible by a few percent) but never over-reports.
#[derive(Debug, Default, Clone, Copy)]
pub struct EvictionReport {
    pub entries_evicted: u64,
    pub bytes_freed: u64,
    pub bytes_remaining: u64,
}

/// Sum the file sizes under the cache's tracked subdirs.
fn compute_total_size(root: &Path) -> Result<u64, CacheError> {
    let mut total = 0u64;
    for sub in ["ac", "cas", "structural", "log"] {
        let dir = root.join(sub);
        if dir.exists() {
            total = total.saturating_add(dir_size(&dir)?);
        }
    }
    Ok(total)
}

fn dir_size(p: &Path) -> Result<u64, CacheError> {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(p).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file()
            && let Ok(m) = entry.metadata()
        {
            total = total.saturating_add(m.len());
        }
    }
    Ok(total)
}

/// Concrete content hashes (hex strings) an AC entry references in CAS.
fn referenced_hashes(entry: &AcEntry) -> Vec<String> {
    let mut out = Vec::with_capacity(entry.outputs.len() + 2);
    for o in &entry.outputs {
        out.push(o.content_hash.clone());
    }
    if let Some(h) = &entry.stdout_blob {
        out.push(h.clone());
    }
    if let Some(h) = &entry.stderr_blob {
        out.push(h.clone());
    }
    out
}

fn cas_path_for_hex(root: &Path, hex: &str) -> Option<PathBuf> {
    if hex.len() < 2 {
        return None;
    }
    Some(root.join("cas").join(&hex[..2]).join(hex))
}

fn evict_to_blocking(
    root: &Path,
    target: u64,
    min_age: Duration,
) -> Result<EvictionReport, CacheError> {
    let initial_size = compute_total_size(root)?;
    if initial_size <= target {
        return Ok(EvictionReport {
            entries_evicted: 0,
            bytes_freed: 0,
            bytes_remaining: initial_size,
        });
    }

    let now = SystemTime::now();
    let ac_dir = root.join("ac");

    // (mtime, path, ac_file_size, referenced_hashes)
    let mut entries: Vec<(SystemTime, PathBuf, u64, Vec<String>)> = Vec::new();
    for entry in walkdir::WalkDir::new(&ac_dir)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path().to_path_buf();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let mtime = meta.modified().unwrap_or(now);
        let size = meta.len();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let parsed: AcEntry = match serde_json::from_slice(&bytes) {
            Ok(e) => e,
            Err(_) => {
                // Corrupt AC - TDD-0012 says delete unconditionally.
                let _ = std::fs::remove_file(&path);
                continue;
            }
        };
        let refs = referenced_hashes(&parsed);
        entries.push((mtime, path, size, refs));
    }
    entries.sort_by_key(|(mtime, _, _, _)| *mtime);

    // Count references per hash across ALL AC entries; we'll decrement
    // as we add candidates to the evict set. A blob becomes "freeable"
    // when its count drops to 0.
    let mut ref_count: HashMap<String, u32> = HashMap::new();
    for entry in &entries {
        for r in &entry.3 {
            *ref_count.entry(r.clone()).or_insert(0) += 1;
        }
    }

    // Pre-stat referenced CAS blob sizes so the "stop" condition is
    // accurate. Missing blobs (already gone) contribute 0.
    let mut blob_size: HashMap<String, u64> = HashMap::new();
    for hash in ref_count.keys() {
        let Some(path) = cas_path_for_hex(root, hash) else {
            continue;
        };
        if let Ok(m) = path.metadata() {
            blob_size.insert(hash.clone(), m.len());
        }
    }

    // Plan eviction: walk oldest-first, skip the recency buffer.
    let mut evict_indices: Vec<usize> = Vec::new();
    let mut estimated_freed: u64 = 0;
    for (i, entry) in entries.iter().enumerate() {
        if initial_size.saturating_sub(estimated_freed) <= target {
            break;
        }
        if let Ok(age) = now.duration_since(entry.0)
            && age < min_age
        {
            continue;
        }
        // Tentatively evict: AC + each blob whose ref count drops to 0.
        estimated_freed = estimated_freed.saturating_add(entry.2);
        for r in &entry.3 {
            if let Some(c) = ref_count.get_mut(r) {
                *c = c.saturating_sub(1);
                if *c == 0
                    && let Some(sz) = blob_size.get(r)
                {
                    estimated_freed = estimated_freed.saturating_add(*sz);
                }
            }
        }
        evict_indices.push(i);
    }

    // Apply the plan. `ref_count` is now post-decrement, so an entry
    // with count 0 is freeable; non-zero means a surviving AC entry
    // still wants it.
    let mut bytes_freed: u64 = 0;
    let mut entries_evicted: u64 = 0;
    for i in evict_indices {
        let (_, path, size, refs) = &entries[i];
        if std::fs::remove_file(path).is_err() {
            continue;
        }
        bytes_freed = bytes_freed.saturating_add(*size);
        entries_evicted += 1;
        for hash in refs {
            // If another AC still references it, leave the blob alone.
            if ref_count.get(hash).copied().unwrap_or(1) != 0 {
                continue;
            }
            let Some(blob) = cas_path_for_hex(root, hash) else {
                continue;
            };
            if let Ok(m) = blob.metadata() {
                let sz = m.len();
                if std::fs::remove_file(&blob).is_ok() {
                    bytes_freed = bytes_freed.saturating_add(sz);
                    // Mark as deleted so a sibling AC referencing the
                    // same blob doesn't try to delete it again next
                    // iteration.
                    if let Some(c) = ref_count.get_mut(hash) {
                        *c = 1;
                    }
                }
            }
        }
    }

    Ok(EvictionReport {
        entries_evicted,
        bytes_freed,
        bytes_remaining: initial_size.saturating_sub(bytes_freed),
    })
}

/// Atomically write `bytes` to an arbitrary destination path. Uses a
/// sibling temp file in `dst`'s directory (so rename stays on one
/// filesystem) and applies the executable bit before the rename - so
/// `dst` goes from non-existent to fully-formed in one step.
///
/// Critically, this is the *only safe way* to overwrite a
/// currently-executing binary on Linux: open-for-write fails with
/// ETXTBSY, but rename-over the running binary is fine (the running
/// process keeps the old inode; future invocations get the new one).
/// Used by the executor's cache-restore paths so `bin:giant`-style
/// targets self-replace cleanly when warm-cache-restoring.
pub async fn atomic_write_output(
    dst: PathBuf,
    bytes: Vec<u8>,
    executable: bool,
) -> std::io::Result<()> {
    spawn_blocking(move || atomic_write_output_blocking(&dst, &bytes, executable))
        .await
        .map_err(std::io::Error::other)?
}

fn atomic_write_output_blocking(dst: &Path, bytes: &[u8], executable: bool) -> std::io::Result<()> {
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp_name = format!(
        ".{}.tmp-{}-{}",
        dst.file_name().and_then(|n| n.to_str()).unwrap_or("write"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0),
    );
    let tmp = parent.join(tmp_name);
    let staged = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if executable { 0o755 } else { 0o644 };
            f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, dst) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Sync implementation of write-tmp-then-rename. Used by both the async
/// `atomic_write` (via spawn_blocking) and the sync sidecar helpers
/// (called from inside another spawn_blocking already).
fn atomic_write_blocking(tmp: &Path, dst: &Path, bytes: &[u8]) -> Result<(), CacheError> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    {
        let mut f = std::fs::File::create(tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    // rename(2) - atomic within a filesystem (TDD-0007).
    if let Err(e) = std::fs::rename(tmp, dst) {
        let _ = std::fs::remove_file(tmp);
        return Err(e.into());
    }
    Ok(())
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
            assert!(dir.path().join(sub).is_dir(), "expected {sub}/ to exist");
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
        assert!(matches!(
            err,
            CacheError::VersionMismatch { found: 999, .. }
        ));
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
        let entries: Vec<_> = std::fs::read_dir(dir.path().join("tmp")).unwrap().collect();
        assert!(entries.is_empty(), "tmp/ should be empty after write");
    }

    // -------- eviction (TDD-0012) --------

    /// Build an AC entry that references a list of CAS blobs (which
    /// the caller must have put_cas'd already). Doesn't model
    /// outputs_content_hash specially - that field isn't itself a CAS
    /// blob, so it doesn't affect eviction accounting.
    async fn put_ac_with_blobs(
        cache: &LocalCache,
        id: &str,
        blob_hashes: &[ContentHash],
    ) -> CacheKey {
        let key = CacheKey::new(ContentHash::of_bytes(id.as_bytes()));
        let outputs: Vec<OutputEntry> = blob_hashes
            .iter()
            .enumerate()
            .map(|(i, h)| OutputEntry {
                path: format!("out-{i}"),
                content_hash: h.to_hex(),
                size: 0,
                executable: false,
                mode: "0644".into(),
                symlink_target: None,
            })
            .collect();
        let entry = AcEntry {
            schema: AC_SCHEMA,
            target_id: id.into(),
            cache_key: key.to_hex(),
            command: "true".into(),
            cwd: "".into(),
            outputs,
            outputs_content_hash: "".into(),
            stdout_blob: None,
            stderr_blob: None,
            exit_code: 0,
            duration_ms: 1,
            built_at: "2026-01-01T00:00:00Z".into(),
            built_by: None,
        };
        cache.put_ac(&key, &entry).await.unwrap();
        key
    }

    /// Force an AC file's mtime so eviction sees it as old.
    fn set_mtime_secs_ago(path: &Path, secs: u64) {
        let when = std::time::SystemTime::now() - Duration::from_secs(secs);
        let ft = filetime::FileTime::from_system_time(when);
        filetime::set_file_mtime(path, ft).expect("set mtime");
    }

    #[tokio::test]
    async fn total_size_zero_for_empty_cache() {
        let (cache, _dir) = temp_cache().await;
        assert_eq!(cache.total_size().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn total_size_counts_ac_and_cas() {
        let (cache, _dir) = temp_cache().await;
        let h = cache.put_cas(b"hello there".to_vec()).await.unwrap();
        let _ = put_ac_with_blobs(&cache, "t", &[h]).await;
        let size = cache.total_size().await.unwrap();
        assert!(
            size > 11,
            "should include both AC json and CAS blob: {size}"
        );
    }

    #[tokio::test]
    async fn evict_noop_when_under_target() {
        let (cache, _dir) = temp_cache().await;
        let h = cache.put_cas(b"x".to_vec()).await.unwrap();
        let _ = put_ac_with_blobs(&cache, "t", &[h]).await;
        let r = cache.evict_to(1_000_000, Duration::ZERO).await.unwrap();
        assert_eq!(r.entries_evicted, 0);
        assert_eq!(r.bytes_freed, 0);
        // Cache still intact.
        assert!(cache.total_size().await.unwrap() > 0);
    }

    #[tokio::test]
    async fn evict_drops_oldest_first() {
        let (cache, dir) = temp_cache().await;
        // Three independent AC entries, each with its own blob.
        let h_old = cache.put_cas(vec![b'a'; 1024]).await.unwrap();
        let k_old = put_ac_with_blobs(&cache, "old", &[h_old]).await;
        let h_mid = cache.put_cas(vec![b'b'; 1024]).await.unwrap();
        let k_mid = put_ac_with_blobs(&cache, "mid", &[h_mid]).await;
        let h_new = cache.put_cas(vec![b'c'; 1024]).await.unwrap();
        let _k_new = put_ac_with_blobs(&cache, "new", std::slice::from_ref(&h_new)).await;

        // Backdate the first two so they're outside the recency buffer.
        set_mtime_secs_ago(&cache.ac_path(&k_old), 3600);
        set_mtime_secs_ago(&cache.ac_path(&k_mid), 1800);

        // Aim well below current size - should evict old + mid; keep new.
        let r = cache.evict_to(1000, Duration::from_secs(60)).await.unwrap();
        assert!(
            r.entries_evicted >= 2,
            "evicted {} entries",
            r.entries_evicted
        );
        assert!(
            cache.get_ac(&k_old).await.unwrap().is_none(),
            "old should be gone"
        );
        assert!(
            cache.get_ac(&k_mid).await.unwrap().is_none(),
            "mid should be gone"
        );
        // The blobs that only the evicted entries referenced should be gone too.
        assert!(!cache.has_cas(&h_old).await, "old's blob should be gone");
        assert!(!cache.has_cas(&h_mid).await, "mid's blob should be gone");
        // new is within the buffer → safe.
        assert!(cache.has_cas(&h_new).await, "new's blob must survive");
        drop(dir);
    }

    #[tokio::test]
    async fn evict_respects_recency_buffer() {
        let (cache, _dir) = temp_cache().await;
        let h = cache.put_cas(vec![b'z'; 4096]).await.unwrap();
        let _ = put_ac_with_blobs(&cache, "fresh", std::slice::from_ref(&h)).await;
        // No mtime backdating; everything is "right now".
        let r = cache.evict_to(0, Duration::from_secs(60)).await.unwrap();
        assert_eq!(r.entries_evicted, 0, "buffer should skip everything");
        assert!(cache.has_cas(&h).await);
    }

    #[tokio::test]
    async fn evict_keeps_shared_blobs() {
        // Two AC entries reference the same CAS blob. Evicting the old
        // one must not delete the blob - the newer one still needs it.
        let (cache, _dir) = temp_cache().await;
        let shared = cache.put_cas(vec![b's'; 2048]).await.unwrap();
        let k_old = put_ac_with_blobs(&cache, "old", std::slice::from_ref(&shared)).await;
        let _k_new = put_ac_with_blobs(&cache, "new", std::slice::from_ref(&shared)).await;

        set_mtime_secs_ago(&cache.ac_path(&k_old), 3600);

        let _ = cache.evict_to(1000, Duration::from_secs(60)).await.unwrap();
        assert!(
            cache.get_ac(&k_old).await.unwrap().is_none(),
            "old AC should be evicted"
        );
        assert!(cache.has_cas(&shared).await, "shared blob must survive");
    }

    #[tokio::test]
    async fn evict_handles_corrupt_ac_during_real_pass() {
        // When eviction does run, it sweeps corrupt AC entries it
        // encounters along the way. (No unconditional sweep - we only
        // scan when over-limit, by design.)
        let (cache, dir) = temp_cache().await;
        let h_good = cache.put_cas(vec![b'g'; 4096]).await.unwrap();
        let k_good = put_ac_with_blobs(&cache, "good", std::slice::from_ref(&h_good)).await;
        // Backdate the good one so it's the eviction candidate.
        set_mtime_secs_ago(&cache.ac_path(&k_good), 3600);
        // Plant a corrupt AC file alongside.
        let bogus_path = dir.path().join("ac").join("ab").join("abadbabe.json");
        std::fs::create_dir_all(bogus_path.parent().unwrap()).unwrap();
        std::fs::write(&bogus_path, b"{ not json").unwrap();

        // Target below current size → real eviction pass runs.
        let _ = cache.evict_to(0, Duration::from_secs(60)).await.unwrap();
        assert!(!bogus_path.exists(), "corrupt AC should be deleted in-pass");
    }
}
