//! `build --watch` / `test --watch` - initial build, then rebuild the
//! affected subset of the selection when files change (ADR-0023).
//!
//! This module owns the CLI watch loop plus the shared watch *mechanics*
//! (`standard_excludes`, `Debouncer`, `next_affected_cycle`) that the
//! engine session's `watch.start` also uses. The graph is prepared once;
//! each change batch rebuilds only the affected targets in the selection.
//! Ctrl-C exits.
//!
//! A `giant.yaml` edit mid-watch is not picked up (the graph is fixed for
//! the watch's lifetime - restart to re-discover); this mirrors the engine
//! session, which reloads config through its own `config.reload` path.

use crate::cli::build::BuildArgs;
use crate::cli::prep::{self, Prepared};
use crate::events::Event;
use crate::executor::{BuildJob, build};
use crate::model::TargetId;
use crate::paths::AbsPath;
use crate::renderer::{self, Mode, Renderer};
use crate::selection;
use crate::watcher;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Run `build`/`test` in watch mode: build `selection` once, then rebuild
/// the affected subset on each debounced change until Ctrl-C. Called from
/// `build::execute_with_mode` when `--watch` is set; `test_mode` carries
/// the build-vs-test distinction (and `--with-tests`).
pub(super) async fn run_watch(
    args: &BuildArgs,
    global: &super::GlobalFlags,
    test_mode: selection::TestMode,
) -> anyhow::Result<()> {
    let parallelism = args.jobs.unwrap_or_else(prep::num_cpus_estimate);
    let cancel = CancellationToken::new();
    let mode = renderer::detect_mode(args.color, /* ndjson */ false);
    let render = RenderOpts {
        mode,
        quiet: args.quiet,
        show_toolchains: args.show_toolchains,
    };

    // Ctrl-C → cancel the loop (and any in-flight rebuild).
    {
        let cancel = cancel.clone();
        ctrlc::set_handler(move || cancel.cancel()).ok();
    }

    // Prepare + select once; the loop rebuilds affected targets on this
    // graph until interrupted.
    print_note(mode, "initial build");
    let prepared = run_prepare(global, parallelism, cancel.clone()).await?;
    let workspace_root = prepared.workspace_root.clone();

    let select_opts = selection::SelectionOpts {
        tags: args.tags.clone(),
        no_tags: args.no_tags.clone(),
    };
    let pattern_selection =
        resolve_pattern_selection(&prepared, &args.patterns, test_mode, &select_opts)?;

    // `--affected` scopes the watch to the targets changed since the
    // baseline; the loop then rebuilds among those on live changes.
    let selection = if args.affected {
        let changed = super::build::resolve_changed_files(args, workspace_root.as_path())?;
        let refs: Vec<&Path> = changed.iter().map(|p| p.as_path()).collect();
        let affected = selection::affected_targets(&prepared.graph, &refs);
        pattern_selection
            .into_iter()
            .filter(|id| affected.contains(id))
            .collect()
    } else {
        pattern_selection
    };
    if selection.is_empty() {
        anyhow::bail!("no targets to watch");
    }

    run_build(
        &prepared,
        &selection,
        parallelism,
        global.fresh,
        cancel.clone(),
        render,
    )
    .await?;

    // Watcher: exclude .git, the state dir, the cache dir, and declared
    // outputs so the engine's own writes don't loop.
    let cache_root = AbsPath::new(prep::resolve_cache_dir(&prepared.config.cache.dir)?);
    let state_dir = PathBuf::from(&prepared.config.state.dir);
    let excludes = standard_excludes(&workspace_root, &cache_root, &state_dir, &prepared.graph);
    let (_handle, mut rx) = watcher::spawn(workspace_root.as_path(), excludes)?;

    print_note(
        mode,
        &format!("watching {} target(s) - Ctrl-C to exit", selection.len()),
    );

    let mut debouncer = Debouncer::new(
        Duration::from_millis(args.quiet_ms),
        Duration::from_millis(args.max_delay_ms),
    );
    while let Some(to_build) = next_affected_cycle(
        &prepared.graph,
        &mut rx,
        &mut debouncer,
        &selection,
        &workspace_root,
        &cancel,
    )
    .await
    {
        println!();
        if to_build.is_empty() {
            print_note(mode, "no targets affected");
            continue;
        }
        print_note(
            mode,
            &format!("{} target(s) affected, rebuilding", to_build.len()),
        );
        if let Err(e) = run_build(
            &prepared,
            &to_build,
            parallelism,
            global.fresh,
            cancel.clone(),
            render,
        )
        .await
        {
            print_note(mode, &format!("build failed: {e}"));
        }
    }

    if cancel.is_cancelled() {
        println!();
        print_note(mode, "cancelled");
    }
    Ok(())
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
/// selection language (TDD-0011).
fn resolve_pattern_selection(
    prepared: &Prepared,
    patterns: &[String],
    test_mode: selection::TestMode,
    opts: &selection::SelectionOpts,
) -> anyhow::Result<Vec<TargetId>> {
    selection::resolve_patterns(&prepared.graph, patterns, test_mode, opts).map_err(Into::into)
}

/// How a watch rebuild renders. Bundled to keep `run_build` under the
/// positional-arg lint and to pass display flags as one unit.
#[derive(Clone, Copy)]
struct RenderOpts {
    mode: Mode,
    quiet: bool,
    show_toolchains: bool,
}

/// Run one build, blocking until it finishes (or cancellation). Same
/// renderer wiring as `giant build` so users see consistent output. A
/// fresh renderer per cycle gives each rebuild its own clean block.
async fn run_build(
    prepared: &Prepared,
    selection: &[TargetId],
    parallelism: usize,
    fresh: bool,
    cancel: CancellationToken,
    render: RenderOpts,
) -> anyhow::Result<()> {
    let RenderOpts {
        mode,
        quiet,
        show_toolchains,
    } = render;
    let (tx, mut rx) = mpsc::channel::<Event>(1024);
    // Fold `toolchain`-tagged targets out of the human view (TDD-0017).
    let hidden: Arc<Mutex<HashSet<TargetId>>> = Arc::new(Mutex::new(if show_toolchains {
        HashSet::new()
    } else {
        prepared.graph.ids_with_tag("toolchain")
    }));
    let renderer_task = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut out = tokio::io::stdout();
        let mut r = Renderer::new(mode, 0, quiet);
        r.set_hidden(hidden);
        while let Some(ev) = rx.recv().await {
            if let Some(line) = r.render(&ev) {
                let _ = out.write_all(line.as_bytes()).await;
                let _ = out.flush().await;
            }
        }
    });

    let (remote, upload_tx, upload_handle) = prep::open_remote(&prepared.config)?;
    let log_capture = crate::executor::LogCapture::from_cache_config(&prepared.config.cache);

    let job = BuildJob {
        graph: Arc::new(prepared.graph.clone()),
        selection: selection.to_vec(),
        cache: prepared.cache.clone(),
        workspace_root: prepared.workspace_root.clone(),
        parallelism,
        fresh,
        force_fresh: None,
        events: tx,
        cancel,
        build_id: format!("watch_{}", prep::short_random()),
        log_capture,
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

    // The renderer already emits the failed-target list in its summary
    // block; the watch loop just needs a short message to log.
    if summary.counts.failed > 0 {
        anyhow::bail!("{} target(s) failed", summary.counts.failed);
    }
    Ok(())
}

/// The standard watcher exclude set: `.git`, the configured state dir,
/// the cache dir, and every declared output path - so the engine's own
/// writes don't loop the watcher. Shared by `build --watch` and the
/// session-mode watch / affected / change-subscription loops.
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
