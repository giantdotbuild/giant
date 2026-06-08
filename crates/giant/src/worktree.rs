//! Disposable git worktree for `giant verify` (ADR-0036).
//!
//! A verify run builds in a throwaway checkout of the committed state rather
//! than the live tree, so a sandboxed command can never mutate or delete the
//! user's working files. The worktree shares the repo's object store and omits
//! gitignored paths (`node_modules`, `target/`, `.giant`), so it is cheap to
//! create and tear down. A jj colocated repo works through its git view.

use std::io;
use std::path::{Path, PathBuf};

use tokio::process::Command;

/// A checked-out worktree that removes itself on drop.
pub struct Worktree {
    repo: PathBuf,
    path: PathBuf,
}

impl Worktree {
    /// Absolute path to the checkout. Point the build's workspace root here.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        // Best-effort cleanup. `git worktree remove --force` drops the checkout
        // and its administrative entry; `prune` mops up if the directory was
        // already gone (e.g. removed out from under us).
        let _ = std::process::Command::new("git")
            .current_dir(&self.repo)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .output();
        let _ = std::process::Command::new("git")
            .current_dir(&self.repo)
            .args(["worktree", "prune"])
            .output();
    }
}

/// Check out the committed state (`HEAD`) into a throwaway worktree keyed by
/// `id`. The workspace must be under git (a jj colocated repo counts); without
/// it verify cannot isolate, so this returns a clear error rather than falling
/// back to the live tree.
pub async fn create(workspace_root: &Path, id: &str) -> io::Result<Worktree> {
    if !is_git_repo(workspace_root).await {
        return Err(io::Error::other(
            "giant verify builds in an isolated worktree and needs a git \
             repository (a jj colocated repo works too). Initialise one, or \
             build without verify.",
        ));
    }

    let dest = std::env::temp_dir()
        .join("giant-worktrees")
        .join(sanitize(id));
    // A crashed earlier run may have left this path or a stale registration.
    let _ = tokio::fs::remove_dir_all(&dest).await;
    let _ = git(workspace_root, &["worktree", "prune"]).await;

    let out = Command::new("git")
        .current_dir(workspace_root)
        .args(["worktree", "add", "--detach", "--quiet"])
        .arg(&dest)
        .arg("HEAD")
        .output()
        .await?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    Ok(Worktree {
        repo: workspace_root.to_path_buf(),
        path: dest,
    })
}

async fn is_git_repo(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--is-inside-work-tree"])
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn git(dir: &Path, args: &[&str]) -> io::Result<std::process::Output> {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .await
}

/// Turn an arbitrary id into a safe single path component.
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
