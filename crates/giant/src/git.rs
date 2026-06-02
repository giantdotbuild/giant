//! Small focused git interface used by `giant affected` and the fsmonitor
//! client.
//!
//! - `open`: discover the repository, distinguishing "not a repo" from
//!   real errors so callers can fall back gracefully.
//! - `affected_files_since`: files changed since a git ref, for
//!   `giant affected`.

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("not a git repository: {0}")]
    NotARepo(PathBuf),

    #[error("git operation failed: {0}")]
    Other(String),
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
