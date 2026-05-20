//! Structural inputs: line-pattern fingerprinting with git fast-path.
//!
//! See TDD-0002 for the algorithm.

use crate::model::ContentHash;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum StructuralError {
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid sidecar: {0}")]
    Corrupt(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sidecar {
    pub schema: u32,
    pub target_id: String,
    pub computed_at: i64,
    pub git_head_at_computation: Option<String>,
    pub gitignore_hash: Option<String>,
    pub inputs: Vec<SidecarInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarInput {
    pub files: Vec<String>,
    pub lines: Vec<String>,
    pub scope: Vec<String>,
    pub global_hash: String,
    pub per_file: BTreeMap<String, PerFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerFileEntry {
    pub lines_hash: String,
    pub mtime_ns: Option<u64>,
    pub size: Option<u64>,
}

/// Compute the global fingerprint for one structural input on one target,
/// cold path. See TDD-0002 §Cold computation.
pub fn compute_cold(
    _workspace_root: &Path,
    _files: &[String],
    _lines: &[String],
    _scope: &[String],
) -> Result<ContentHash, StructuralError> {
    todo!("TDD-0002 cold compute via git index or filesystem walk")
}

/// Validate a sidecar against the current workspace state, returning
/// `Ok(None)` if still valid or `Ok(Some(new_hash))` if recomputed.
pub fn validate_or_recompute(
    _workspace_root: &Path,
    _sidecar: &mut Sidecar,
) -> Result<Option<ContentHash>, StructuralError> {
    todo!("TDD-0002 warm validation via git status or mtime")
}
