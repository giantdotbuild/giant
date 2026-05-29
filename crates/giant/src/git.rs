//! Small focused git interface - just what the structural-input fast-path
//! needs (TDD-0002 §Enumeration).
//!
//! - `get_index_files_and_status`: enumerate tracked files from the
//!   index (single file read, no recursive walk) plus untracked files
//!   from `git status`. The structural fast-path fingerprints each, using
//!   the per-target sidecar's recorded (mtime, size) to skip re-reads.
//! - `affected_files_since`: files changed since a git ref, for
//!   `giant affected`.

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

/// Discover the git repository containing `workspace_root`. Distinguishes
/// "not a repo" from real errors so callers can fall back gracefully.
pub fn open(workspace_root: &Path) -> Result<gix::Repository, GitError> {
    match gix::discover(workspace_root) {
        Ok(repo) => Ok(repo),
        Err(e) => {
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

/// Files changed in the working tree (committed + uncommitted) since
/// `base`, plus untracked-but-not-gitignored files. Workspace-relative
/// paths.
///
/// Implementation shells out to `git` for two reasons:
/// (1) `git diff --name-only --no-renames <base>` and
/// `git ls-files --others --exclude-standard` are stable, well-known
/// commands users can audit;
/// (2) the gix `diff` API for the same query is significantly more
/// code without any speed advantage at our scale.
pub fn affected_files_since(
    workspace_root: &Path,
    base: &str,
) -> Result<Vec<std::path::PathBuf>, GitError> {
    use std::process::Command;

    let diff = Command::new("git")
        .args(["diff", "--name-only", "--no-renames", "-z", base])
        .current_dir(workspace_root)
        .output()
        .map_err(|e| GitError::Other(format!("spawn git diff: {e}")))?;
    if !diff.status.success() {
        return Err(GitError::Other(format!(
            "git diff failed: {}",
            String::from_utf8_lossy(&diff.stderr).trim()
        )));
    }

    let untracked = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .current_dir(workspace_root)
        .output()
        .map_err(|e| GitError::Other(format!("spawn git ls-files: {e}")))?;
    if !untracked.status.success() {
        return Err(GitError::Other(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&untracked.stderr).trim()
        )));
    }

    let mut files: Vec<std::path::PathBuf> = parse_z_separated(&diff.stdout);
    files.extend(parse_z_separated(&untracked.stdout));
    files.sort();
    files.dedup();
    Ok(files)
}

/// Parse `-z` (NUL-separated) output from git into PathBufs.
fn parse_z_separated(bytes: &[u8]) -> Vec<std::path::PathBuf> {
    bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| {
            std::path::PathBuf::from(std::ffi::OsString::from(
                String::from_utf8_lossy(s).into_owned(),
            ))
        })
        .collect()
}
