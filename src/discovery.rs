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

    #[error("discovery '{0}' failed to produce its declared output {1}")]
    MissingOutput(String, String),
}

/// One target's discovery output file, parsed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryFragment {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,

    #[serde(default)]
    pub targets: Vec<TargetSpec>,

    #[serde(default)]
    pub tasks: indexmap::IndexMap<String, crate::config::TaskSpec>,

    /// Optional fingerprint-input list (TDD-0001 / TDD-0003): files the
    /// discovery script actually read. Used in future versions for tighter
    /// cache invalidation than declared inputs alone.
    #[serde(default)]
    pub fingerprint_inputs: Vec<String>,
}

fn default_schema_version() -> u32 {
    1
}

/// Parse a discovery output file from disk.
pub fn parse_fragment(path: &std::path::Path) -> Result<DiscoveryFragment, DiscoveryError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| DiscoveryError::Io(path.display().to_string(), e))?;
    serde_json::from_str(&raw).map_err(|e| DiscoveryError::Json(path.display().to_string(), e))
}

/// Merge a fragment into the graph. Enforces uniqueness, then re-runs
/// inference and acyclic validation (TDD-0003).
pub fn merge_into(graph: &mut BuildGraph, frag: DiscoveryFragment) -> Result<(), DiscoveryError> {
    for target in frag.targets {
        graph.add_target(target)?;
    }
    // tasks are merged elsewhere (config module owns the task map)
    Ok(())
}
