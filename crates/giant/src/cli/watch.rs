//! Shared file-watch mechanics for the engine's watch loops.
//!
//! `build --watch` / `test --watch` and the stdio session's `watch.start`
//! both run *inside the engine* now (TDD-0021): the CLI dispatches
//! `watch.start` to an in-process `SessionState`, exactly like the TUI.
//! This module holds the pieces that loop shares - the exclude set, the
//! debouncer, and the per-cycle "what's affected" step.

use crate::model::TargetId;
use crate::paths::AbsPath;
use crate::selection;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The standard watcher exclude set: `.git`, the configured state dir,
/// the cache dir, and every declared output path - so the engine's own
/// writes don't loop the watcher. Shared by the session-mode watch /
/// affected / change-subscription loops.
///
/// `state_dir` is the workspace-relative state directory
/// (`config.state.dir`, default `.giant`); honouring it rather than
/// hardcoding `.giant` keeps a custom state dir from self-triggering.
pub(crate) fn standard_excludes(
    workspace_root: &AbsPath,
    cache_root: &AbsPath,
    state_dir: &Path,
    graph: &crate::graph::BuildGraph,
) -> Vec<PathBuf> {
    let mut excludes = vec![
        workspace_root.as_path().join(".git"),
        workspace_root.as_path().join(state_dir),
        cache_root.as_path().to_path_buf(),
    ];
    for (_, spec) in graph.iter() {
        for o in &spec.outputs {
            excludes.push(workspace_root.as_path().join(o.as_path()));
        }
    }
    excludes
}

/// Debounce one change batch and reduce it to the affected subset of
/// `selection` - the shared watch *mechanics* behind `build --watch` and
/// the session's `watch.start`. Returns the targets to rebuild this
/// cycle (possibly **empty** when a real change touched nothing in the
/// selection - the caller decides whether to note that), or `None` when
/// the watch is over (cancelled or the watcher channel closed).
pub(crate) async fn next_affected_cycle(
    graph: &crate::graph::BuildGraph,
    rx: &mut tokio::sync::mpsc::Receiver<PathBuf>,
    debouncer: &mut Debouncer,
    selection: &[TargetId],
    workspace_root: &AbsPath,
    cancel: &CancellationToken,
) -> Option<Vec<TargetId>> {
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        let batch = match debouncer.next_batch(rx, cancel).await {
            // A non-empty batch is a real change set; an empty one is a
            // cancellation wake - loop and the check above exits.
            Some(b) if !b.is_empty() => b,
            Some(_) => continue,
            None => return None,
        };
        let rel: Vec<PathBuf> = batch
            .iter()
            .map(|p| {
                p.strip_prefix(workspace_root.as_path())
                    .unwrap_or(p)
                    .to_path_buf()
            })
            .collect();
        let refs: Vec<&Path> = rel.iter().map(|p| p.as_path()).collect();
        let affected = selection::affected_targets(graph, &refs);
        return Some(
            selection
                .iter()
                .filter(|id| affected.contains(*id))
                .cloned()
                .collect(),
        );
    }
}

// =============================================================================
// Debouncer
// =============================================================================

/// Coalesces file events into batches.
///
/// Each batch starts when the first event arrives. The batch flushes
/// when either:
/// - `quiet` time has passed since the *last* event in this batch
///   (the user has stopped editing), OR
/// - `max_delay` time has passed since the *first* event in this batch
///   (events are streaming continuously; we flush so the user gets
///   feedback).
///
/// Returns `None` if the channel closes mid-wait.
pub struct Debouncer {
    quiet: Duration,
    max_delay: Duration,
    pending: HashSet<PathBuf>,
    first_event: Option<Instant>,
    last_event: Option<Instant>,
}

impl Debouncer {
    pub fn new(quiet: Duration, max_delay: Duration) -> Self {
        Self {
            quiet,
            max_delay,
            pending: HashSet::new(),
            first_event: None,
            last_event: None,
        }
    }

    /// Wait for the next debounced batch. Returns `None` if `rx` closes
    /// while we have no pending events. On cancellation, returns an
    /// empty batch so the caller can re-check and exit cleanly.
    pub async fn next_batch(
        &mut self,
        rx: &mut mpsc::Receiver<PathBuf>,
        cancel: &CancellationToken,
    ) -> Option<Vec<PathBuf>> {
        loop {
            // How long until we must flush, given the pending state?
            // None = no pending, wait indefinitely for a first event.
            let until_flush = match (self.first_event, self.last_event) {
                (Some(first), Some(last)) => Some(
                    self.quiet
                        .saturating_sub(last.elapsed())
                        .min(self.max_delay.saturating_sub(first.elapsed())),
                ),
                _ => None,
            };

            // Helper future that yields once `until_flush` elapses, or
            // never resolves if `until_flush` is None.
            let flush_due = async {
                match until_flush {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending::<()>().await,
                }
            };

            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Some(self.flush()),
                _ = flush_due => return Some(self.flush()),
                got = rx.recv() => match got {
                    Some(path) => {
                        let now = Instant::now();
                        if self.first_event.is_none() {
                            self.first_event = Some(now);
                        }
                        self.last_event = Some(now);
                        self.pending.insert(path);
                    }
                    None => {
                        // channel closed
                        if self.pending.is_empty() {
                            return None;
                        }
                        return Some(self.flush());
                    }
                }
            }
        }
    }

    fn flush(&mut self) -> Vec<PathBuf> {
        self.first_event = None;
        self.last_event = None;
        self.pending.drain().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn debouncer_returns_single_event_after_quiet_window() {
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let mut deb = Debouncer::new(Duration::from_millis(30), Duration::from_millis(500));
        tx.send(PathBuf::from("a")).await.unwrap();

        let batch =
            tokio::time::timeout(Duration::from_millis(200), deb.next_batch(&mut rx, &cancel))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(batch, vec![PathBuf::from("a")]);
    }

    #[tokio::test]
    async fn debouncer_coalesces_burst() {
        let (tx, mut rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let mut deb = Debouncer::new(Duration::from_millis(40), Duration::from_millis(500));
        for i in 0..10 {
            tx.send(PathBuf::from(format!("f{i}"))).await.unwrap();
        }
        let batch =
            tokio::time::timeout(Duration::from_millis(300), deb.next_batch(&mut rx, &cancel))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(batch.len(), 10, "all 10 events should coalesce");
    }

    #[tokio::test]
    async fn debouncer_max_delay_flushes_with_streaming_events() {
        let (tx, mut rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let mut deb = Debouncer::new(Duration::from_millis(40), Duration::from_millis(80));
        // Stream events forever - until max_delay forces a flush.
        let send_task = tokio::spawn(async move {
            for i in 0..200 {
                let _ = tx.send(PathBuf::from(format!("s{i}"))).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let start = Instant::now();
        let batch = deb.next_batch(&mut rx, &cancel).await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 200,
            "max_delay should bound the batch wait; got {elapsed:?}"
        );
        assert!(
            !batch.is_empty(),
            "should have collected at least one event"
        );
        send_task.abort();
    }

    #[tokio::test]
    async fn debouncer_dedupes_same_path() {
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let mut deb = Debouncer::new(Duration::from_millis(30), Duration::from_millis(200));
        for _ in 0..5 {
            tx.send(PathBuf::from("same")).await.unwrap();
        }
        let batch =
            tokio::time::timeout(Duration::from_millis(200), deb.next_batch(&mut rx, &cancel))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(batch, vec![PathBuf::from("same")]);
    }
}
