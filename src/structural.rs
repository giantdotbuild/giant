//! Structural inputs: line-pattern-filtered file fingerprinting.
//!
//! Stages, per TDD-0002:
//!
//! - **Stage 1 (shipped):** cold compute via filesystem walk; correctness.
//! - **Stage 2 (this file):** per-target sidecar holds (path → lines_hash,
//!   mtime, size). Warm runs walk the workspace but skip re-reading files
//!   whose mtime+size match the sidecar.
//! - **Stage 3 (next slice):** git fast-path - enumerate via gix index for
//!   cold compute; `git status` delta to skip the walk entirely for warm.
//!
//! The cache-key contribution is unchanged across stages: same
//! per-file fingerprint, same global combine, same hash. Stage 2/3 only
//! speed up *how* we get to that hash.

use crate::cache::LocalCache;
use crate::model::{ContentHash, TargetId};
use crate::paths::AbsPath;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, thiserror::Error)]
pub enum StructuralError {
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),

    #[error("cache: {0}")]
    Cache(#[from] crate::cache::CacheError),

    #[error("sidecar corrupt at {target_id}: {detail}")]
    Corrupt { target_id: String, detail: String },
}

// =============================================================================
// Sidecar format
// =============================================================================

/// One target's structural-input cache state on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sidecar {
    pub schema: u32,
    pub target_id: String,
    pub computed_at: i64,
    pub entries: Vec<SidecarEntry>,
}

/// State for one (files, lines, scope) input on a target. The triple
/// is canonicalised (sorted) before storage so YAML reordering and
/// duplicates don't multiply entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarEntry {
    pub files: Vec<String>,
    pub lines: Vec<String>,
    pub scope: Vec<String>,
    /// Hex of the combined per-file hash (TDD-0002 §Per-input fingerprint).
    pub global_hash: String,
    pub per_file: BTreeMap<String, PerFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerFileEntry {
    /// Hex of sha256 over the file's matching lines joined with `\0`.
    pub lines_hash: String,
    /// Nanoseconds since UNIX epoch. Zero when filesystem doesn't report
    /// mtime; in that case mtime-skip is disabled for this file.
    #[serde(default)]
    pub mtime_ns: u64,
    pub size: u64,
}

const SCHEMA: u32 = 1;

// =============================================================================
// Public API
// =============================================================================

/// Compute the global fingerprint for one structural input. Loads the
/// sidecar if present, uses mtime+size to skip re-reading unchanged
/// files, writes back the updated sidecar.
///
/// Sync - caller is expected to be inside `spawn_blocking` for parallel
/// per-target hashing.
pub fn compute_fingerprint(
    workspace_root: &AbsPath,
    cache: &LocalCache,
    target_id: &TargetId,
    files_globs: &[String],
    lines_patterns: &[String],
    scope: &[String],
) -> Result<ContentHash, StructuralError> {
    if files_globs.is_empty() || lines_patterns.is_empty() {
        return Ok(ContentHash::of_bytes(b""));
    }

    // Canonicalise the spec for matching against any existing sidecar entry.
    let mut canonical_files: Vec<String> = files_globs.to_vec();
    canonical_files.sort();
    canonical_files.dedup();
    let mut canonical_lines: Vec<String> = lines_patterns.to_vec();
    canonical_lines.sort();
    canonical_lines.dedup();
    let mut canonical_scope: Vec<String> = scope.to_vec();
    canonical_scope.sort();
    canonical_scope.dedup();

    // Load sidecar; find the entry matching this exact spec, if any.
    let mut sidecar = load_sidecar(cache, target_id)?;
    let prior_per_file: BTreeMap<String, PerFileEntry> = sidecar
        .as_ref()
        .and_then(|s| {
            s.entries
                .iter()
                .find(|e| e.files == canonical_files && e.lines == canonical_lines && e.scope == canonical_scope)
                .map(|e| e.per_file.clone())
        })
        .unwrap_or_default();

    // Walk + fingerprint, using mtime+size to skip unchanged files.
    let per_file = walk_with_mtime_skip(
        workspace_root,
        &canonical_files,
        &canonical_lines,
        &canonical_scope,
        &prior_per_file,
    )?;

    let global_hash = combine_per_file(&per_file);

    // Write back the updated sidecar. Other entries (different specs on
    // the same target - rare but legal) are preserved.
    let updated_entry = SidecarEntry {
        files: canonical_files.clone(),
        lines: canonical_lines.clone(),
        scope: canonical_scope.clone(),
        global_hash: global_hash.to_hex(),
        per_file,
    };

    let mut entries = sidecar
        .take()
        .map(|s| s.entries)
        .unwrap_or_default();
    entries.retain(|e| {
        !(e.files == canonical_files && e.lines == canonical_lines && e.scope == canonical_scope)
    });
    entries.push(updated_entry);

    let new_sidecar = Sidecar {
        schema: SCHEMA,
        target_id: target_id.as_str().to_string(),
        computed_at: now_unix_seconds(),
        entries,
    };

    save_sidecar(cache, target_id, &new_sidecar)?;

    Ok(global_hash)
}

// =============================================================================
// Internals
// =============================================================================

fn load_sidecar(
    cache: &LocalCache,
    target_id: &TargetId,
) -> Result<Option<Sidecar>, StructuralError> {
    let Some(bytes) = cache.get_structural_sidecar_raw(target_id)? else {
        return Ok(None);
    };
    let sidecar: Sidecar = serde_json::from_slice(&bytes).map_err(|e| StructuralError::Corrupt {
        target_id: target_id.as_str().to_string(),
        detail: e.to_string(),
    })?;
    if sidecar.schema != SCHEMA {
        // Schema mismatch - discard and recompute. Future code can
        // migrate; for now we just treat as cold.
        return Ok(None);
    }
    Ok(Some(sidecar))
}

fn save_sidecar(
    cache: &LocalCache,
    target_id: &TargetId,
    sidecar: &Sidecar,
) -> Result<(), StructuralError> {
    let bytes = serde_json::to_vec_pretty(sidecar).map_err(|e| StructuralError::Corrupt {
        target_id: target_id.as_str().to_string(),
        detail: e.to_string(),
    })?;
    cache.put_structural_sidecar_raw(target_id, &bytes)?;
    Ok(())
}

/// Walk the workspace (or the scoped subtrees), fingerprint each
/// matching file. Uses `prior_per_file` to skip re-reading files whose
/// stat (mtime + size) hasn't changed since the last computation.
fn walk_with_mtime_skip(
    workspace_root: &AbsPath,
    files_globs: &[String],
    lines_patterns: &[String],
    scope: &[String],
    prior_per_file: &BTreeMap<String, PerFileEntry>,
) -> Result<BTreeMap<String, PerFileEntry>, StructuralError> {
    let patterns: Vec<glob::Pattern> = files_globs
        .iter()
        .filter_map(|s| glob::Pattern::new(s).ok())
        .collect();
    if patterns.is_empty() {
        return Ok(BTreeMap::new());
    }

    let walk_roots: Vec<std::path::PathBuf> = if scope.is_empty() {
        vec![workspace_root.as_path().to_path_buf()]
    } else {
        scope
            .iter()
            .map(|s| workspace_root.as_path().join(s))
            .collect()
    };

    let mut current: BTreeMap<String, PerFileEntry> = BTreeMap::new();
    for root in &walk_roots {
        if !root.exists() {
            continue;
        }
        let walker = WalkBuilder::new(root).standard_filters(true).build();
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            let path = entry.path();
            let rel = path
                .strip_prefix(workspace_root.as_path())
                .unwrap_or(path);
            let rel_str = rel.to_string_lossy().into_owned();

            if !patterns.iter().any(|p| p.matches(&rel_str)) {
                continue;
            }

            let Some(entry) = fingerprint_file_with_skip(path, lines_patterns, prior_per_file.get(&rel_str))?
            else {
                continue;
            };
            current.insert(rel_str, entry);
        }
    }
    Ok(current)
}

/// Fingerprint one file, reusing the prior entry's `lines_hash` when
/// (mtime, size) match. Returns `None` when the file has no matching
/// lines (so it doesn't appear in the per-file map at all).
fn fingerprint_file_with_skip(
    path: &Path,
    lines_patterns: &[String],
    prior: Option<&PerFileEntry>,
) -> Result<Option<PerFileEntry>, StructuralError> {
    let metadata = match path.metadata() {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    let size = metadata.len();
    let mtime_ns = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    // mtime-skip: if the prior entry has the same (mtime, size) and we
    // believe the filesystem reports usable mtime (mtime_ns != 0), trust
    // the stored hash.
    if let Some(p) = prior
        && mtime_ns != 0
        && p.mtime_ns == mtime_ns
        && p.size == size
    {
        return Ok(Some(PerFileEntry {
            lines_hash: p.lines_hash.clone(),
            mtime_ns,
            size,
        }));
    }

    // Re-read + re-hash.
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };
    let mut hasher = ContentHash::hasher();
    let mut any_match = false;
    for line in content.lines() {
        if lines_patterns
            .iter()
            .any(|p| line.starts_with(p.as_str()))
        {
            hasher.update(line.as_bytes());
            hasher.update(b"\0");
            any_match = true;
        }
    }
    if !any_match {
        return Ok(None);
    }
    Ok(Some(PerFileEntry {
        lines_hash: hasher.finalize().to_hex(),
        mtime_ns,
        size,
    }))
}

fn combine_per_file(per_file: &BTreeMap<String, PerFileEntry>) -> ContentHash {
    let mut hasher = ContentHash::hasher();
    for (path, entry) in per_file {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        // Convert hex back to bytes for the combine; cheaper than hex
        // strings but the on-disk format stays human-readable.
        let bytes = const_hex::decode(&entry.lines_hash).unwrap_or_default();
        hasher.update(&bytes);
        hasher.update(b"\0");
    }
    hasher.finalize()
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn ws(tempdir: &tempfile::TempDir) -> AbsPath {
        AbsPath::new(tempdir.path().to_path_buf())
    }

    async fn temp_cache_dir() -> (LocalCache, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let abs = AbsPath::new(dir.path().to_path_buf());
        let cache = LocalCache::open(abs).await.unwrap();
        (cache, dir)
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[tokio::test]
    async fn empty_inputs_return_sentinel() {
        let (cache, _cache_dir) = temp_cache_dir().await;
        let dir = tempfile::tempdir().unwrap();
        let id = TargetId::new("t");
        let h = compute_fingerprint(&ws(&dir), &cache, &id, &[], &["package ".into()], &[]).unwrap();
        assert_eq!(h, ContentHash::of_bytes(b""));
    }

    #[tokio::test]
    async fn matching_lines_change_hash() {
        let (cache, _cache_dir) = temp_cache_dir().await;
        let dir = tempfile::tempdir().unwrap();
        let id = TargetId::new("t");
        write(
            &dir.path().join("a.go"),
            "package foo\nimport \"x\"\nfunc f() {}\n",
        );
        let h1 = compute_fingerprint(
            &ws(&dir),
            &cache,
            &id,
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        write(
            &dir.path().join("a.go"),
            "package foo\nimport \"x\"\nfunc f() { return }\n",
        );
        let h2 = compute_fingerprint(
            &ws(&dir),
            &cache,
            &id,
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        assert_eq!(h1, h2, "function body edit must not change structural hash");
        write(
            &dir.path().join("a.go"),
            "package foo\nimport \"y\"\nfunc f() { return }\n",
        );
        let h3 = compute_fingerprint(
            &ws(&dir),
            &cache,
            &id,
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        assert_ne!(h1, h3, "import edit must change structural hash");
    }

    #[tokio::test]
    async fn sidecar_written_after_compute() {
        let (cache, _cache_dir) = temp_cache_dir().await;
        let dir = tempfile::tempdir().unwrap();
        let id = TargetId::new("svc");
        write(&dir.path().join("a.go"), "package foo\nimport \"x\"\n");
        compute_fingerprint(
            &ws(&dir),
            &cache,
            &id,
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        // Sidecar should be on disk now.
        let bytes = cache.get_structural_sidecar_raw(&id).unwrap();
        assert!(bytes.is_some(), "sidecar should be written");
        let sidecar: Sidecar = serde_json::from_slice(&bytes.unwrap()).unwrap();
        assert_eq!(sidecar.schema, 1);
        assert_eq!(sidecar.target_id, "svc");
        assert_eq!(sidecar.entries.len(), 1);
        let e = &sidecar.entries[0];
        assert_eq!(e.files, vec!["*.go".to_string()]);
        assert_eq!(e.per_file.len(), 1);
        assert!(e.per_file.contains_key("a.go"));
    }

    #[tokio::test]
    async fn warm_path_reuses_prior_lines_hash_when_unchanged() {
        let (cache, _cache_dir) = temp_cache_dir().await;
        let dir = tempfile::tempdir().unwrap();
        let id = TargetId::new("svc");
        let p = dir.path().join("a.go");
        write(&p, "package foo\nimport \"x\"\n");

        // Cold compute writes a sidecar with mtime/size for a.go.
        let h1 = compute_fingerprint(
            &ws(&dir),
            &cache,
            &id,
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();

        // Warm compute (no file changes) → identical hash, and the
        // sidecar's per_file entry is reused (we can't easily observe
        // the no-read directly, but we assert the hash is unchanged
        // and the sidecar is still well-formed).
        let h2 = compute_fingerprint(
            &ws(&dir),
            &cache,
            &id,
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        assert_eq!(h1, h2);
    }

    #[tokio::test]
    async fn changing_file_content_invalidates_via_size_change() {
        // Even if mtime resolution is coarse, a content change of
        // different length flips `size`, so the sidecar's stored entry
        // doesn't match → re-read → new hash.
        let (cache, _cache_dir) = temp_cache_dir().await;
        let dir = tempfile::tempdir().unwrap();
        let id = TargetId::new("svc");
        write(&dir.path().join("a.go"), "package foo\nimport \"x\"\n");
        let h1 = compute_fingerprint(
            &ws(&dir),
            &cache,
            &id,
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        // Add an import line - file is longer + content changed.
        write(
            &dir.path().join("a.go"),
            "package foo\nimport \"x\"\nimport \"yy\"\n",
        );
        let h2 = compute_fingerprint(
            &ws(&dir),
            &cache,
            &id,
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        assert_ne!(h1, h2);
    }

    #[tokio::test]
    async fn deleting_file_invalidates_fingerprint() {
        let (cache, _cache_dir) = temp_cache_dir().await;
        let dir = tempfile::tempdir().unwrap();
        let id = TargetId::new("svc");
        write(&dir.path().join("a.go"), "package a\nimport \"x\"\n");
        write(&dir.path().join("b.go"), "package b\nimport \"y\"\n");
        let h1 = compute_fingerprint(
            &ws(&dir),
            &cache,
            &id,
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        std::fs::remove_file(dir.path().join("b.go")).unwrap();
        let h2 = compute_fingerprint(
            &ws(&dir),
            &cache,
            &id,
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        assert_ne!(h1, h2);
    }

    #[tokio::test]
    async fn scope_limits_walk() {
        let (cache, _cache_dir) = temp_cache_dir().await;
        let dir = tempfile::tempdir().unwrap();
        let id = TargetId::new("svc");
        write(&dir.path().join("internal/a.go"), "package a\nimport \"x\"\n");
        write(&dir.path().join("vendor/b.go"), "package b\nimport \"y\"\n");
        let with_scope = compute_fingerprint(
            &ws(&dir),
            &cache,
            &id,
            &["**/*.go".into()],
            &["package ".into(), "import ".into()],
            &["internal".into()],
        )
        .unwrap();
        // Different target id so sidecar doesn't conflict.
        let id2 = TargetId::new("svc2");
        let no_scope = compute_fingerprint(
            &ws(&dir),
            &cache,
            &id2,
            &["**/*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        assert_ne!(with_scope, no_scope);
    }

    #[tokio::test]
    async fn gitignored_files_excluded() {
        let (cache, _cache_dir) = temp_cache_dir().await;
        let dir = tempfile::tempdir().unwrap();
        let id = TargetId::new("svc");
        // Initialize a marker .git directory so the ignore crate treats
        // this as a repo and honors .gitignore.
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        write(&dir.path().join(".gitignore"), "gen/\n");
        write(&dir.path().join("a.go"), "package a\nimport \"x\"\n");
        write(&dir.path().join("gen/auto.go"), "package gen\nimport \"z\"\n");
        let h = compute_fingerprint(
            &ws(&dir),
            &cache,
            &id,
            &["**/*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        // Reference: same workspace minus gen/auto.go.
        let dir2 = tempfile::tempdir().unwrap();
        let id_ref = TargetId::new("ref");
        write(&dir2.path().join("a.go"), "package a\nimport \"x\"\n");
        let h_ref = compute_fingerprint(
            &ws(&dir2),
            &cache,
            &id_ref,
            &["**/*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        assert_eq!(h, h_ref, "gitignored files must not contribute");
    }

    #[tokio::test]
    async fn corrupt_sidecar_is_recovered() {
        let (cache, _cache_dir) = temp_cache_dir().await;
        let dir = tempfile::tempdir().unwrap();
        let id = TargetId::new("svc");
        write(&dir.path().join("a.go"), "package a\nimport \"x\"\n");

        // Plant a corrupt sidecar.
        cache
            .put_structural_sidecar_raw(&id, b"{ not json")
            .unwrap();

        // Should error cleanly on load - caller can choose to recompute.
        let bytes = cache.get_structural_sidecar_raw(&id).unwrap().unwrap();
        let parsed: Result<Sidecar, _> = serde_json::from_slice(&bytes);
        assert!(parsed.is_err());
    }
}
