//! `giant-task --watch` - re-run a task on file changes.
//!
//! Watches the task's declared `inputs:` patterns when present, falling
//! back to the workspace root (with `.git/`, `.giant/`, and the cache
//! dir excluded so we don't loop on our own writes). Uses the same
//! `notify`-backed watcher as core, debounced via a quiet-window +
//! max-delay pair. Ctrl-C exits the loop.
//!
//! Each cycle runs the task once. If a build is in flight when a new
//! batch lands, the current cycle is allowed to finish; the new events
//! become the next cycle's batch.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use giant::watcher;
use tokio::sync::mpsc;

use crate::config::TaskConfig;
use crate::runner;

#[allow(clippy::too_many_arguments)]
pub async fn loop_forever(
    cfg: &TaskConfig,
    name: &str,
    positionals: &[String],
    args: &[String],
    workspace_root: &Path,
    verbose: bool,
    quiet_window: Duration,
    max_delay: Duration,
) -> anyhow::Result<u8> {
    let task = cfg
        .tasks
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("unknown task: {name}"))?
        .clone();

    let excludes: Vec<PathBuf> = vec![
        workspace_root.join(".git"),
        workspace_root.join(".giant"),
        workspace_root.join("output"), // most workspaces write build outputs here
    ];

    println!("· initial run");
    let _ = runner::run(cfg, name, positionals, args, workspace_root, verbose).await;

    let (_handle, mut rx) = watcher::spawn(workspace_root, excludes.clone())
        .map_err(|e| anyhow::anyhow!("file watcher failed to start: {e}"))?;

    let input_matchers = compile_matchers(&task.inputs);
    println!(
        "· watching {} - Ctrl-C to exit",
        if task.inputs.is_empty() {
            workspace_root.display().to_string()
        } else {
            format!("{} input pattern(s)", task.inputs.len())
        }
    );

    let mut debouncer = Debouncer::new(quiet_window, max_delay);
    loop {
        let batch = match debouncer.next_batch(&mut rx).await {
            Some(b) if !b.is_empty() => b,
            Some(_) => continue,
            None => break,
        };

        // Filter to paths matching the task's input patterns (if any).
        // If no patterns, every batch counts.
        let relevant: Vec<PathBuf> = if input_matchers.is_empty() {
            batch.into_iter().collect()
        } else {
            batch
                .into_iter()
                .filter(|p| {
                    let rel = p.strip_prefix(workspace_root).unwrap_or(p);
                    let rel_str = rel.to_string_lossy();
                    input_matchers.iter().any(|pat| pat.matches(&rel_str))
                })
                .collect()
        };
        if relevant.is_empty() {
            continue;
        }

        println!();
        println!("· {} file(s) changed, re-running", relevant.len());
        let _ = runner::run(cfg, name, positionals, args, workspace_root, verbose).await;
    }

    Ok(0)
}

fn compile_matchers(patterns: &[String]) -> Vec<glob::Pattern> {
    patterns
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect()
}

/// Coalesce a burst of file events into one batch. Same shape as core
/// uses for `giant watch`: flush after `quiet` ms of silence, OR after
/// `max` ms from the first event, whichever comes first.
struct Debouncer {
    quiet: Duration,
    max: Duration,
}

impl Debouncer {
    fn new(quiet: Duration, max: Duration) -> Self {
        Self { quiet, max }
    }

    async fn next_batch(&mut self, rx: &mut mpsc::Receiver<PathBuf>) -> Option<HashSet<PathBuf>> {
        let first = rx.recv().await?;
        let started = Instant::now();
        let mut batch: HashSet<PathBuf> = HashSet::new();
        batch.insert(first);

        loop {
            let quiet_left = self.quiet;
            let max_left = self.max.saturating_sub(started.elapsed());
            let timeout = quiet_left.min(max_left);
            if timeout.is_zero() {
                break;
            }
            tokio::select! {
                evt = rx.recv() => {
                    match evt {
                        Some(p) => { batch.insert(p); }
                        None => return Some(batch),
                    }
                }
                _ = tokio::time::sleep(timeout) => break,
            }
        }
        Some(batch)
    }
}
