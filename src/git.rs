//! Small focused git interface - just what the structural-input fast-path
//! needs (TDD-0002 §Warm validation).
//!
//! Two operations:
//! - `get_index_files_and_status`: enumerate tracked files from the
//!   index (single file read, no recursive walk) plus untracked files
//!   from `git status`. Used for cold structural-input compute.
//! - `get_full_status_fast_scoped`: `git status` scoped via pathspecs.
//!   Used for warm validation - only files git reports as modified /
//!   added / deleted get re-hashed.

use gix::bstr::ByteSlice;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("not a git repository: {0}")]
    NotARepo(PathBuf),

    #[error("git operation failed: {0}")]
    Other(String),
}

/// Files tracked in the git index plus all untracked files visible to
/// `git status`. Paths are workspace-relative.
pub struct IndexAndStatus {
    pub tracked: Vec<PathBuf>,
    pub untracked: Vec<PathBuf>,
}

/// Result of a fast `git status` query. Paths are workspace-relative.
#[derive(Debug, Default)]
pub struct StatusFast {
    pub modified: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    pub untracked: Vec<PathBuf>,
}

fn open(workspace_root: &Path) -> Result<gix::Repository, GitError> {
    match gix::discover(workspace_root) {
        Ok(repo) => Ok(repo),
        Err(e) => {
            // Distinguish "not a repo" from real errors so callers can fall
            // back gracefully.
            let msg = e.to_string();
            if msg.contains("not a git repository") || msg.contains("does not exist") {
                Err(GitError::NotARepo(workspace_root.to_path_buf()))
            } else {
                Err(GitError::Other(msg))
            }
        }
    }
}

/// Enumerate tracked files matching `extensions_no_dot` from the git
/// index, plus all untracked files. Returns `None` when the workspace
/// isn't a git repository, so callers can fall back to a filesystem walk.
///
/// Extension match is the lowercase extension of the path's filename;
/// `extensions_no_dot` should be like `["go", "mod"]` (no leading dot).
pub fn get_index_files_and_status(
    workspace_root: &Path,
    extensions_no_dot: &[&str],
) -> Option<IndexAndStatus> {
    let repo = open(workspace_root).ok()?;
    let index = repo.open_index().ok()?;

    let tracked: Vec<PathBuf> = index
        .entries()
        .iter()
        .filter(|entry| {
            matches!(
                entry.mode,
                gix::index::entry::Mode::FILE | gix::index::entry::Mode::FILE_EXECUTABLE
            )
        })
        .filter_map(|entry| {
            let path_bstr = entry.path_in(index.path_backing());
            let path_str = path_bstr.to_str().ok()?;
            if !extensions_no_dot.is_empty() {
                let ext = Path::new(path_str).extension()?.to_str()?;
                if !extensions_no_dot.contains(&ext) {
                    return None;
                }
            }
            Some(PathBuf::from(path_str))
        })
        .collect();

    let mut untracked: Vec<PathBuf> = Vec::new();
    if let Ok(s) = repo.status(gix::progress::Discard) {
        let iter = s
            .untracked_files(gix::status::UntrackedFiles::Files)
            .into_index_worktree_iter(Vec::<gix::bstr::BString>::new());
        if let Ok(iter) = iter {
            for item in iter.flatten() {
                if let Some(gix::status::index_worktree::iter::Summary::Added) = item.summary() {
                    let p = item.rela_path().to_str_lossy().into_owned();
                    untracked.push(PathBuf::from(p));
                }
            }
        }
    }

    Some(IndexAndStatus { tracked, untracked })
}

/// `git status` scoped via pathspecs. Returns the three lists of paths
/// (modified, deleted, untracked) - workspace-relative.
///
/// Returns `None` if not in a git repository. Used for warm structural-
/// input validation: only files git reports as changed need re-reading.
pub fn get_full_status_fast_scoped(
    workspace_root: &Path,
    pathspecs: &[gix::bstr::BString],
) -> Option<StatusFast> {
    let repo = open(workspace_root).ok()?;

    let mut out = StatusFast::default();

    let status = repo.status(gix::progress::Discard).ok()?;
    let iter = status
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_index_worktree_iter(pathspecs.to_vec())
        .ok()?;

    for item in iter.flatten() {
        if let Some(summary) = item.summary() {
            use gix::status::index_worktree::iter::Summary;
            let p = item.rela_path().to_str_lossy().into_owned();
            let path = PathBuf::from(p);
            match summary {
                Summary::Removed => out.deleted.push(path),
                Summary::Modified | Summary::TypeChange | Summary::Conflict => {
                    out.modified.push(path)
                }
                Summary::Added | Summary::IntentToAdd | Summary::Renamed | Summary::Copied => {
                    out.untracked.push(path)
                }
            }
        }
    }
    out.modified.sort();
    out.deleted.sort();
    out.untracked.sort();
    Some(out)
}

/// HEAD commit hash, hex-encoded. `None` if not in a git repo or no HEAD.
pub fn head_commit(workspace_root: &Path) -> Option<String> {
    let repo = open(workspace_root).ok()?;
    let head = repo.head_id().ok()?;
    Some(head.to_string())
}
