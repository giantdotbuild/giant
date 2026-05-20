//! Build graph.
//!
//! Wraps `petgraph` with target-id semantics. Holds the merged set of
//! targets and the dep edges (explicit + output-inferred). See TDD-0003
//! for inference, TDD-0001 for the target schema.

use crate::model::{TargetId, TargetSpec};
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("duplicate target id: {0}")]
    DuplicateId(TargetId),

    #[error("dep references unknown target: {0} (from {1})")]
    UnknownDep(TargetId, TargetId),

    #[error("dependency cycle: {0}")]
    Cycle(String),

    #[error("two targets produce the same output {path}: {a} and {b}")]
    OutputCollision { path: String, a: TargetId, b: TargetId },
}

#[derive(Debug, Default)]
pub struct BuildGraph {
    targets: HashMap<TargetId, TargetSpec>,
    // TODO(impl): petgraph DiGraph<TargetId, ()> for edges + cycle detection
}

impl BuildGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_target(&mut self, spec: TargetSpec) -> Result<(), GraphError> {
        if self.targets.contains_key(&spec.id) {
            return Err(GraphError::DuplicateId(spec.id));
        }
        self.targets.insert(spec.id.clone(), spec);
        Ok(())
    }

    pub fn get(&self, id: &TargetId) -> Option<&TargetSpec> {
        self.targets.get(id)
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&TargetId, &TargetSpec)> {
        self.targets.iter()
    }

    /// Run output-based dep inference over the current set of targets,
    /// populating `inferred_deps` and adding the edges (TDD-0003).
    pub fn apply_output_based_inference(&mut self) -> Result<(), GraphError> {
        // TODO(impl): inverted-output index + glob match against inputs.
        // See TDD-0003 §Output-based dep inference.
        Ok(())
    }

    /// Detect cycles in the (explicit + inferred) dep graph.
    pub fn validate_acyclic(&self) -> Result<(), GraphError> {
        // TODO(impl): petgraph toposort and report the cycle path.
        Ok(())
    }
}
