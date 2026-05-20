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

    /// Discovered tasks. Engine doesn't have tasks-from-discovery wired in
    /// v0.1 yet (TDD-0005); accept and ignore for forward compatibility.
    #[serde(default)]
    pub tasks: indexmap::IndexMap<String, crate::config::TaskSpec>,

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

/// Merge a fragment's targets into the graph. Tasks are accepted but
/// not yet integrated (TDD-0005 task-from-discovery is v1.1).
pub fn merge_into(graph: &mut BuildGraph, frag: DiscoveryFragment) -> Result<(), DiscoveryError> {
    for target in frag.targets {
        graph.add_target(target)?;
    }
    Ok(())
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
        assert!(matches!(err, DiscoveryError::UnsupportedSchema { found: 99, .. }));
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
        merge_into(&mut graph, frag).unwrap();
        assert!(graph.get(&crate::model::TargetId::new("x")).is_some());
    }
}
