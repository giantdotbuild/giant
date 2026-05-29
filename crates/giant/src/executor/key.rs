//! Cache-key composition (TDD-0009). Computes a target's content-addressed
//! cache key from command, cwd, env, file inputs, structural inputs, and
//! dep output hashes. `giant explain` reaches in via the `_with_breakdown`
//! variant to show users what fed the hash.

use super::ExecutorError;
use crate::cache::LocalCache;
use crate::model::{CacheKey, ContentHash, Input, TargetId, TargetSpec};
use crate::paths::AbsPath;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Built-in env contributions for the cache key (see TDD-0007).
const GIANT_VERSION: &str = env!("CARGO_PKG_VERSION");
const TARGET_TRIPLE: &str = env!("GIANT_TARGET_TRIPLE");

/// Schema version for the cache-key composition. Bump on any change.
const KEY_SCHEMA: &str = "v1";

/// Breakdown of what went into a target's cache key. Populated when the
/// caller asks for it (see `compute_cache_key_with_breakdown`); the
/// dispatcher's `compute_cache_key` just discards it. Used by
/// `giant explain` to show users where the bytes that produced the
/// final hash came from.
#[derive(Debug, Clone)]
pub struct CacheKeyBreakdown {
    pub schema: String,
    pub command: String,
    pub cwd: String,
    pub user_env: std::collections::BTreeMap<String, String>,
    pub built_in_env: std::collections::BTreeMap<String, String>,
    pub file_inputs: Vec<FileInputContribution>,
    pub structural_inputs: Vec<StructuralContribution>,
    /// dep_id → output_content_hash. The caller fills this in *after*
    /// the hash is computed (the inner compose has no view of dep IDs).
    pub dep_outputs: std::collections::BTreeMap<TargetId, ContentHash>,
}

#[derive(Debug, Clone)]
pub struct FileInputContribution {
    pub rel_path: String,
    pub content_hash: ContentHash,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct StructuralContribution {
    pub files: Vec<String>,
    pub lines: Vec<String>,
    pub scope: Vec<String>,
    pub fingerprint: ContentHash,
}

/// Compute the cache key for a target. See TDD-0009 §Cache key composition.
///
/// `dep_output_hashes` is each direct dep's output content hash - *not* its
/// cache key. This is the early-cutoff property: byte-identical upstream
/// rebuilds leave downstream cache keys unchanged.
pub(super) async fn compute_cache_key(
    spec: &TargetSpec,
    workspace_root: &AbsPath,
    cache: &LocalCache,
    dep_output_hashes: &[ContentHash],
) -> Result<CacheKey, ExecutorError> {
    let spec = spec.clone();
    let workspace_root = workspace_root.clone();
    let cache = cache.clone();
    let dep_output_hashes = dep_output_hashes.to_vec();
    let hash = tokio::task::spawn_blocking(move || {
        compose_cache_key_blocking(&spec, &workspace_root, &cache, &dep_output_hashes, None)
    })
    .await
    .map_err(|e| ExecutorError::Io(std::io::Error::other(e.to_string())))??;
    Ok(CacheKey::new(hash))
}

/// Like `compute_cache_key` but also returns a `CacheKeyBreakdown` so
/// callers (`giant explain`) can show users what fed into the hash.
pub async fn compute_cache_key_with_breakdown(
    spec: &TargetSpec,
    workspace_root: &AbsPath,
    cache: &LocalCache,
    dep_outputs: std::collections::BTreeMap<TargetId, ContentHash>,
) -> Result<(CacheKey, CacheKeyBreakdown), ExecutorError> {
    let spec = spec.clone();
    let workspace_root = workspace_root.clone();
    let cache = cache.clone();
    let dep_output_hashes: Vec<ContentHash> = dep_outputs.values().copied().collect();
    let (key, mut bd) = tokio::task::spawn_blocking(
        move || -> Result<(ContentHash, CacheKeyBreakdown), std::io::Error> {
            let mut bd = empty_breakdown(&spec);
            let h = compose_cache_key_blocking(
                &spec,
                &workspace_root,
                &cache,
                &dep_output_hashes,
                Some(&mut bd),
            )?;
            Ok((h, bd))
        },
    )
    .await
    .map_err(|e| ExecutorError::Io(std::io::Error::other(e.to_string())))??;
    // Fill in dep_outputs with real IDs (the inner compose has no view).
    bd.dep_outputs = dep_outputs;
    Ok((CacheKey::new(key), bd))
}

fn empty_breakdown(spec: &TargetSpec) -> CacheKeyBreakdown {
    let mut built_in = std::collections::BTreeMap::new();
    built_in.insert("GIANT_TARGET_TRIPLE".into(), TARGET_TRIPLE.into());
    built_in.insert("GIANT_VERSION".into(), GIANT_VERSION.into());
    CacheKeyBreakdown {
        schema: KEY_SCHEMA.to_string(),
        command: spec.command.clone(),
        cwd: spec.cwd.as_path().to_string_lossy().into_owned(),
        user_env: spec
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        built_in_env: built_in,
        file_inputs: Vec::new(),
        structural_inputs: Vec::new(),
        dep_outputs: std::collections::BTreeMap::new(),
    }
}

/// The actual cache-key hash composition. Sync; caller wraps in
/// spawn_blocking. If `breakdown` is `Some`, populates `file_inputs` and
/// `structural_inputs` alongside hashing so `giant explain` can show
/// exactly what bytes fed the hash.
fn compose_cache_key_blocking(
    spec: &TargetSpec,
    workspace_root: &AbsPath,
    cache: &LocalCache,
    dep_output_hashes: &[ContentHash],
    mut breakdown: Option<&mut CacheKeyBreakdown>,
) -> Result<ContentHash, std::io::Error> {
    let mut h = ContentHash::hasher();
    h.update(KEY_SCHEMA.as_bytes());
    h.update(b"\0");

    // command
    h.update(b"cmd\0");
    h.update(spec.command.as_bytes());
    h.update(b"\0");

    // cwd
    h.update(b"cwd\0");
    h.update(spec.cwd.as_path().to_string_lossy().as_bytes());
    h.update(b"\0");

    // env (sorted by key) + built-in target triple + version
    h.update(b"env\0");
    let mut env_keys: Vec<&String> = spec.env.keys().collect();
    env_keys.sort();
    for k in env_keys {
        h.update(k.as_bytes());
        h.update(b"=");
        h.update(spec.env[k].as_bytes());
        h.update(b"\0");
    }
    h.update(b"GIANT_TARGET_TRIPLE=");
    h.update(TARGET_TRIPLE.as_bytes());
    h.update(b"\0");
    h.update(b"GIANT_VERSION=");
    h.update(GIANT_VERSION.as_bytes());
    h.update(b"\0");

    // file inputs (expand globs, sort, hash content)
    h.update(b"file_inputs\0");
    let mut file_globs: Vec<&str> = Vec::new();
    // Collect structural-input specs as we walk; they hash separately
    // so the section is independent of file-input order.
    let mut structurals: Vec<StructuralSpec> = Vec::new();
    for input in &spec.inputs {
        match input {
            Input::File { glob } => {
                file_globs.push(glob.as_str());
            }
            Input::Structural {
                files,
                lines,
                scope,
            } => {
                structurals.push(StructuralSpec {
                    files: files.iter().map(|g| g.as_str().to_string()).collect(),
                    lines: lines.clone(),
                    scope: scope
                        .iter()
                        .map(|s| s.as_path().to_string_lossy().into_owned())
                        .collect(),
                });
            }
        }
    }
    // Canonicalise the structural specs ahead of time so the
    // parallel-computed section is deterministic on both sides of
    // the join. Sorts are cheap on a handful of specs.
    for s in &mut structurals {
        s.files.sort();
        s.lines.sort();
        s.scope.sort();
    }
    structurals.sort();

    // Independent work - run concurrently. The file-input branch
    // walks + hashes; the structural branch consults the gix
    // fast-path / sidecar. Neither touches the other's data, so
    // rayon::join gives us a clean wall-time overlap of the slower
    // of the two phases.
    let (file_result, structural_result): (
        Result<Vec<FileInputItem>, std::io::Error>,
        Result<Vec<ContentHash>, std::io::Error>,
    ) = rayon::join(
        || compute_file_inputs(workspace_root, cache, &file_globs),
        || compute_structural_inputs(workspace_root, cache, &spec.id, &structurals),
    );
    let file_items = file_result?;
    let structural_items = structural_result?;

    // Hash file_inputs section in deterministic order.
    for item in &file_items {
        h.update(item.rel_path.as_bytes());
        h.update(b"\0");
        h.update(item.content_hash.as_bytes());
        h.update(b"\0");
        if let Some(bd) = breakdown.as_deref_mut() {
            bd.file_inputs.push(FileInputContribution {
                rel_path: item.rel_path.clone(),
                content_hash: item.content_hash,
                size: item.size,
            });
        }
    }

    // Hash structural_inputs section. Same shape as before.
    h.update(b"structural_inputs\0");
    for (s, fp) in structurals.iter().zip(structural_items.iter()) {
        for f in &s.files {
            h.update(f.as_bytes());
            h.update(b"\0");
        }
        h.update(b"|\0");
        for l in &s.lines {
            h.update(l.as_bytes());
            h.update(b"\0");
        }
        h.update(b"|\0");
        for sc in &s.scope {
            h.update(sc.as_bytes());
            h.update(b"\0");
        }
        h.update(b"|\0");
        h.update(fp.as_bytes());
        h.update(b"\0");
        if let Some(bd) = breakdown.as_deref_mut() {
            bd.structural_inputs.push(StructuralContribution {
                files: s.files.clone(),
                lines: s.lines.clone(),
                scope: s.scope.clone(),
                fingerprint: *fp,
            });
        }
    }

    // dep output content hashes (sorted). Hashing each dep's *output*
    // content (not its cache key) is what gives the early-cutoff
    // property: a byte-identical upstream rebuild leaves this section -
    // and so the whole key - unchanged.
    h.update(b"dep_outputs\0");
    let mut sorted: Vec<[u8; 32]> = dep_output_hashes.iter().map(|h| *h.as_bytes()).collect();
    sorted.sort();
    for hb in &sorted {
        h.update(hb);
        h.update(b"\0");
    }

    Ok(h.finalize())
}

/// One hashed file input ready to be folded into the cache-key hash.
/// Order is fixed by `compute_file_inputs` (sorted by `rel_path`).
struct FileInputItem {
    rel_path: String,
    content_hash: ContentHash,
    size: u64,
}

/// Walk + hash file inputs. Walk is parallel via `expand_globs_batched`;
/// hashing is parallel via rayon over the sorted path list. Sequential
/// hashing was visibly slow on warm runs of large discovery targets
/// (~1 ms per file × 70+ files); rayon brings that to ~10 ms.
fn compute_file_inputs(
    workspace_root: &AbsPath,
    cache: &LocalCache,
    globs: &[&str],
) -> Result<Vec<FileInputItem>, std::io::Error> {
    use rayon::prelude::*;

    let mut paths = expand_globs_batched(workspace_root.as_path(), globs, cache)?;
    paths.sort();
    paths.dedup();

    paths
        .par_iter()
        .map(|p| {
            let rel = p
                .strip_prefix(workspace_root.as_path())
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned();
            let content_hash = ContentHash::of_file(p)?;
            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            Ok(FileInputItem {
                rel_path: rel,
                content_hash,
                size,
            })
        })
        .collect()
}

/// Compute structural-input fingerprints. Sequential across structural
/// specs (usually only 1-2 per target) since `compute_fingerprint`
/// uses gix and a per-target sidecar - contention rather than gain if
/// parallelised at this layer.
fn compute_structural_inputs(
    workspace_root: &AbsPath,
    cache: &LocalCache,
    target_id: &TargetId,
    structurals: &[StructuralSpec],
) -> Result<Vec<ContentHash>, std::io::Error> {
    structurals
        .iter()
        .map(|s| {
            crate::structural::compute_fingerprint(
                workspace_root,
                cache,
                target_id,
                &s.files,
                &s.lines,
                &s.scope,
            )
            .map_err(|e| std::io::Error::other(e.to_string()))
        })
        .collect()
}

/// Internal canonical representation of one structural input, used to
/// build a deterministic section of the cache key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct StructuralSpec {
    files: Vec<String>,
    lines: Vec<String>,
    scope: Vec<String>,
}

/// Expand a set of input globs against the workspace at most once.
///
/// Without this, declaring N globs that contain `**` would walk the
/// workspace N times. We split the input list into:
///   - **literal paths**: no glob metachars; resolved via single `stat`
///   - **shallow globs**: no `**`; cheap, resolved by `glob::glob` (it
///     reads only directories the pattern actually traverses)
///   - **recursive globs**: contain `**`; matched against ONE shared
///     workspace walk (`walkdir`) that visits each directory once.
///
/// The shared walk also prunes known noise (`.git`, `.giant`, the
/// configured cache directory) to keep the visit budget bounded.
fn expand_globs_batched(
    workspace_root: &Path,
    globs: &[&str],
    cache: &LocalCache,
) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut recursive: Vec<String> = Vec::new();

    for &g in globs {
        if g.contains("**") {
            recursive.push(g.to_string());
        } else if has_glob_metachars(g) {
            // Shallow glob - let the glob crate handle it; it'll only
            // read the directories actually referenced by the pattern.
            let full = workspace_root.join(g).to_string_lossy().into_owned();
            let entries = glob::glob(&full)
                .map_err(|e| std::io::Error::other(format!("bad glob {g:?}: {e}")))?;
            for entry in entries.flatten() {
                if entry.is_file() {
                    out.push(entry);
                }
            }
        } else {
            // Literal path - one stat, no walk.
            let p = workspace_root.join(g);
            if p.is_file() {
                out.push(p);
            }
        }
    }

    if recursive.is_empty() {
        return Ok(out);
    }

    // Compile all recursive patterns into a single GlobSet. Internally
    // this builds Aho-Corasick literal prefilters across all patterns,
    // so matching a path against N globs costs roughly the same as
    // matching one. With per-pattern `glob::Pattern::matches_path` in a
    // loop, cost grew O(N) per file visited.
    let mut gs = globset::GlobSetBuilder::new();
    for g in &recursive {
        let glob = globset::Glob::new(g)
            .map_err(|e| std::io::Error::other(format!("bad glob {g:?}: {e}")))?;
        gs.add(glob);
    }
    let glob_set = Arc::new(
        gs.build()
            .map_err(|e| std::io::Error::other(format!("glob set build: {e}")))?,
    );

    // Shared parallel walk for all recursive patterns.
    //
    // Standard filters off so the semantics match `glob::glob` exactly:
    // declared inputs are matched against the literal filesystem, not
    // against what git tracks.
    let cache_dir = cache.root().as_path().to_path_buf();

    // Dot-prefixed VCS/tool dirs and common build outputs: walking
    // them is pure waste for input matching, and excluding them gives
    // a substantial speedup on real-world monorepos. If a user really
    // does have inputs underneath one of these, they'd be in the
    // gitignored territory and using giant from there is unusual; we
    // can revisit if a real case shows up.
    let skip_names: &[&str] = &[
        ".git",
        ".giant",
        ".direnv",
        ".devenv",
        "node_modules",
        "target",
    ];

    let matches: Arc<std::sync::Mutex<Vec<PathBuf>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    let workspace_root_owned = workspace_root.to_path_buf();

    ignore::WalkBuilder::new(workspace_root)
        .standard_filters(false)
        .hidden(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            let path = entry.path();
            if path == cache_dir.as_path() {
                return false;
            }
            if let Some(name) = entry.file_name().to_str()
                && skip_names.contains(&name)
            {
                return false;
            }
            true
        })
        .build_parallel()
        .run(|| {
            let matches = Arc::clone(&matches);
            let workspace_root = workspace_root_owned.clone();
            let glob_set = Arc::clone(&glob_set);
            Box::new(move |result| {
                let Ok(entry) = result else {
                    return ignore::WalkState::Continue;
                };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    return ignore::WalkState::Continue;
                }
                let path = entry.path();
                let rel = path.strip_prefix(&workspace_root).unwrap_or(path);
                if glob_set.is_match(rel) {
                    matches.lock().unwrap().push(path.to_path_buf());
                }
                ignore::WalkState::Continue
            })
        });

    let mut found = Arc::try_unwrap(matches)
        .map(|m| m.into_inner().unwrap())
        .unwrap_or_else(|arc| std::mem::take(&mut *arc.lock().unwrap()));
    out.append(&mut found);

    Ok(out)
}

fn has_glob_metachars(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}
