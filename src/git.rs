//! Small focused git interface for the structural-input fast-path and
//! affected-detection. Backed by `gix`.
//!
//! See TDD-0002 §Git fast-path.

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("not a git repository: {0}")]
    NotARepo(PathBuf),

    #[error("git operation failed: {0}")]
    Other(String),
}

/// Files tracked in the git index plus untracked files in the worktree.
pub struct IndexAndStatus {
    pub tracked: Vec<PathBuf>,
    pub untracked: Vec<PathBuf>,
}

/// Result of a fast `git status` scoped to pathspecs.
pub struct StatusFast {
    pub modified: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    pub untracked: Vec<PathBuf>,
}

/// Return tracked files matching `extensions` plus all untracked.
/// Cold-path enumeration for structural inputs (TDD-0002).
pub fn get_index_files_and_status(
    _workspace_root: &Path,
    _extensions: &[&str],
) -> Result<IndexAndStatus, GitError> {
    todo!("TDD-0002 §Cold computation via git index")
}

/// Scoped `git status` for warm structural validation (TDD-0002).
pub fn get_full_status_fast_scoped(
    _workspace_root: &Path,
    _pathspecs: &[gix::bstr::BString],
) -> Result<StatusFast, GitError> {
    todo!("TDD-0002 §Warm validation via git status")
}

/// HEAD commit hash, if available.
pub fn head_commit(_workspace_root: &Path) -> Result<Option<String>, GitError> {
    todo!()
}
