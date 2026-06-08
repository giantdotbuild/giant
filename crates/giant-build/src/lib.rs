//! Shared core for the `giant build` / `giant test` / `giant verify` porcelains
//! (ADR-0034 phase B). Each bin is a thin `main` that parses [`BuildArgs`] and
//! calls [`run`] with a test selection mode; `verify` additionally forces the
//! sandbox and a fresh build.
//!
//! The build runs in-process through the engine's adapter (`giant::run_one_build`
//! / `giant::run_watch_command`); this crate owns the args, the selection glue,
//! and the tty renderer that consumes the event stream.

pub mod renderer;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::Args;
use giant::events::Event;
use giant::selection::{self, TestMode};
use giant::{TargetId, git};
use renderer::{ColorChoice, Renderer};

#[derive(Args, Debug)]
pub struct BuildArgs {
    /// Target IDs to build. Empty = build all non-test targets.
    pub patterns: Vec<String>,

    /// Path to giant.yaml / giant.json. Defaults to walking up from cwd.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Force a fresh build (bypass cache).
    #[arg(long)]
    pub fresh: bool,

    /// Enforce declared inputs/outputs by running each eligible target through
    /// the `giant-sandbox` helper (Linux only; ADR-0030). Off by default.
    #[arg(long)]
    pub sandbox: bool,

    /// Number of parallel jobs (default: number of CPUs).
    #[arg(short = 'j', long)]
    pub jobs: Option<usize>,

    /// Emit NDJSON events on stdout instead of the human renderer.
    #[arg(long, value_name = "FORMAT")]
    pub events: Option<EventsFormat>,

    /// Build only targets affected by changes. Requires `--base` or `--file`.
    #[arg(long)]
    pub affected: bool,

    /// Git ref used as the diff baseline.
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,

    /// Explicit changed-file list. Repeatable; overrides `--base`.
    #[arg(long, value_name = "PATH")]
    pub file: Vec<PathBuf>,

    /// Only print failures and the final summary. Useful in CI.
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// When to colorize output. `auto` honors stdout-is-tty and `NO_COLOR`.
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,

    /// Include only targets carrying this tag. Repeatable; unioned.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// Exclude targets carrying this tag. Repeatable.
    #[arg(long = "no-tag", value_name = "TAG")]
    pub no_tags: Vec<String>,

    /// Show `toolchain`-tagged targets in the output (folded out by default).
    #[arg(long)]
    pub show_toolchains: bool,

    /// Rebuild the affected subset of the selection on file change, until
    /// interrupted (Ctrl-C).
    #[arg(long)]
    pub watch: bool,

    /// Include `test: true` targets in the selection (`build` excludes them by
    /// default; no-op for `test`).
    #[arg(long)]
    pub with_tests: bool,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum EventsFormat {
    Ndjson,
}

/// Run a build with the given base test-selection mode. Returns the process exit
/// code (0 = success, 1 = one or more targets failed). Setup problems are `Err`.
pub async fn run(args: BuildArgs, base_mode: TestMode) -> anyhow::Result<i32> {
    // `--with-tests` widens the `build` selection to include tests; `test` is
    // already `Only`, so it stays.
    let test_mode = match base_mode {
        TestMode::Exclude if args.with_tests => TestMode::Include,
        m => m,
    };

    // Event channel + renderer, set up before `prepare`. id_width starts at 0
    // and gets recomputed inside the renderer once `BuildStarted` arrives.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(1024);
    let ndjson = matches!(args.events, Some(EventsFormat::Ndjson));
    let mode = renderer::detect_mode(args.color, ndjson);
    let quiet = args.quiet;
    let hidden: Arc<Mutex<HashSet<TargetId>>> = Arc::new(Mutex::new(HashSet::new()));
    let hidden_for_render = hidden.clone();
    let renderer_task = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut out = tokio::io::stdout();
        let mut r = Renderer::new(mode, 0, quiet);
        r.set_hidden(hidden_for_render);
        let mut final_counts: Option<giant::events::TargetCounts> = None;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    match ev {
                        Some(ev) => {
                            if let Event::BuildFinished { counts, .. } = &ev {
                                final_counts = Some(counts.clone());
                            }
                            if let Some(line) = r.render(&ev) {
                                let _ = out.write_all(line.as_bytes()).await;
                                let _ = out.flush().await;
                            }
                        }
                        None => break,
                    }
                }
                _ = tick.tick() => {
                    if let Some(line) = r.heartbeat() {
                        let _ = out.write_all(line.as_bytes()).await;
                        let _ = out.flush().await;
                    }
                }
            }
        }
        final_counts
    });

    // Drain and join the renderer before bailing out early, so any buffered
    // event line is flushed and the task doesn't outlive `run`.
    let teardown = |tx, task: tokio::task::JoinHandle<_>| async move {
        drop(tx);
        let _ = task.await;
    };

    let parallelism = args.jobs.unwrap_or_else(giant::num_cpus_estimate);

    let prepared = match giant::prepare(args.config.as_deref()).await {
        Ok(p) => p,
        Err(e) => {
            teardown(tx, renderer_task).await;
            return Err(e);
        }
    };

    // Fold `toolchain`-tagged targets out of the human view (TDD-0017).
    if !args.show_toolchains {
        *hidden.lock().expect("hidden set mutex") = prepared.graph.ids_with_tag("toolchain");
    }

    let opts = selection::SelectionOpts {
        tags: args.tags.clone(),
        no_tags: args.no_tags.clone(),
    };
    // `failed-last`: re-select the targets that failed in the most recent build.
    let pattern_selection: Vec<TargetId> = if args.patterns.len() == 1
        && args.patterns[0] == "failed-last"
    {
        let path = giant::last_failures_path(
            prepared.workspace_root.as_path(),
            &prepared.config.state.dir,
        );
        let failed: Vec<TargetId> = giant::read_last_failures(&path)
            .into_iter()
            .filter(|id| prepared.graph.get(id).is_some())
            .collect();
        if failed.is_empty() {
            teardown(tx, renderer_task).await;
            anyhow::bail!("no recent failures recorded - run a build first, then `failed-last`");
        }
        failed
    } else {
        match selection::resolve_patterns(&prepared.graph, &args.patterns, test_mode, &opts) {
            Ok(v) => v,
            Err(e) => {
                teardown(tx, renderer_task).await;
                return Err(e.into());
            }
        }
    };

    let selection = if args.affected {
        let changed = resolve_changed_files(&args, prepared.workspace_root.as_path())?;
        let changed_refs: Vec<&Path> = changed.iter().map(|p| p.as_path()).collect();
        let affected = selection::affected_targets(&prepared.graph, &changed_refs);
        let intersected: Vec<TargetId> = pattern_selection
            .into_iter()
            .filter(|id| affected.contains(id))
            .collect();
        if intersected.is_empty() {
            teardown(tx, renderer_task).await;
            print_note(mode, "no affected targets");
            return Ok(0);
        }
        intersected
    } else {
        pattern_selection
    };

    if selection.is_empty() {
        teardown(tx, renderer_task).await;
        print_note(mode, "no targets to build");
        return Ok(0);
    }

    // Resolve `--sandbox` once: errors here (no helper / non-Linux) fail the
    // build before any target runs, never a silent unsandboxed fallback.
    let sandbox = giant::resolve_sandbox(
        args.sandbox,
        &prepared.config.sandbox,
        prepared.workspace_root.as_path(),
    )?;

    // Watch: rebuild the affected subset on change, through the engine's
    // watch loop. Runs until Ctrl-C.
    if args.watch {
        print_note(
            mode,
            &format!("watching {} target(s) - Ctrl-C to exit", selection.len()),
        );
        giant::run_watch_command(
            prepared,
            tx,
            args.config.clone(),
            parallelism,
            selection,
            args.fresh,
            sandbox,
        )
        .await?;
        let _ = renderer_task.await;
        print_note(mode, "cancelled");
        return Ok(0);
    }

    // Keep cache handles for post-build eviction; `prepared` is consumed below.
    let cache_for_evict = prepared.cache.clone();
    let cache_cfg = prepared.config.cache.clone();

    giant::run_one_build(
        prepared,
        tx,
        args.config.clone(),
        parallelism,
        selection,
        args.fresh,
        sandbox,
    )
    .await?;

    let counts = renderer_task.await.ok().flatten().unwrap_or_default();

    // Post-build cache eviction (TDD-0012). Silent; only if over the limit.
    if counts.failed == 0 {
        let _ = maybe_evict(&cache_for_evict, &cache_cfg).await;
    }

    // The renderer already printed the failed-target list; just request a
    // non-zero exit, no extra banner.
    Ok(if counts.failed > 0 { 1 } else { 0 })
}

/// If the cache is over its configured trigger, evict down to the configured
/// target. No-op when `max_size_gb` is unset or the cache is under the trigger.
async fn maybe_evict(
    cache: &giant::LocalCache,
    cfg: &giant::config::CacheConfig,
) -> Result<(), giant::cache::CacheError> {
    let Some(max_gb) = cfg.max_size_gb else {
        return Ok(());
    };
    if max_gb == 0 {
        return Ok(());
    }
    let max_bytes = max_gb.saturating_mul(1024 * 1024 * 1024);
    let trigger = max_bytes.saturating_mul(cfg.evict_when_above_pct as u64) / 100;
    let target = max_bytes.saturating_mul(cfg.evict_target_pct as u64) / 100;
    let current = cache.total_size().await?;
    if current <= trigger {
        return Ok(());
    }
    // 5-minute recency buffer per TDD-0012; protects in-flight builds elsewhere.
    let _ = cache
        .evict_to(target, std::time::Duration::from_secs(5 * 60))
        .await?;
    Ok(())
}

/// One-off informational line outside the event stream - uses the renderer's
/// theme so visual style stays consistent.
fn print_note(mode: renderer::Mode, msg: &str) {
    print!("{}", renderer::note(&mode.theme(), msg));
}

/// Resolve the list of changed files from `--file` (explicit) or `--base` (git
/// diff). Returns workspace-relative paths.
fn resolve_changed_files(args: &BuildArgs, workspace_root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !args.file.is_empty() {
        return Ok(args
            .file
            .iter()
            .map(|p| {
                p.strip_prefix(workspace_root)
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|_| p.clone())
            })
            .collect());
    }
    let base = args.base.as_deref().ok_or_else(|| {
        anyhow::anyhow!("--affected requires --base <ref> or one or more --file <path>")
    })?;
    git::affected_files_since(workspace_root, base)
        .map_err(|e| anyhow::anyhow!("affected detection: {e}"))
}
