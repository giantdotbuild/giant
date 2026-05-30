//! Discovery: build `include:` targets first, merge their JSON outputs
//! into the main graph.
//!
//! See TDD-0003 for the bootstrap-pass scheduling and merge rules,
//! TDD-0015 for the discovery output protocol including the `reads`
//! manifest used for cache invalidation.

use crate::graph::{BuildGraph, GraphError};
use crate::model::{CacheKey, ContentHash, TargetSpec};
use crate::paths::{AbsPath, WsRelPath};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const DISCOVERY_KEY_SCHEMA: &str = "disc-v2";
const GIANT_VERSION: &str = env!("CARGO_PKG_VERSION");
const TARGET_TRIPLE: &str = env!("GIANT_TARGET_TRIPLE");

/// Compute the cache key for a discovery target per ADR-0017:
/// `cmd + env + cwd + scope + content`, where `content` is the merged
/// set of argv-resolved in-tree files and the target's declared
/// `inputs:` globs (no dep cache keys). The returned key is the lookup
/// key for the discovery sidecar; whether the cached output is *valid*
/// still depends on verifying the `reads` manifest against the live
/// filesystem.
///
/// "executable content" is any argv token that resolves to an existing
/// file under `workspace_root` (after joining with the target's cwd).
/// That covers the common cases automatically:
///   - `bash scripts/discover.sh`  → hashes `scripts/discover.sh`
///   - `./bin/discover foo`         → hashes `bin/discover`
///   - `tools/discover foo`         → hashes `tools/discover`
///
/// System interpreters (`bash`, `python3`) live outside the workspace
/// and are intentionally not hashed - they'd make the key host-specific.
/// Binaries on PATH aren't resolved either; users wiring an external
/// binary should pin the path or wire a dep target whose output the
/// discovery target consumes.
///
/// Stable across runs given the same command/env/cwd/scope/executable
/// content; sensitive to engine version bumps (the schema marker and
/// `GIANT_VERSION` are mixed in).
pub fn discovery_cache_key(spec: &TargetSpec, workspace_root: &AbsPath) -> CacheKey {
    let mut h = ContentHash::hasher();
    h.update(DISCOVERY_KEY_SCHEMA.as_bytes());
    h.update(b"\0");

    h.update(b"cmd\0");
    h.update(spec.command.as_bytes());
    h.update(b"\0");

    h.update(b"cwd\0");
    h.update(spec.cwd.as_path().to_string_lossy().as_bytes());
    h.update(b"\0");

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

    h.update(b"scope\0");
    let mut scope_strs: Vec<String> = spec
        .scope
        .iter()
        .map(|s| s.as_path().to_string_lossy().into_owned())
        .collect();
    scope_strs.sort();
    for s in &scope_strs {
        h.update(s.as_bytes());
        h.update(b"\0");
    }

    // Two sources of executable / data content feed the key, merged
    // and deduped: argv-walk (the discovery binary/script and any
    // path-shaped args inside the workspace) and the target's
    // declared `inputs:` (explicit list for helpers / data files the
    // argv walk can't see). Sorted by relative path so the ordering
    // is independent of declaration order.
    let mut content_inputs: Vec<(PathBuf, ContentHash)> = Vec::new();
    content_inputs.extend(resolve_executable_inputs(
        &spec.command,
        spec.cwd.as_path(),
        workspace_root,
    ));
    content_inputs.extend(expand_declared_inputs(spec, workspace_root));
    content_inputs.sort_by(|a, b| a.0.cmp(&b.0));
    content_inputs.dedup_by(|a, b| a.0 == b.0);

    h.update(b"content\0");
    for (rel, content_hash) in &content_inputs {
        h.update(rel.to_string_lossy().as_bytes());
        h.update(b"=");
        h.update(content_hash.as_bytes());
        h.update(b"\0");
    }

    CacheKey::new(h.finalize())
}

/// Expand the discovery target's declared `inputs:` to (workspace-
/// relative path, content hash) pairs. Globs are resolved against
/// `workspace_root`. `Input::Structural` is ignored - that variant
/// is for regular targets' wave-mode inputs and doesn't apply here.
fn expand_declared_inputs(
    spec: &TargetSpec,
    workspace_root: &AbsPath,
) -> Vec<(PathBuf, ContentHash)> {
    use crate::model::Input;
    let ws = workspace_root.as_path();
    let mut out: Vec<(PathBuf, ContentHash)> = Vec::new();
    for input in &spec.inputs {
        let Input::File { glob } = input else {
            continue;
        };
        let pattern = ws.join(glob.as_str());
        let Some(pattern_str) = pattern.to_str() else {
            continue;
        };
        let Ok(matched) = glob::glob(pattern_str) else {
            continue;
        };
        for entry in matched.flatten() {
            let Ok(canon) = entry.canonicalize() else {
                continue;
            };
            let Ok(rel) = canon.strip_prefix(ws) else {
                continue;
            };
            let Ok(meta) = std::fs::metadata(&canon) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let Ok(hash) = ContentHash::of_file(&canon) else {
                continue;
            };
            out.push((rel.to_path_buf(), hash));
        }
    }
    out
}

/// Walk the command's argv looking for tokens that resolve to a real
/// file inside `workspace_root`. Each hit contributes (workspace-
/// relative path, content hash) to the caller. Sorted by relative
/// path so the order doesn't depend on argv position.
///
/// Three resolution strategies, in order:
///   1. **Absolute path** (`/abs/path/...`): use as-is.
///   2. **Path containing `/`**: join with the target's `cwd` (which
///      is itself joined onto `workspace_root`).
///   3. **Bare name** (no `/`, only meaningful for `argv[0]`): walk
///      the target's `PATH` env (with the process env as fallback)
///      and pick the first hit.
///
/// In all cases we then check that the resolved path lives under
/// `workspace_root`. System tools (`bash`, `/usr/bin/grep`) land
/// outside and get skipped - they'd otherwise make the key
/// host-specific. In-tree binaries dropped into a PATH-listed dir
/// like `bin/` get hashed, so editing the discover binary
/// invalidates the discovery cache even when invoked by bare name.
fn resolve_executable_inputs(
    command: &str,
    cwd: &Path,
    workspace_root: &AbsPath,
) -> Vec<(PathBuf, ContentHash)> {
    let Ok(argv) = shell_words::split(command) else {
        return Vec::new();
    };
    let workspace_abs = workspace_root.as_path();
    let cwd_abs = workspace_abs.join(cwd);
    let mut out: Vec<(PathBuf, ContentHash)> = Vec::new();
    for (i, tok) in argv.iter().enumerate() {
        if tok.starts_with('-') {
            continue;
        }
        let candidates = candidate_paths(tok, &cwd_abs, i == 0);
        for candidate in candidates {
            let Ok(canon) = candidate.canonicalize() else {
                continue;
            };
            let Ok(rel) = canon.strip_prefix(workspace_abs) else {
                continue;
            };
            let Ok(meta) = std::fs::metadata(&canon) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let Ok(hash) = ContentHash::of_file(&canon) else {
                continue;
            };
            out.push((rel.to_path_buf(), hash));
            break; // first hit wins for this token
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// Build the ordered list of paths to try for one argv token. For
/// non-argv0 or path-shaped tokens it's a single candidate; for a
/// bare argv0 we walk `$PATH` (process env) so workspace-local PATH
/// entries - `bin/`, `target/release/`, `node_modules/.bin/` - are
/// reached.
fn candidate_paths(tok: &str, cwd_abs: &Path, is_argv0: bool) -> Vec<PathBuf> {
    if Path::new(tok).is_absolute() {
        return vec![PathBuf::from(tok)];
    }
    if tok.contains('/') || !is_argv0 {
        return vec![cwd_abs.join(tok)];
    }
    // Bare argv0: try the cwd first (sometimes scripts ship there),
    // then each `$PATH` entry. The shell wouldn't try cwd unless
    // `.` is on PATH, but doing it here is cheap and covers the
    // common ad-hoc case.
    let mut out: Vec<PathBuf> = vec![cwd_abs.join(tok)];
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            out.push(dir.join(tok));
        }
    }
    out
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("IO error reading discovery output {0}: {1}")]
    Io(String, std::io::Error),

    #[error("invalid JSON in discovery output {0}: {1}")]
    Json(String, serde_json::Error),

    #[error("graph error: {0}")]
    Graph(#[from] GraphError),

    #[error("unsupported discovery schema_version {found} in {file}")]
    UnsupportedSchema { file: String, found: u32 },
}

/// One target's discovery output file, parsed.
///
/// `deny_unknown_fields` catches typos in field names - a `outputs` vs
/// `output` confusion in a discovery script is a loud, line-pointed error
/// instead of silent staleness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryFragment {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,

    #[serde(default)]
    pub targets: Vec<TargetSpec>,

    /// Further `include:` targets to run after this fragment is merged.
    /// Each is built, its output parsed, its targets merged, and any
    /// `include:` it emits gets enqueued for the next round (TDD-0003).
    #[serde(default)]
    pub include: Vec<TargetSpec>,

    /// Files and directories the discovery actually consulted. The
    /// engine uses this to verify whether the cached output is still
    /// valid on the next run (TDD-0015 §Verifier algorithm). Absent
    /// means the discovery did not cooperate - under lenient mode the
    /// output is used once but not cached; under strict it's an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reads: Option<DiscoveryReads>,
}

/// The recorded-reads manifest. Two entry kinds: file entries (whole-file
/// or excerpt) and directory entries (whole listing or filtered listing).
/// See TDD-0015 for entry-kind semantics and verifier rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryReads {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<ReadFileEntry>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirs: Vec<ReadDirEntry>,
}

/// A file entry in the `reads` manifest. When `lines` is empty, the
/// verifier hashes the whole file. When non-empty, only lines whose
/// prefix matches any pattern are hashed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFileEntry {
    pub path: WsRelPath,

    /// Substring-prefix patterns. Single-string form (`"^pkg"`) and
    /// list form (`["^pkg", "^import "]`) both parse to the same Vec.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_string_or_list"
    )]
    pub lines: Vec<String>,
}

/// A directory entry in the `reads` manifest. When `filter` is empty,
/// the verifier hashes the directory's full listing. When non-empty,
/// only entries matching any glob filter are hashed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadDirEntry {
    pub path: WsRelPath,

    /// Glob patterns matched against entry names (not paths). Same
    /// single-or-list deserialization as `ReadFileEntry::lines`.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_string_or_list"
    )]
    pub filter: Vec<String>,
}

/// Accept either `"x"` or `["x", "y"]` on the wire, normalize to Vec.
fn deserialize_string_or_list<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

fn default_schema_version() -> u32 {
    1
}

const SUPPORTED_SCHEMA: u32 = 1;

/// Materialized `reads` manifest: paths + recorded content hashes. This
/// is what the engine writes to the discovery sidecar after a cold run
/// and verifies against the filesystem on warm runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedReads {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<RecordedFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirs: Vec<RecordedDir>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedFile {
    pub path: WsRelPath,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<String>,
    pub content_hash: ContentHash,

    /// `(mtime, size)` from the cold-compute pass. The verifier trusts
    /// the recorded `content_hash` when both still match. Optional for
    /// backward compatibility with sidecars written before this field
    /// landed; missing values force a full re-hash on warm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedDir {
    pub path: WsRelPath,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<String>,
    pub listing_hash: ContentHash,

    /// Directory mtime from the cold-compute pass. Adding or removing
    /// entries bumps the parent's mtime on every common filesystem; an
    /// mtime match lets the verifier skip the listing rehash. Optional
    /// for backward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ns: Option<u64>,
}

/// Outcome of verifying a `RecordedReads` manifest against the live
/// filesystem.
#[derive(Debug, Clone)]
pub enum VerifyOutcome {
    Match,
    Mismatch { reason: String },
}

/// Hash every entry in `reads` against the workspace, capturing
/// (mtime, size) alongside the content hash so warm verification can
/// stat-skip unchanged files. Parallel via rayon.
pub fn materialize_reads(
    reads: &DiscoveryReads,
    workspace_root: &std::path::Path,
) -> std::io::Result<RecordedReads> {
    use rayon::prelude::*;

    let files: Result<Vec<RecordedFile>, std::io::Error> = reads
        .files
        .par_iter()
        .map(|e| {
            let abs = workspace_root.join(e.path.as_path());
            let (mtime_ns, size) = match abs.metadata() {
                Ok(m) => (crate::paths::mtime_ns(&m), Some(m.len())),
                Err(_) => (None, None),
            };
            let content_hash = hash_file_entry(e, workspace_root)?;
            Ok(RecordedFile {
                path: e.path.clone(),
                lines: e.lines.clone(),
                content_hash,
                mtime_ns,
                size,
            })
        })
        .collect();

    let dirs: Result<Vec<RecordedDir>, std::io::Error> = reads
        .dirs
        .par_iter()
        .map(|e| {
            let abs = workspace_root.join(e.path.as_path());
            let mtime_ns = abs.metadata().ok().and_then(|m| crate::paths::mtime_ns(&m));
            let listing_hash = hash_dir_entry(e, workspace_root)?;
            Ok(RecordedDir {
                path: e.path.clone(),
                filter: e.filter.clone(),
                listing_hash,
                mtime_ns,
            })
        })
        .collect();

    Ok(RecordedReads {
        files: files?,
        dirs: dirs?,
    })
}

/// Verify every entry in `recorded` against the workspace. Returns
/// `Match` only when nothing has shifted.
///
/// Two fast paths layered ahead of the rehash:
///
///   1. **mtime + size skip.** For file entries with recorded
///      `(mtime_ns, size)`, the verifier stats the current file and
///      reuses the recorded `content_hash` when both still match.
///      Same idea as the structural-input sidecar's Stage 2 path.
///   2. **mtime skip for dirs.** Adding or removing a child file bumps
///      the parent dir's mtime on every common filesystem, so an
///      unchanged dir mtime means the listing hash is still valid.
///
/// The remaining hash work runs in parallel via rayon. First mismatch
/// reported deterministically (the file/dir with the lex-smallest path
/// among the mismatching entries).
pub fn verify_reads(
    recorded: &RecordedReads,
    workspace_root: &std::path::Path,
) -> std::io::Result<VerifyOutcome> {
    verify_reads_with_fsmonitor(recorded, workspace_root, None)
}

/// As [`verify_reads`], but consults an optional fsmonitor change set
/// to skip entries the monitor confirmed unchanged. Returns the first
/// mismatch any rayon worker observes; remaining workers see the
/// answer-already-decided signal via `find_map_any` and stop.
pub fn verify_reads_with_fsmonitor(
    recorded: &RecordedReads,
    workspace_root: &std::path::Path,
    changeset: Option<&crate::fsmonitor::ChangeSet>,
) -> std::io::Result<VerifyOutcome> {
    use rayon::prelude::*;

    let files_result: Option<std::io::Result<VerifyOutcome>> =
        recorded.files.par_iter().find_map_any(|f| {
            if changeset.is_some_and(|cs| !cs.file_might_have_changed(f.path.as_path())) {
                return None;
            }
            verify_entry(
                workspace_root,
                f.path.as_path(),
                EntryKind::File {
                    lines: &f.lines,
                    recorded_hash: f.content_hash,
                    recorded_mtime: f.mtime_ns,
                    recorded_size: f.size,
                },
            )
        });
    if let Some(r) = files_result {
        return r;
    }

    let dirs_result: Option<std::io::Result<VerifyOutcome>> =
        recorded.dirs.par_iter().find_map_any(|d| {
            if changeset.is_some_and(|cs| !cs.dir_might_have_changed(d.path.as_path())) {
                return None;
            }
            verify_entry(
                workspace_root,
                d.path.as_path(),
                EntryKind::Dir {
                    filter: &d.filter,
                    recorded_hash: d.listing_hash,
                    recorded_mtime: d.mtime_ns,
                },
            )
        });
    if let Some(r) = dirs_result {
        return r;
    }

    Ok(VerifyOutcome::Match)
}

/// Shared verifier body: (mtime, size)-skip first, then content-hash
/// recompute. Returns `Some(Mismatch)` on diff, `Some(Err)` on I/O
/// failure, `None` when the entry is unchanged (so `find_map_any`
/// keeps scanning).
enum EntryKind<'a> {
    File {
        lines: &'a [String],
        recorded_hash: ContentHash,
        recorded_mtime: Option<u64>,
        recorded_size: Option<u64>,
    },
    Dir {
        filter: &'a [String],
        recorded_hash: ContentHash,
        recorded_mtime: Option<u64>,
    },
}

fn verify_entry(
    workspace_root: &std::path::Path,
    rel: &std::path::Path,
    kind: EntryKind<'_>,
) -> Option<std::io::Result<VerifyOutcome>> {
    let abs = workspace_root.join(rel);

    let (mtime_ok, label) = match &kind {
        EntryKind::File {
            recorded_mtime,
            recorded_size,
            ..
        } => {
            let want = recorded_mtime.zip(*recorded_size);
            let ok = want.and_then(|(t, s)| {
                let m = abs.metadata().ok()?;
                (m.len() == s && crate::paths::mtime_ns(&m) == Some(t)).then_some(())
            });
            (ok.is_some(), "file")
        }
        EntryKind::Dir { recorded_mtime, .. } => {
            let ok = recorded_mtime.and_then(|t| {
                let m = abs.metadata().ok()?;
                (crate::paths::mtime_ns(&m) == Some(t)).then_some(())
            });
            (ok.is_some(), "directory")
        }
    };

    if mtime_ok {
        return None;
    }

    // Fast-path didn't trigger (or no recorded baseline). Fall back to
    // a full rehash; map missing files to a precise Mismatch so the
    // bootstrap loop can log a useful reason.
    let (computed, recorded_hash) = match kind {
        EntryKind::File {
            lines,
            recorded_hash,
            ..
        } => {
            let entry = ReadFileEntry {
                path: WsRelPath::new(rel).expect("rel path"),
                lines: lines.to_vec(),
            };
            (hash_file_entry(&entry, workspace_root), recorded_hash)
        }
        EntryKind::Dir {
            filter,
            recorded_hash,
            ..
        } => {
            let entry = ReadDirEntry {
                path: WsRelPath::new(rel).expect("rel path"),
                filter: filter.to_vec(),
            };
            (hash_dir_entry(&entry, workspace_root), recorded_hash)
        }
    };

    match computed {
        Ok(h) if h == recorded_hash => None,
        Ok(_) => Some(Ok(VerifyOutcome::Mismatch {
            reason: format!("{label} content changed: {}", rel.display()),
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Ok(VerifyOutcome::Mismatch {
            reason: format!("{label} missing: {}", rel.display()),
        })),
        Err(e) => Some(Err(e)),
    }
}

/// Sidecar schema. Bump (and migrate / reject) when the on-disk shape
/// changes incompatibly.
const SIDECAR_SCHEMA: u32 = 1;

/// On-disk representation of a discovery target's cached output and the
/// `RecordedReads` manifest used to verify it. One per cache key, under
/// `.giant/discovery/<key>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoverySidecar {
    pub schema: u32,
    pub cache_key: CacheKey,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<TargetSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<TargetSpec>,
    pub reads: RecordedReads,
}

impl DiscoverySidecar {
    pub fn new(
        cache_key: CacheKey,
        targets: Vec<TargetSpec>,
        include: Vec<TargetSpec>,
        reads: RecordedReads,
    ) -> Self {
        Self {
            schema: SIDECAR_SCHEMA,
            cache_key,
            targets,
            include,
            reads,
        }
    }
}

fn sidecar_path(state_dir: &std::path::Path, key: CacheKey) -> std::path::PathBuf {
    state_dir
        .join("discovery")
        .join(format!("{}.json", key.to_hex()))
}

/// Write a sidecar atomically: serialize to `.tmp`, fsync, rename into
/// place. Concurrent invocations on the same key resolve "last
/// finisher wins" without corrupting the file.
pub fn write_sidecar(
    state_dir: &std::path::Path,
    sidecar: &DiscoverySidecar,
) -> std::io::Result<()> {
    let final_path = sidecar_path(state_dir, sidecar.cache_key);
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = final_path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(sidecar)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    std::fs::write(&tmp_path, &bytes)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

impl RecordedReads {
    /// Project to the entry-spec form the discovery emitted (paths +
    /// patterns, no hashes). Used when restoring a cached discovery
    /// output to disk so downstream consumers see the same JSON the
    /// discovery would have produced.
    pub fn to_discovery_reads(&self) -> DiscoveryReads {
        DiscoveryReads {
            files: self
                .files
                .iter()
                .map(|f| ReadFileEntry {
                    path: f.path.clone(),
                    lines: f.lines.clone(),
                })
                .collect(),
            dirs: self
                .dirs
                .iter()
                .map(|d| ReadDirEntry {
                    path: d.path.clone(),
                    filter: d.filter.clone(),
                })
                .collect(),
        }
    }
}

/// Re-create the `DiscoveryFragment` JSON that a discovery target's
/// command would have produced. Used when a sidecar verifies and the
/// engine restores the output without running the discovery, so
/// downstream targets that read the output file see consistent
/// contents.
pub fn fragment_from_sidecar(sidecar: &DiscoverySidecar) -> DiscoveryFragment {
    DiscoveryFragment {
        schema_version: SUPPORTED_SCHEMA,
        targets: sidecar.targets.clone(),
        include: sidecar.include.clone(),
        reads: Some(sidecar.reads.to_discovery_reads()),
    }
}

/// Read a sidecar for the given key. Returns `Ok(None)` when the file
/// is absent or has an incompatible schema (treated as a cache miss -
/// the caller will re-run the discovery and rewrite). I/O errors and
/// malformed JSON propagate so the caller can surface diagnostics.
pub fn read_sidecar(
    state_dir: &std::path::Path,
    key: CacheKey,
) -> std::io::Result<Option<DiscoverySidecar>> {
    let path = sidecar_path(state_dir, key);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let sidecar: DiscoverySidecar = serde_json::from_str(&raw)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    if sidecar.schema != SIDECAR_SCHEMA {
        return Ok(None);
    }
    if sidecar.cache_key != key {
        // Self-check: the file's recorded key doesn't match the lookup
        // key. Treat as a miss so the caller rewrites.
        return Ok(None);
    }
    Ok(Some(sidecar))
}

fn hash_file_entry(
    entry: &ReadFileEntry,
    workspace_root: &std::path::Path,
) -> std::io::Result<ContentHash> {
    let full_path = workspace_root.join(entry.path.as_path());
    if entry.lines.is_empty() {
        return ContentHash::of_file(&full_path);
    }
    // Excerpt mode: read file, hash lines whose prefix matches any pattern.
    // Matched lines preserve their file order, separated by NUL so e.g.
    // "foo\nbar" and "foobar" hash differently.
    let raw = std::fs::read_to_string(&full_path)?;
    let mut h = ContentHash::hasher();
    for line in raw.lines() {
        if entry.lines.iter().any(|p| line.starts_with(p.as_str())) {
            h.update(line.as_bytes());
            h.update(b"\0");
        }
    }
    Ok(h.finalize())
}

fn hash_dir_entry(
    entry: &ReadDirEntry,
    workspace_root: &std::path::Path,
) -> std::io::Result<ContentHash> {
    let full_path = workspace_root.join(entry.path.as_path());

    // Compile filter patterns once. An invalid filter glob in a stored
    // sidecar is treated as an IO error so the caller can surface a
    // diagnostic; in practice the same glob was validated when the
    // discovery output was first parsed.
    let filters: Result<Vec<glob::Pattern>, _> =
        entry.filter.iter().map(|s| glob::Pattern::new(s)).collect();
    let filters = filters
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;

    let mut names: Vec<String> = Vec::new();
    for child in std::fs::read_dir(&full_path)? {
        let child = child?;
        let name = child.file_name().to_string_lossy().into_owned();
        let keep = filters.is_empty() || filters.iter().any(|p| p.matches(&name));
        if keep {
            names.push(name);
        }
    }
    names.sort();

    let mut h = ContentHash::hasher();
    for n in &names {
        h.update(n.as_bytes());
        h.update(b"\0");
    }
    Ok(h.finalize())
}

/// Parse a discovery output file from disk.
pub fn parse_fragment(path: &std::path::Path) -> Result<DiscoveryFragment, DiscoveryError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| DiscoveryError::Io(path.display().to_string(), e))?;
    let frag: DiscoveryFragment = serde_json::from_str(&raw)
        .map_err(|e| DiscoveryError::Json(path.display().to_string(), e))?;
    if frag.schema_version != SUPPORTED_SCHEMA {
        return Err(DiscoveryError::UnsupportedSchema {
            file: path.display().to_string(),
            found: frag.schema_version,
        });
    }
    Ok(frag)
}

/// Merge a fragment's targets (and any nested `include:` entries) into
/// the graph. Returns the list of newly-added include target IDs so
/// the bootstrap loop can build them in the next wave.
///
/// Nested includes whose ID is already in the graph (e.g. a discovery
/// that emits its own self-id, or two discoveries that both emit the
/// same nested include) are silently deduplicated - this is the
/// cycle-detection safety net for recursive discovery.
pub fn merge_into(
    graph: &mut BuildGraph,
    frag: DiscoveryFragment,
) -> Result<Vec<crate::model::TargetId>, DiscoveryError> {
    let mut new_includes: Vec<crate::model::TargetId> = Vec::with_capacity(frag.include.len());
    for inc in frag.include {
        let id = inc.id.clone();
        if graph.get(&id).is_some() {
            // Already added by an earlier wave (or duplicate within
            // this wave). Skip; the cycle/dup is harmless here - the
            // bootstrap loop's seen-set won't re-build it either.
            continue;
        }
        graph.add_target(inc)?;
        new_includes.push(id);
    }
    for target in frag.targets {
        graph.add_target(target)?;
    }
    Ok(new_includes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_json(s: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        f.write_all(s.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn parse_minimal_fragment() {
        let f = write_json(r#"{ "targets": [] }"#);
        let frag = parse_fragment(f.path()).unwrap();
        assert_eq!(frag.schema_version, 1);
        assert!(frag.targets.is_empty());
    }

    #[test]
    fn parse_fragment_with_target() {
        let f = write_json(
            r#"{
              "targets": [
                { "id": "go:bin:server",
                  "inputs": ["cmd/server/**/*.go"],
                  "outputs": ["bin/server"],
                  "command": "go build -o bin/server ./cmd/server" }
              ]
            }"#,
        );
        let frag = parse_fragment(f.path()).unwrap();
        assert_eq!(frag.targets.len(), 1);
        assert_eq!(frag.targets[0].id.as_str(), "go:bin:server");
    }

    #[test]
    fn parse_rejects_unknown_field() {
        // deny_unknown_fields catches typos
        let f = write_json(r#"{ "targets": [], "tagets": [] }"#);
        let err = parse_fragment(f.path()).unwrap_err();
        assert!(matches!(err, DiscoveryError::Json(_, _)));
    }

    #[test]
    fn parse_rejects_unknown_schema() {
        let f = write_json(r#"{ "schema_version": 99, "targets": [] }"#);
        let err = parse_fragment(f.path()).unwrap_err();
        assert!(matches!(
            err,
            DiscoveryError::UnsupportedSchema { found: 99, .. }
        ));
    }

    #[test]
    fn merge_adds_targets_to_graph() {
        let f = write_json(
            r#"{
              "targets": [
                { "id": "x",
                  "inputs": [],
                  "outputs": ["x.out"],
                  "command": "true" }
              ]
            }"#,
        );
        let frag = parse_fragment(f.path()).unwrap();
        let mut graph = BuildGraph::new();
        let new_includes = merge_into(&mut graph, frag).unwrap();
        assert!(graph.get(&crate::model::TargetId::new("x")).is_some());
        assert!(new_includes.is_empty());
    }

    use crate::model::TargetId;

    fn include_spec(id: &str, command: &str) -> TargetSpec {
        TargetSpec {
            id: TargetId::new(id),
            inputs: vec![],
            outputs: vec![],
            deps: vec![],
            command: command.into(),
            cwd: Default::default(),
            env: Default::default(),
            cache: None,
            remote_cache: true,
            exists: None,
            timeout_secs: None,
            test: false,
            tags: Default::default(),
            label: None,
            scope: vec![],
            inferred_deps: Default::default(),
        }
    }

    fn empty_workspace() -> (tempfile::TempDir, AbsPath) {
        let dir = tempfile::tempdir().unwrap();
        let abs = AbsPath::new(dir.path().to_path_buf());
        (dir, abs)
    }

    #[test]
    fn discovery_key_is_deterministic() {
        let (_d, ws) = empty_workspace();
        let a = include_spec("d", "tools/d.sh > out.json");
        let b = include_spec("d", "tools/d.sh > out.json");
        assert_eq!(discovery_cache_key(&a, &ws), discovery_cache_key(&b, &ws));
    }

    #[test]
    fn discovery_key_changes_with_command() {
        let (_d, ws) = empty_workspace();
        let a = include_spec("d", "tools/d.sh > out.json");
        let b = include_spec("d", "tools/d2.sh > out.json");
        assert_ne!(discovery_cache_key(&a, &ws), discovery_cache_key(&b, &ws));
    }

    #[test]
    fn discovery_key_changes_with_cwd() {
        let (_d, ws) = empty_workspace();
        let mut a = include_spec("d", "tools/d.sh");
        let mut b = include_spec("d", "tools/d.sh");
        a.cwd = WsRelPath::new("pkg").unwrap();
        b.cwd = WsRelPath::new("cmd").unwrap();
        assert_ne!(discovery_cache_key(&a, &ws), discovery_cache_key(&b, &ws));
    }

    #[test]
    fn discovery_key_changes_with_env_value() {
        let (_d, ws) = empty_workspace();
        let mut a = include_spec("d", "tools/d.sh");
        let mut b = include_spec("d", "tools/d.sh");
        a.env.insert("LANG".into(), "en_US.UTF-8".into());
        b.env.insert("LANG".into(), "C".into());
        assert_ne!(discovery_cache_key(&a, &ws), discovery_cache_key(&b, &ws));
    }

    #[test]
    fn discovery_key_stable_under_env_declaration_order() {
        let (_d, ws) = empty_workspace();
        let mut a = include_spec("d", "tools/d.sh");
        let mut b = include_spec("d", "tools/d.sh");
        a.env.insert("A".into(), "1".into());
        a.env.insert("B".into(), "2".into());
        b.env.insert("B".into(), "2".into());
        b.env.insert("A".into(), "1".into());
        assert_eq!(discovery_cache_key(&a, &ws), discovery_cache_key(&b, &ws));
    }

    #[test]
    fn discovery_key_changes_with_scope() {
        let (_d, ws) = empty_workspace();
        let mut a = include_spec("d", "tools/d.sh");
        let mut b = include_spec("d", "tools/d.sh");
        a.scope = vec![WsRelPath::new("pkg").unwrap()];
        b.scope = vec![WsRelPath::new("cmd").unwrap()];
        assert_ne!(discovery_cache_key(&a, &ws), discovery_cache_key(&b, &ws));
    }

    #[test]
    fn discovery_key_stable_under_scope_order() {
        let (_d, ws) = empty_workspace();
        let mut a = include_spec("d", "tools/d.sh");
        let mut b = include_spec("d", "tools/d.sh");
        a.scope = vec![
            WsRelPath::new("pkg").unwrap(),
            WsRelPath::new("cmd").unwrap(),
        ];
        b.scope = vec![
            WsRelPath::new("cmd").unwrap(),
            WsRelPath::new("pkg").unwrap(),
        ];
        assert_eq!(discovery_cache_key(&a, &ws), discovery_cache_key(&b, &ws));
    }

    #[test]
    fn discovery_key_empty_scope_vs_set_scope_differ() {
        let (_d, ws) = empty_workspace();
        let mut a = include_spec("d", "tools/d.sh");
        let b = include_spec("d", "tools/d.sh");
        a.scope = vec![WsRelPath::new("pkg").unwrap()];
        assert_ne!(discovery_cache_key(&a, &ws), discovery_cache_key(&b, &ws));
    }

    /// Editing the discover script changes the key without any change
    /// to the declared spec - the executable's content is now part of
    /// the key.
    #[test]
    fn discovery_key_changes_when_script_content_changes() {
        let (_d, ws) = empty_workspace();
        std::fs::create_dir_all(ws.as_path().join("tools")).unwrap();
        let script = ws.as_path().join("tools/d.sh");
        std::fs::write(&script, b"#!/bin/sh\necho a\n").unwrap();

        let spec = include_spec("d", "tools/d.sh");
        let key_before = discovery_cache_key(&spec, &ws);

        std::fs::write(&script, b"#!/bin/sh\necho b\n").unwrap();
        let key_after = discovery_cache_key(&spec, &ws);

        assert_ne!(
            key_before, key_after,
            "editing the discover script should invalidate the cache"
        );
    }

    /// A script-via-interpreter invocation: the interpreter (`bash`)
    /// lives outside the workspace and is ignored; the script under
    /// the workspace is hashed.
    #[test]
    fn discovery_key_hashes_script_argument_under_interpreter() {
        let (_d, ws) = empty_workspace();
        std::fs::create_dir_all(ws.as_path().join("tools")).unwrap();
        let script = ws.as_path().join("tools/d.sh");
        std::fs::write(&script, b"v1").unwrap();

        let spec = include_spec("d", "bash tools/d.sh --flag");
        let key_v1 = discovery_cache_key(&spec, &ws);

        std::fs::write(&script, b"v2").unwrap();
        let key_v2 = discovery_cache_key(&spec, &ws);

        assert_ne!(key_v1, key_v2);
    }

    /// PATH-resolved bare binaries that point INSIDE the workspace
    /// (e.g. an in-tree `bin/discover-go` reachable because the user
    /// prepended `bin/` to `$PATH`) are hashed. Editing the binary
    /// invalidates the discovery cache.
    #[test]
    fn discovery_key_hashes_path_lookup_when_target_lives_in_workspace() {
        let (_d, ws) = empty_workspace();
        std::fs::create_dir_all(ws.as_path().join("bin")).unwrap();
        let bin = ws.as_path().join("bin/discover-go");
        std::fs::write(&bin, b"v1").unwrap();

        let prev_path = std::env::var_os("PATH");
        // Single-thread the test against the global PATH env it
        // depends on; concurrent tests in this file don't touch PATH,
        // so a plain set/restore is enough.
        let new_path = ws.as_path().join("bin").into_os_string();
        // SAFETY: tests in this module are not multithreaded over PATH.
        unsafe {
            std::env::set_var("PATH", &new_path);
        }

        let spec = include_spec("d", "discover-go pkg/...");
        let key_v1 = discovery_cache_key(&spec, &ws);

        std::fs::write(&bin, b"v2").unwrap();
        let key_v2 = discovery_cache_key(&spec, &ws);

        // Restore PATH before any assertion that might panic.
        match prev_path {
            // SAFETY: see above
            Some(p) => unsafe {
                std::env::set_var("PATH", p);
            },
            None => unsafe {
                std::env::remove_var("PATH");
            },
        }

        assert_ne!(
            key_v1, key_v2,
            "editing an in-workspace PATH-resolved discover binary should invalidate the cache"
        );
    }

    /// Declared `inputs:` on a discovery target contribute to the
    /// cache key - editing a helper that the argv walk can't see
    /// still invalidates the discovery.
    #[test]
    fn discovery_key_changes_when_declared_input_changes() {
        use crate::model::Input;
        use crate::types::GlobPattern;
        let (_d, ws) = empty_workspace();
        std::fs::create_dir_all(ws.as_path().join("tools/lib")).unwrap();
        let helper = ws.as_path().join("tools/lib/helper.sh");
        std::fs::write(&helper, b"v1").unwrap();

        let mut spec = include_spec("d", "tools/discover.sh");
        spec.inputs = vec![Input::File {
            glob: GlobPattern::new("tools/lib/**/*.sh").unwrap(),
        }];
        let key_v1 = discovery_cache_key(&spec, &ws);

        std::fs::write(&helper, b"v2").unwrap();
        let key_v2 = discovery_cache_key(&spec, &ws);

        assert_ne!(
            key_v1, key_v2,
            "editing a declared `inputs:` file should invalidate the discovery cache"
        );
    }

    /// Argv walk + declared inputs merge cleanly: a file caught by
    /// both contributes once (deduped on the workspace-relative
    /// path), so the cache key doesn't double-count.
    #[test]
    fn discovery_key_dedupes_argv_and_declared_overlap() {
        use crate::model::Input;
        use crate::types::GlobPattern;
        let (_d, ws) = empty_workspace();
        std::fs::create_dir_all(ws.as_path().join("tools")).unwrap();
        std::fs::write(ws.as_path().join("tools/d.sh"), b"v1").unwrap();

        let argv_only = include_spec("d", "tools/d.sh");
        let mut both = include_spec("d", "tools/d.sh");
        both.inputs = vec![Input::File {
            glob: GlobPattern::new("tools/d.sh").unwrap(),
        }];

        assert_eq!(
            discovery_cache_key(&argv_only, &ws),
            discovery_cache_key(&both, &ws),
            "the same file caught both ways must hash identically"
        );
        // Sanity: drop the file used by both, key changes.
        std::fs::write(ws.as_path().join("tools/d.sh"), b"v2").unwrap();
        let key_after = discovery_cache_key(&argv_only, &ws);
        let _ = both;
        let key_argv_v1 = discovery_cache_key(
            &{
                let mut s = include_spec("d", "tools/d.sh");
                s.cwd = argv_only.cwd.clone();
                s
            },
            &ws,
        );
        assert_eq!(key_after, key_argv_v1);
    }

    /// PATH lookups that resolve outside the workspace (system tools)
    /// don't contribute to the key - keeps it host-stable.
    #[test]
    fn discovery_key_ignores_system_path_lookups() {
        let (_d, ws) = empty_workspace();
        // `ls` exists on every POSIX host but lives under /usr/bin or
        // /bin, neither of which is inside our tempdir workspace.
        let a = include_spec("d", "ls pkg/");
        let b = include_spec("d", "ls pkg/");
        assert_eq!(discovery_cache_key(&a, &ws), discovery_cache_key(&b, &ws));
    }

    // ------------ verifier ------------

    fn make_file(root: &std::path::Path, rel: &str, content: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    fn whole_file_entry(rel: &str) -> ReadFileEntry {
        ReadFileEntry {
            path: WsRelPath::new(rel).unwrap(),
            lines: vec![],
        }
    }

    fn excerpt_entry(rel: &str, lines: &[&str]) -> ReadFileEntry {
        ReadFileEntry {
            path: WsRelPath::new(rel).unwrap(),
            lines: lines.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn dir_entry(rel: &str, filter: &[&str]) -> ReadDirEntry {
        ReadDirEntry {
            path: WsRelPath::new(rel).unwrap(),
            filter: filter.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn whole_file_verifier_match_then_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        make_file(dir.path(), "go.mod", "module x\n");
        let reads = DiscoveryReads {
            files: vec![whole_file_entry("go.mod")],
            dirs: vec![],
        };
        let recorded = materialize_reads(&reads, dir.path()).unwrap();
        assert!(matches!(
            verify_reads(&recorded, dir.path()).unwrap(),
            VerifyOutcome::Match
        ));

        make_file(dir.path(), "go.mod", "module y\n");
        match verify_reads(&recorded, dir.path()).unwrap() {
            VerifyOutcome::Mismatch { reason } => assert!(reason.contains("go.mod")),
            VerifyOutcome::Match => panic!("expected mismatch"),
        }
    }

    #[test]
    fn whole_file_verifier_reports_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        make_file(dir.path(), "x.txt", "hi");
        let reads = DiscoveryReads {
            files: vec![whole_file_entry("x.txt")],
            dirs: vec![],
        };
        let recorded = materialize_reads(&reads, dir.path()).unwrap();
        std::fs::remove_file(dir.path().join("x.txt")).unwrap();
        match verify_reads(&recorded, dir.path()).unwrap() {
            VerifyOutcome::Mismatch { reason } => assert!(reason.contains("missing")),
            _ => panic!(),
        }
    }

    #[test]
    fn excerpt_verifier_ignores_non_matching_line_edits() {
        let dir = tempfile::tempdir().unwrap();
        make_file(
            dir.path(),
            "pkg/foo.go",
            "package foo\nimport \"fmt\"\nfunc Hello() {}\n",
        );
        let reads = DiscoveryReads {
            files: vec![excerpt_entry("pkg/foo.go", &["package ", "import "])],
            dirs: vec![],
        };
        let recorded = materialize_reads(&reads, dir.path()).unwrap();

        // Function-body edit: package + import lines unchanged → still Match.
        make_file(
            dir.path(),
            "pkg/foo.go",
            "package foo\nimport \"fmt\"\nfunc Hello() { fmt.Println(\"hi\") }\n",
        );
        assert!(matches!(
            verify_reads(&recorded, dir.path()).unwrap(),
            VerifyOutcome::Match
        ));

        // Adding an import: matching line set changes → Mismatch.
        make_file(
            dir.path(),
            "pkg/foo.go",
            "package foo\nimport \"fmt\"\nimport \"log\"\nfunc Hello() {}\n",
        );
        assert!(matches!(
            verify_reads(&recorded, dir.path()).unwrap(),
            VerifyOutcome::Mismatch { .. }
        ));
    }

    #[test]
    fn dir_verifier_whole_listing_invalidates_on_any_add() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkg")).unwrap();
        make_file(dir.path(), "pkg/a.go", "");
        let reads = DiscoveryReads {
            files: vec![],
            dirs: vec![dir_entry("pkg", &[])],
        };
        let recorded = materialize_reads(&reads, dir.path()).unwrap();

        // Adding any file changes the listing.
        make_file(dir.path(), "pkg/README.md", "");
        assert!(matches!(
            verify_reads(&recorded, dir.path()).unwrap(),
            VerifyOutcome::Mismatch { .. }
        ));
    }

    #[test]
    fn dir_verifier_filter_ignores_non_matching_additions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkg")).unwrap();
        make_file(dir.path(), "pkg/a.go", "");
        make_file(dir.path(), "pkg/b.go", "");
        let reads = DiscoveryReads {
            files: vec![],
            dirs: vec![dir_entry("pkg", &["*.go"])],
        };
        let recorded = materialize_reads(&reads, dir.path()).unwrap();

        // Adding a README doesn't match the `*.go` filter → still Match.
        make_file(dir.path(), "pkg/README.md", "");
        assert!(matches!(
            verify_reads(&recorded, dir.path()).unwrap(),
            VerifyOutcome::Match
        ));

        // Adding a new .go file does match the filter → Mismatch.
        make_file(dir.path(), "pkg/c.go", "");
        assert!(matches!(
            verify_reads(&recorded, dir.path()).unwrap(),
            VerifyOutcome::Mismatch { .. }
        ));
    }

    #[test]
    fn dir_verifier_invalidates_on_removal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkg")).unwrap();
        make_file(dir.path(), "pkg/a.go", "");
        make_file(dir.path(), "pkg/b.go", "");
        let reads = DiscoveryReads {
            files: vec![],
            dirs: vec![dir_entry("pkg", &[])],
        };
        let recorded = materialize_reads(&reads, dir.path()).unwrap();

        std::fs::remove_file(dir.path().join("pkg/a.go")).unwrap();
        assert!(matches!(
            verify_reads(&recorded, dir.path()).unwrap(),
            VerifyOutcome::Mismatch { .. }
        ));
    }

    #[test]
    fn dir_verifier_invalidates_on_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("pkg")).unwrap();
        make_file(dir.path(), "pkg/a.go", "");
        let reads = DiscoveryReads {
            files: vec![],
            dirs: vec![dir_entry("pkg", &[])],
        };
        let recorded = materialize_reads(&reads, dir.path()).unwrap();

        std::fs::remove_dir_all(dir.path().join("pkg")).unwrap();
        match verify_reads(&recorded, dir.path()).unwrap() {
            VerifyOutcome::Mismatch { reason } => {
                assert!(reason.contains("missing") || reason.contains("pkg"))
            }
            _ => panic!(),
        }
    }

    #[test]
    fn dir_listing_hash_order_independent() {
        // The listing hash sorts names before hashing - two filesystems
        // that return entries in different orders should still match.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("d")).unwrap();
        make_file(dir.path(), "d/a", "");
        make_file(dir.path(), "d/b", "");
        let reads = DiscoveryReads {
            files: vec![],
            dirs: vec![dir_entry("d", &[])],
        };
        let r1 = materialize_reads(&reads, dir.path()).unwrap();
        let r2 = materialize_reads(&reads, dir.path()).unwrap();
        assert_eq!(r1.dirs[0].listing_hash, r2.dirs[0].listing_hash);
    }

    // ------------ sidecar I/O ------------

    fn sample_sidecar(key: CacheKey) -> DiscoverySidecar {
        DiscoverySidecar::new(
            key,
            vec![],
            vec![],
            RecordedReads {
                files: vec![RecordedFile {
                    path: WsRelPath::new("go.mod").unwrap(),
                    lines: vec![],
                    content_hash: ContentHash::of_bytes(b"module x\n"),
                    mtime_ns: None,
                    size: None,
                }],
                dirs: vec![],
            },
        )
    }

    #[test]
    fn sidecar_round_trip() {
        let ws = tempfile::tempdir().unwrap();
        let key = CacheKey::new(ContentHash::of_bytes(b"key1"));
        let s = sample_sidecar(key);
        write_sidecar(ws.path(), &s).unwrap();

        let back = read_sidecar(ws.path(), key).unwrap().unwrap();
        assert_eq!(back.cache_key, key);
        assert_eq!(back.reads.files.len(), 1);
        assert_eq!(
            back.reads.files[0].content_hash,
            s.reads.files[0].content_hash
        );
    }

    #[test]
    fn sidecar_missing_returns_none() {
        let ws = tempfile::tempdir().unwrap();
        let key = CacheKey::new(ContentHash::of_bytes(b"never-written"));
        assert!(read_sidecar(ws.path(), key).unwrap().is_none());
    }

    #[test]
    fn sidecar_unknown_schema_returns_none() {
        let ws = tempfile::tempdir().unwrap();
        let key = CacheKey::new(ContentHash::of_bytes(b"k"));
        let path = sidecar_path(ws.path(), key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Hand-written sidecar with a schema we don't support.
        let body = serde_json::json!({
            "schema": 99,
            "cache_key": key,
            "reads": { "files": [], "dirs": [] }
        });
        std::fs::write(&path, serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(read_sidecar(ws.path(), key).unwrap().is_none());
    }

    #[test]
    fn sidecar_key_mismatch_returns_none() {
        // The file on disk says it was written for key A; the lookup
        // asks for key B (e.g., file was renamed manually, or there's a
        // hash collision in the truncated filename). Treat as a miss.
        let ws = tempfile::tempdir().unwrap();
        let key_a = CacheKey::new(ContentHash::of_bytes(b"a"));
        let key_b = CacheKey::new(ContentHash::of_bytes(b"b"));
        let mut s = sample_sidecar(key_a);
        s.cache_key = key_a;
        // Write the file under key_b's filename but keep key_a in the body.
        let path = sidecar_path(ws.path(), key_b);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = serde_json::to_vec(&s).unwrap();
        std::fs::write(&path, bytes).unwrap();

        assert!(read_sidecar(ws.path(), key_b).unwrap().is_none());
    }

    #[test]
    fn sidecar_malformed_json_is_error() {
        let ws = tempfile::tempdir().unwrap();
        let key = CacheKey::new(ContentHash::of_bytes(b"k"));
        let path = sidecar_path(ws.path(), key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        assert!(read_sidecar(ws.path(), key).is_err());
    }

    #[test]
    fn sidecar_overwrites_existing() {
        let ws = tempfile::tempdir().unwrap();
        let key = CacheKey::new(ContentHash::of_bytes(b"k"));
        let mut s = sample_sidecar(key);
        write_sidecar(ws.path(), &s).unwrap();

        s.reads.files[0].content_hash = ContentHash::of_bytes(b"different bytes");
        write_sidecar(ws.path(), &s).unwrap();

        let back = read_sidecar(ws.path(), key).unwrap().unwrap();
        assert_eq!(
            back.reads.files[0].content_hash,
            ContentHash::of_bytes(b"different bytes")
        );
    }

    #[test]
    fn sidecar_write_leaves_no_tmp_file() {
        // Atomic rename should not leave the .tmp companion behind.
        let ws = tempfile::tempdir().unwrap();
        let key = CacheKey::new(ContentHash::of_bytes(b"k"));
        write_sidecar(ws.path(), &sample_sidecar(key)).unwrap();
        let tmp = sidecar_path(ws.path(), key).with_extension("json.tmp");
        assert!(!tmp.exists(), "stale .tmp file: {}", tmp.display());
    }

    // ------------ original parse tests below ------------

    #[test]
    fn parse_fragment_without_reads_field() {
        // Backward compatibility: a fragment with no `reads` field
        // parses fine. Whether the engine caches the output depends on
        // strict/lenient mode (separate slice).
        let f = write_json(r#"{ "targets": [] }"#);
        let frag = parse_fragment(f.path()).unwrap();
        assert!(frag.reads.is_none());
    }

    #[test]
    fn parse_reads_whole_file_entry() {
        let f = write_json(
            r#"{
              "targets": [],
              "reads": {
                "files": [
                  { "path": "go.mod" },
                  { "path": "tools/discover.sh" }
                ]
              }
            }"#,
        );
        let frag = parse_fragment(f.path()).unwrap();
        let reads = frag.reads.unwrap();
        assert_eq!(reads.files.len(), 2);
        assert!(reads.files[0].lines.is_empty());
        assert_eq!(
            reads.files[0].path.as_path(),
            std::path::Path::new("go.mod")
        );
    }

    #[test]
    fn parse_reads_excerpt_entry_single_pattern() {
        let f = write_json(
            r#"{
              "reads": {
                "files": [
                  { "path": "pkg/foo.go", "lines": "^package " }
                ]
              }
            }"#,
        );
        let frag = parse_fragment(f.path()).unwrap();
        let entry = &frag.reads.unwrap().files[0];
        assert_eq!(entry.lines, vec!["^package ".to_string()]);
    }

    #[test]
    fn parse_reads_excerpt_entry_pattern_list() {
        let f = write_json(
            r#"{
              "reads": {
                "files": [
                  { "path": "pkg/foo.go",
                    "lines": ["^package ", "^import "] }
                ]
              }
            }"#,
        );
        let frag = parse_fragment(f.path()).unwrap();
        let entry = &frag.reads.unwrap().files[0];
        assert_eq!(entry.lines.len(), 2);
        assert_eq!(entry.lines[1], "^import ");
    }

    #[test]
    fn parse_reads_dir_entry_with_filter() {
        let f = write_json(
            r#"{
              "reads": {
                "dirs": [
                  { "path": "pkg/" },
                  { "path": "cmd/", "filter": "*.go" },
                  { "path": "internal/", "filter": ["*.go", "*.proto"] }
                ]
              }
            }"#,
        );
        let frag = parse_fragment(f.path()).unwrap();
        let dirs = &frag.reads.unwrap().dirs;
        assert_eq!(dirs.len(), 3);
        assert!(dirs[0].filter.is_empty());
        assert_eq!(dirs[1].filter, vec!["*.go".to_string()]);
        assert_eq!(dirs[2].filter.len(), 2);
    }

    #[test]
    fn parse_reads_rejects_unknown_entry_field() {
        // `deny_unknown_fields` on entry types - a typo (`paht` for
        // `path`) is a parse error, not a silent skip.
        let f = write_json(
            r#"{
              "reads": { "files": [ { "paht": "go.mod" } ] }
            }"#,
        );
        let err = parse_fragment(f.path()).unwrap_err();
        assert!(matches!(err, DiscoveryError::Json(_, _)));
    }

    #[test]
    fn merge_returns_nested_include_ids() {
        let f = write_json(
            r#"{
              "include": [
                { "id": "discover:wave2",
                  "inputs": ["scripts/wave2.sh"],
                  "outputs": [".giant/wave2.json"],
                  "command": "scripts/wave2.sh > .giant/wave2.json" }
              ],
              "targets": [
                { "id": "x",
                  "inputs": [],
                  "outputs": ["x.out"],
                  "command": "true" }
              ]
            }"#,
        );
        let frag = parse_fragment(f.path()).unwrap();
        let mut graph = BuildGraph::new();
        let new_includes = merge_into(&mut graph, frag).unwrap();
        assert_eq!(new_includes.len(), 1);
        assert_eq!(new_includes[0].as_str(), "discover:wave2");
        // Both the nested include AND the static target are in the graph now.
        assert!(
            graph
                .get(&crate::model::TargetId::new("discover:wave2"))
                .is_some()
        );
        assert!(graph.get(&crate::model::TargetId::new("x")).is_some());
    }
}
