//! Discovery: build `include:` targets first, merge their JSON outputs
//! into the main graph.
//!
//! See TDD-0003 for the bootstrap-pass scheduling and merge rules.

use crate::graph::{BuildGraph, GraphError};
use crate::model::TargetSpec;
use serde::{Deserialize, Serialize};

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
    /// `include:` it emits goes into the next wave - recursively, up
    /// to a depth limit. See TDD-0003 §Wave-based bootstrap.
    #[serde(default)]
    pub include: Vec<TargetSpec>,

    /// Optional fingerprint-input list (TDD-0001 / TDD-0003): files the
    /// discovery script actually read. Reserved for future use in tighter
    /// cache invalidation than declared inputs alone.
    #[serde(default)]
    pub fingerprint_inputs: Vec<String>,
}

fn default_schema_version() -> u32 {
    1
}

const SUPPORTED_SCHEMA: u32 = 1;

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
        assert!(graph.get(&crate::model::TargetId::new("discover:wave2")).is_some());
        assert!(graph.get(&crate::model::TargetId::new("x")).is_some());
    }
}
