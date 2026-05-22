//! Discovery: build `include:` targets first, merge their JSON outputs
//! into the main graph.
//!
//! See TDD-0003 for the bootstrap-pass scheduling and merge rules,
//! TDD-0015 for the discovery output protocol including the `reads`
//! manifest used for cache invalidation.

use crate::graph::{BuildGraph, GraphError};
use crate::model::{CacheKey, ContentHash, TargetSpec};
use crate::paths::WsRelPath;
use serde::{Deserialize, Serialize};

const DISCOVERY_KEY_SCHEMA: &str = "disc-v1";
const GIANT_VERSION: &str = env!("CARGO_PKG_VERSION");
const TARGET_TRIPLE: &str = env!("GIANT_TARGET_TRIPLE");

/// Compute the cache key for a discovery target per ADR-0013:
/// `cmd + env + cwd + scope` (no file inputs, no dep keys). The returned
/// key is the lookup key for the discovery sidecar; whether the cached
/// output is *valid* still depends on verifying the `reads` manifest
/// against the live filesystem.
///
/// Stable across runs given the same command/env/cwd/scope and the same
/// giant binary; sensitive to engine version bumps (the schema marker
/// and `GIANT_VERSION` are mixed in).
pub fn discovery_cache_key(spec: &TargetSpec) -> CacheKey {
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

    CacheKey::new(h.finalize())
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedDir {
    pub path: WsRelPath,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<String>,
    pub listing_hash: ContentHash,
}

/// Outcome of verifying a `RecordedReads` manifest against the live
/// filesystem.
#[derive(Debug, Clone)]
pub enum VerifyOutcome {
    Match,
    Mismatch { reason: String },
}

/// Convert a discovery-emitted `reads` manifest into a `RecordedReads`
/// snapshot by hashing every entry against the workspace. Called once
/// after a cold discovery run, before writing the sidecar.
pub fn materialize_reads(
    reads: &DiscoveryReads,
    workspace_root: &std::path::Path,
) -> std::io::Result<RecordedReads> {
    let files = reads
        .files
        .iter()
        .map(|e| {
            let content_hash = hash_file_entry(e, workspace_root)?;
            Ok::<_, std::io::Error>(RecordedFile {
                path: e.path.clone(),
                lines: e.lines.clone(),
                content_hash,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let dirs = reads
        .dirs
        .iter()
        .map(|e| {
            let listing_hash = hash_dir_entry(e, workspace_root)?;
            Ok::<_, std::io::Error>(RecordedDir {
                path: e.path.clone(),
                filter: e.filter.clone(),
                listing_hash,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(RecordedReads { files, dirs })
}

/// Recompute hashes for every entry in `recorded` against the workspace,
/// returning `Match` only if every entry's current hash matches what was
/// recorded. The first mismatch is reported; remaining entries aren't
/// checked since any single change invalidates the whole cached output.
pub fn verify_reads(
    recorded: &RecordedReads,
    workspace_root: &std::path::Path,
) -> std::io::Result<VerifyOutcome> {
    for f in &recorded.files {
        let entry = ReadFileEntry {
            path: f.path.clone(),
            lines: f.lines.clone(),
        };
        match hash_file_entry(&entry, workspace_root) {
            Ok(current) if current == f.content_hash => {}
            Ok(_) => {
                return Ok(VerifyOutcome::Mismatch {
                    reason: format!("file content changed: {}", f.path.as_path().display()),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(VerifyOutcome::Mismatch {
                    reason: format!("file missing: {}", f.path.as_path().display()),
                });
            }
            Err(e) => return Err(e),
        }
    }

    for d in &recorded.dirs {
        let entry = ReadDirEntry {
            path: d.path.clone(),
            filter: d.filter.clone(),
        };
        match hash_dir_entry(&entry, workspace_root) {
            Ok(current) if current == d.listing_hash => {}
            Ok(_) => {
                return Ok(VerifyOutcome::Mismatch {
                    reason: format!("directory listing changed: {}", d.path.as_path().display()),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(VerifyOutcome::Mismatch {
                    reason: format!("directory missing: {}", d.path.as_path().display()),
                });
            }
            Err(e) => return Err(e),
        }
    }

    Ok(VerifyOutcome::Match)
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

    #[test]
    fn discovery_key_is_deterministic() {
        let a = include_spec("d", "tools/d.sh > out.json");
        let b = include_spec("d", "tools/d.sh > out.json");
        assert_eq!(discovery_cache_key(&a), discovery_cache_key(&b));
    }

    #[test]
    fn discovery_key_changes_with_command() {
        let a = include_spec("d", "tools/d.sh > out.json");
        let b = include_spec("d", "tools/d2.sh > out.json");
        assert_ne!(discovery_cache_key(&a), discovery_cache_key(&b));
    }

    #[test]
    fn discovery_key_changes_with_cwd() {
        let mut a = include_spec("d", "tools/d.sh");
        let mut b = include_spec("d", "tools/d.sh");
        a.cwd = WsRelPath::new("pkg").unwrap();
        b.cwd = WsRelPath::new("cmd").unwrap();
        assert_ne!(discovery_cache_key(&a), discovery_cache_key(&b));
    }

    #[test]
    fn discovery_key_changes_with_env_value() {
        let mut a = include_spec("d", "tools/d.sh");
        let mut b = include_spec("d", "tools/d.sh");
        a.env.insert("LANG".into(), "en_US.UTF-8".into());
        b.env.insert("LANG".into(), "C".into());
        assert_ne!(discovery_cache_key(&a), discovery_cache_key(&b));
    }

    #[test]
    fn discovery_key_stable_under_env_declaration_order() {
        // env is a HashMap so declaration order is already lost, but make
        // it explicit: same env entries inserted differently produce the
        // same key thanks to the sort in `discovery_cache_key`.
        let mut a = include_spec("d", "tools/d.sh");
        let mut b = include_spec("d", "tools/d.sh");
        a.env.insert("A".into(), "1".into());
        a.env.insert("B".into(), "2".into());
        b.env.insert("B".into(), "2".into());
        b.env.insert("A".into(), "1".into());
        assert_eq!(discovery_cache_key(&a), discovery_cache_key(&b));
    }

    #[test]
    fn discovery_key_changes_with_scope() {
        let mut a = include_spec("d", "tools/d.sh");
        let mut b = include_spec("d", "tools/d.sh");
        a.scope = vec![WsRelPath::new("pkg").unwrap()];
        b.scope = vec![WsRelPath::new("cmd").unwrap()];
        assert_ne!(discovery_cache_key(&a), discovery_cache_key(&b));
    }

    #[test]
    fn discovery_key_stable_under_scope_order() {
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
        assert_eq!(discovery_cache_key(&a), discovery_cache_key(&b));
    }

    #[test]
    fn discovery_key_empty_scope_vs_set_scope_differ() {
        let mut a = include_spec("d", "tools/d.sh");
        let b = include_spec("d", "tools/d.sh");
        a.scope = vec![WsRelPath::new("pkg").unwrap()];
        assert_ne!(discovery_cache_key(&a), discovery_cache_key(&b));
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
