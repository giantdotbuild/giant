//! `giant watch [patterns]` - initial build, then rebuild on file changes.
//!
//! Loop:
//!   1. Run `prep::prepare` (discovery + graph merge - incremental;
//!      bootstrap cache-hits when discovery inputs are unchanged).
//!   2. Resolve `--patterns` against the merged graph.
//!   3. Run the initial build.
//!   4. Start the file watcher (excluding `.git/`, `.giant/`, the
//!      cache dir, and every target's declared `outputs:` so build-
//!      written files don't trigger self-rebuilds).
//!   5. Debounce file events (quiet=100ms, max=500ms). On each batch:
//!      a. Re-run `prep::prepare` so newly-discovered targets show up.
//!      b. Compute affected via `selection::affected_targets`.
//!      c. Intersect with the user's pattern selection.
//!      d. If non-empty, build.
//!   6. Loop until Ctrl-C.
//!
//! Builds are never interrupted by new events. Events arriving during
//! a build accumulate; after the build completes, they form the next
//! batch immediately (no wait for the quiet window).

use crate::cli::prep::{self, Prepared};
use crate::events::Event;
use crate::executor::{BuildJob, build};
use crate::model::TargetId;
use crate::paths::AbsPath;
use crate::renderer::{self, ColorChoice, Mode, Renderer};
use crate::selection;
use crate::watcher;
use clap::Args;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Args, Debug)]
pub struct WatchArgs {
    /// Target IDs to watch. Empty = watch all non-test targets.
    #[arg(add = clap_complete::ArgValueCompleter::new(super::dynamic::complete_target_ids))]
    pub patterns: Vec<String>,

    /// Number of parallel jobs for each rebuild (default: number of CPUs).
    #[arg(short = 'j', long)]
    pub jobs: Option<usize>,

    /// Quiet window in ms - flush a batch this long after the last
    /// event in it. Default 100.
    #[arg(long, default_value_t = 100)]
    pub quiet_ms: u64,

    /// Max delay in ms - flush a batch this long after the FIRST event
    /// in it, even if events keep streaming. Default 500.
    #[arg(long, default_value_t = 500)]
    pub max_delay_ms: u64,

    /// Only print failures and the final summary for each cycle.
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// When to colorize output. `auto` honors stdout-is-tty and the
    /// `NO_COLOR` env var.
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,

    /// Include only targets carrying this tag. Repeatable.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// Exclude targets carrying this tag. Repeatable.
    #[arg(long = "no-tag", value_name = "TAG")]
    pub no_tags: Vec<String>,
}

pub async fn execute(args: WatchArgs, global: &super::GlobalFlags) -> anyhow::Result<()> {
    let parallelism = args.jobs.unwrap_or_else(prep::num_cpus_estimate);
    let cancel = CancellationToken::new();
    let mode = renderer::detect_mode(args.color, /* ndjson */ false);
    let quiet = args.quiet;

    // Ctrl-C → cancel.
    {
        let cancel = cancel.clone();
        ctrlc::set_handler(move || {
            cancel.cancel();
        })
        .ok();
    }

    // Initial prepare + build.
    print_note(mode, "initial build");
    let prepared = run_prepare(global, parallelism, cancel.clone()).await?;
    let workspace_root = prepared.workspace_root.clone();

    let select_opts = selection::SelectionOpts {
        tags: args.tags.clone(),
        no_tags: args.no_tags.clone(),
    };
    let pattern_selection = resolve_pattern_selection(&prepared, &args.patterns, &select_opts)?;
    if pattern_selection.is_empty() {
        anyhow::bail!("no targets to watch");
    }

    let initial_outputs = collect_output_paths(&prepared, &workspace_root);
    run_build(
        &prepared,
        &pattern_selection,
        parallelism,
        global.fresh,
        cancel.clone(),
        mode,
        quiet,
    )
    .await?;

    // Now spawn the watcher. Excludes cover .git, .giant, the cache dir,
    // and every declared output path so self-rebuilds don't loop.
    let cache_root_abs = prep::resolve_cache_dir(&prepared.config.cache.dir)?;
    let mut excludes: Vec<PathBuf> = vec![
        workspace_root.as_path().join(".git"),
        workspace_root.as_path().join(".giant"),
        cache_root_abs.clone(),
    ];
    excludes.extend(initial_outputs.iter().cloned());

    let (_handle, mut rx) = watcher::spawn(workspace_root.as_path(), excludes)?;

    print_note(
        mode,
        &format!(
            "watching {} target(s) - Ctrl-C to exit",
            pattern_selection.len()
        ),
    );

    let quiet_window = Duration::from_millis(args.quiet_ms);
    let max = Duration::from_millis(args.max_delay_ms);
    let mut debouncer = Debouncer::new(quiet_window, max);

    loop {
        if cancel.is_cancelled() {
            println!();
            print_note(mode, "cancelled");
            return Ok(());
        }

        let batch = match debouncer.next_batch(&mut rx, &cancel).await {
            Some(batch) => batch,
            None => return Ok(()), // channel closed
        };
        if batch.is_empty() {
            // Empty batches happen when the debouncer wakes on
            // cancellation; loop back and the top-of-loop check exits.
            continue;
        }

        let paths: Vec<PathBuf> = batch
            .into_iter()
            .map(|p| relative_to(&workspace_root, &p))
            .collect();

        // Re-prepare. Discovery bootstrap is itself cached - cache-hits
        // when inputs are unchanged, runs again only when discovery's
        // own inputs (script, deps) changed. New targets discovered in
        // this cycle show up here.
        println!();
        print_note(mode, &format!("{} file(s) changed", paths.len()));
        let prepared = match run_prepare(global, parallelism, cancel.clone()).await {
            Ok(p) => p,
            Err(e) => {
                print_note(mode, &format!("prepare failed: {e}"));
                continue;
            }
        };

        let pattern_selection =
            match resolve_pattern_selection(&prepared, &args.patterns, &select_opts) {
                Ok(s) => s,
                Err(e) => {
                    print_note(mode, &format!("selection failed: {e}"));
                    continue;
                }
            };

        let changed_refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
        let affected = selection::affected_targets(&prepared.graph, &changed_refs);
        let selection: Vec<TargetId> = pattern_selection
            .into_iter()
            .filter(|id| affected.contains(id))
            .collect();

        if selection.is_empty() {
            print_note(mode, "no targets affected");
            continue;
        }

        if let Err(e) = run_build(
            &prepared,
            &selection,
            parallelism,
            global.fresh,
            cancel.clone(),
            mode,
            quiet,
        )
        .await
        {
            print_note(mode, &format!("build failed: {e}"));
        }
    }
}

fn print_note(mode: Mode, msg: &str) {
    print!("{}", renderer::note(&mode.theme(), msg));
}

/// Reusable prepare wrapper. The bootstrap pass needs an event sender;
/// during watch we want quiet bootstraps (cache-hit case is the common
/// one), so we null-sink them.
async fn run_prepare(
    global: &super::GlobalFlags,
    parallelism: usize,
    cancel: CancellationToken,
) -> anyhow::Result<Prepared> {
    let (tx, sink) = prep::null_event_sink();
    let result = prep::prepare(
        global.config.as_deref(),
        parallelism,
        global.fresh,
        tx,
        cancel,
    )
    .await;
    let _ = sink.await;
    result
}

/// Resolve user patterns against the current graph using the shared
/// selection language (TDD-0011). Re-run each watch cycle so newly-
/// discovered targets are included or excluded as the patterns dictate.
fn resolve_pattern_selection(
    prepared: &Prepared,
    patterns: &[String],
    opts: &selection::SelectionOpts,
) -> anyhow::Result<Vec<TargetId>> {
    selection::resolve_patterns(
        &prepared.graph,
        patterns,
        selection::TestMode::Exclude,
        opts,
    )
    .map_err(Into::into)
}

/// All declared output absolute paths, used as the watcher exclusion
/// set so the engine doesn't see its own writes.
fn collect_output_paths(prepared: &Prepared, workspace_root: &AbsPath) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    for (_, spec) in prepared.graph.iter() {
        for o in &spec.outputs {
            out.insert(workspace_root.as_path().join(o.as_path()));
        }
    }
    out
}

/// Run one build, blocking until it finishes (or cancellation). Same
/// renderer wiring as `giant build` so users see consistent output.
async fn run_build(
    prepared: &Prepared,
    selection: &[TargetId],
    parallelism: usize,
    fresh: bool,
    cancel: CancellationToken,
    mode: Mode,
    quiet: bool,
) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::channel::<Event>(1024);
    let renderer_task = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut out = tokio::io::stdout();
        let mut r = Renderer::new(mode, 0, quiet);
        while let Some(ev) = rx.recv().await {
            if let Some(line) = r.render(&ev) {
                let _ = out.write_all(line.as_bytes()).await;
                let _ = out.flush().await;
            }
        }
    });

    let (remote, upload_tx, upload_handle) = prep::open_remote(&prepared.config)?;

    let job = BuildJob {
        graph: Arc::new(prepared.graph.clone()),
        selection: selection.to_vec(),
        cache: prepared.cache.clone(),
        workspace_root: prepared.workspace_root.clone(),
        parallelism,
        fresh,
        events: tx,
        cancel,
        build_id: format!("watch_{}", prep::short_random()),
        #[cfg(feature = "remote")]
        remote,
        #[cfg(feature = "remote")]
        upload_tx: upload_tx.clone(),
    };
    #[cfg(not(feature = "remote"))]
    let _ = (remote, upload_tx);
    let summary = build(job).await?;

    #[cfg(feature = "remote")]
    {
        drop(upload_tx);
        if let Some(h) = upload_handle {
            let _ = h.await;
        }
    }
    #[cfg(not(feature = "remote"))]
    let _ = upload_handle;

    let _ = renderer_task.await;

    // The renderer already emits the failed-target list in its
    // summary block; the watch loop just needs a short message to log.
    if summary.counts.failed > 0 {
        anyhow::bail!("{} target(s) failed", summary.counts.failed);
    }
    Ok(())
}

fn relative_to(workspace_root: &AbsPath, p: &Path) -> PathBuf {
    p.strip_prefix(workspace_root.as_path())
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| p.to_path_buf())
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
