//! Target selection - resolve CLI args (patterns + flags) into a set of
//! target IDs. See TDD-0011.

use crate::graph::BuildGraph;
use crate::model::TargetId;

#[derive(Debug, thiserror::Error)]
pub enum SelectionError {
    #[error("no targets matched: {detail}")]
    Empty { detail: String },

    #[error("invalid pattern '{0}': {1}")]
    BadPattern(String, glob::PatternError),
}

#[derive(Debug, Clone, Default)]
pub struct SelectionRequest {
    pub patterns: Vec<String>,
    pub exclude: Vec<String>,
    pub tags: Vec<String>,
    pub no_tags: Vec<String>,
    pub affected_base: Option<String>,
    pub affected_files: Vec<String>,
    pub include_tests: bool,
    pub tests_only: bool,
}

/// Resolve a selection against the merged graph. Returns the final set
/// of target IDs to build, in graph order.
pub fn resolve(
    _graph: &BuildGraph,
    _req: &SelectionRequest,
) -> Result<Vec<TargetId>, SelectionError> {
    todo!("TDD-0011: pipeline (patterns, exclude, affected, tags, defaults)")
}
