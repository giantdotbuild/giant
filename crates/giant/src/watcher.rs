//! Recursive file watcher backed by the `notify` crate.
//!
//! Bridges notify's sync callback into a tokio mpsc channel. Filters
//! out paths under known "noise" prefixes (`.git/`, `.giant/`, the
//! cache directory) at the watcher boundary so the debouncer doesn't
//! waste cycles on them.
//!
//! Caller keeps the returned `WatcherHandle` alive for as long as
//! they want events; dropping it tears down the OS watch.

use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum WatcherError {
    #[error("notify: {0}")]
    Notify(#[from] notify::Error),
}

/// Owned watcher handle. The OS watch stays active until this is dropped.
pub struct WatcherHandle {
    _watcher: notify::RecommendedWatcher,
}

/// Spawn a recursive watcher over `root`. Events for paths that start
/// with any entry in `exclude_prefixes` are silently dropped at the
/// boundary so they never reach the debouncer.
///
/// Returns the handle (keep it alive!) and a receiver of changed paths.
pub fn spawn(
    root: &Path,
    exclude_prefixes: Vec<PathBuf>,
) -> Result<(WatcherHandle, mpsc::Receiver<PathBuf>), WatcherError> {
    spawn_dirs(&[root.to_path_buf()], true, exclude_prefixes)
}

/// Spawn a watcher over each of `dirs`, recursively or not. A non-recursive
/// watch over a handful of known directories is far cheaper than a recursive
/// watch over a whole workspace (which registers an OS watch per subdirectory -
/// pathological under large trees like `.devenv`). Individual directories that
/// can't be watched (e.g. removed) are skipped rather than failing the whole
/// watch. Events under any `exclude_prefixes` entry are dropped at the boundary.
pub fn spawn_dirs(
    dirs: &[PathBuf],
    recursive: bool,
    exclude_prefixes: Vec<PathBuf>,
) -> Result<(WatcherHandle, mpsc::Receiver<PathBuf>), WatcherError> {
    let (tx, rx) = mpsc::channel::<PathBuf>(1024);
    let excludes = exclude_prefixes.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let Ok(event) = res else { return };
        if !relevant_kind(&event.kind) {
            return;
        }
        for path in event.paths {
            if excludes.iter().any(|p| path.starts_with(p)) {
                continue;
            }
            // try_send: if the queue is full we drop the event. Better
            // than blocking the watcher thread and risking buildup. The
            // debouncer will see the next event anyway.
            let _ = tx.try_send(path);
        }
    })?;
    for dir in dirs {
        if recursive {
            // We can't use notify's `RecursiveMode::Recursive`: it descends and
            // registers an OS watch under *every* subdirectory, including
            // gitignored/hidden trees like `.devenv` (excludes only filter
            // events, not registration). Walk ourselves with a gitignore-aware
            // walker, which prunes ignored and hidden dirs, and watch each kept
            // directory non-recursively.
            watch_pruned(&mut watcher, dir, &exclude_prefixes);
        } else {
            let _ = watcher.watch(dir, RecursiveMode::NonRecursive);
        }
    }
    Ok((WatcherHandle { _watcher: watcher }, rx))
}

/// Register a non-recursive watch on every directory under `root`, skipping
/// gitignored and hidden trees (via the `ignore` walker - so `.git`, `.devenv`,
/// `node_modules`, `target`, … are never descended) and any directory under an
/// `exclude_prefixes` entry (tracked outputs that should not trigger rebuilds).
fn watch_pruned(
    watcher: &mut notify::RecommendedWatcher,
    root: &Path,
    exclude_prefixes: &[PathBuf],
) {
    let excludes: Vec<PathBuf> = exclude_prefixes.to_vec();
    let walk = ignore::WalkBuilder::new(root)
        .filter_entry(move |e| !excludes.iter().any(|p| e.path().starts_with(p)))
        .build();
    for entry in walk.flatten() {
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            let _ = watcher.watch(entry.path(), RecursiveMode::NonRecursive);
        }
    }
}

fn relevant_kind(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
    )
}
