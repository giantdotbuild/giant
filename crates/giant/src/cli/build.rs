//! `giant build` subcommand.

use crate::events::Event;
use crate::git;
use crate::model::TargetId;
use crate::renderer::{self, ColorChoice, Renderer};
use crate::selection;
use clap::Args;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use super::prep;

#[derive(Args, Debug)]
pub struct BuildArgs {
    /// Target IDs to build. Empty = build all non-test targets.
    #[arg(add = clap_complete::ArgValueCompleter::new(super::dynamic::complete_target_ids))]
    pub patterns: Vec<String>,

    /// Number of parallel jobs (default: number of CPUs).
    #[arg(short = 'j', long)]
    pub jobs: Option<usize>,

    /// Emit NDJSON events on stdout. (`--events ndjson` is the only form
    /// in v1; the option is shaped so other formats can be added later.)
    #[arg(long, value_name = "FORMAT")]
    pub events: Option<EventsFormat>,

    /// Build only targets affected by changes. Requires `--base` or
    /// `--file`. If both are given, `--file` wins.
    #[arg(long)]
    pub affected: bool,

    /// Git ref used as the diff baseline. Affected files = everything
    /// changed in the worktree (committed + uncommitted) relative to this
    /// ref, plus untracked-but-not-gitignored files.
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,

    /// Explicit changed-file list. Repeatable; overrides `--base`. Useful
    /// in CI where the file list comes from elsewhere and you don't want
    /// to invoke git.
    #[arg(long, value_name = "PATH")]
    pub file: Vec<PathBuf>,

    /// Only print failures and the final summary. Useful in CI.
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// When to colorize output. `auto` honors stdout-is-tty and the
    /// `NO_COLOR` env var.
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,

    /// Include only targets carrying this tag. Repeatable. Multiple
    /// values are unioned: `--tag release --tag linux` selects targets
    /// tagged `release` OR `linux`.
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// Exclude targets carrying this tag. Repeatable. Composes with
    /// `--tag` so `--tag release --no-tag flaky` means "release AND
    /// NOT flaky".
    #[arg(long = "no-tag", value_name = "TAG")]
    pub no_tags: Vec<String>,

    /// Show `toolchain`-tagged targets in the output. They are folded
    /// out by default so the view stays focused on build targets; they
    /// still build, and failures always surface. (TDD-0017.)
    #[arg(long)]
    pub show_toolchains: bool,

    /// Rebuild the affected subset of the selection when files change,
    /// until interrupted (Ctrl-C). The watch flags below only apply
    /// together with `--watch`.
    #[arg(long)]
    pub watch: bool,

    /// Include `test: true` targets in the selection. `build` excludes
    /// them by default; `test` already selects only tests, so this is a
    /// no-op there. With `--watch` this is the "watch everything" case.
    #[arg(long)]
    pub with_tests: bool,

    /// Quiet window in ms: flush a change batch this long after the last
    /// event in it. Default 100. Only meaningful with `--watch`.
    #[arg(long, default_value_t = 100)]
    pub quiet_ms: u64,

    /// Max delay in ms: flush a batch this long after the first event in
    /// it, even if events keep streaming. Default 500. With `--watch`.
    #[arg(long, default_value_t = 500)]
    pub max_delay_ms: u64,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum EventsFormat {
    Ndjson,
}

pub async fn execute(args: BuildArgs, global: &super::GlobalFlags) -> anyhow::Result<()> {
    execute_with_mode(args, global, selection::TestMode::Exclude).await
}

/// Shared core for `giant build` and `giant test`. The only difference
/// between them is how `test: true` targets are treated.
pub(super) async fn execute_with_mode(
    args: BuildArgs,
    global: &super::GlobalFlags,
    base_mode: selection::TestMode,
) -> anyhow::Result<()> {
    // `--with-tests` widens the `build` selection to include tests;
    // `test` is already `Only`, so it stays.
    let test_mode = match base_mode {
        selection::TestMode::Exclude if args.with_tests => selection::TestMode::Include,
        m => m,
    };

    // Event channel + renderer, set up before `prep::prepare`. id_width
    // starts at 0 and gets updated once we have a selection.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(1024);
    let ndjson = matches!(args.events, Some(EventsFormat::Ndjson));
    let mode = renderer::detect_mode(args.color, ndjson);
    let quiet = args.quiet;
    // Toolchain targets are folded out of the human view. The set is
    // shared with the renderer task and filled in once the graph is
    // loaded (below) - it can't be known at construction.
    let hidden: Arc<Mutex<HashSet<TargetId>>> = Arc::new(Mutex::new(HashSet::new()));
    let hidden_for_render = hidden.clone();
    // The renderer also captures the main build's `build.finished`
    // counts so the caller can set the exit code - in the unified path
    // (TDD-0021) the build runs inside the engine and we no longer get
    // its return value, only the event stream.
    let renderer_task = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut out = tokio::io::stdout();
        // id_width starts at 0; it gets recomputed inside the renderer
        // once `BuildStarted` arrives with the full target list.
        let mut r = Renderer::new(mode, 0, quiet);
        r.set_hidden(hidden_for_render);
        let mut final_counts: Option<crate::events::TargetCounts> = None;
        // Heartbeat: every second we ask the renderer if any
        // currently-running targets have been quiet long enough to
        // warrant a "still running" line. The renderer itself decides
        // (HEARTBEAT_AFTER threshold).
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

    let cancel = CancellationToken::new();
    let parallelism = args.jobs.unwrap_or_else(prep::num_cpus_estimate);

    // Load config, build the graph, open cache.
    let prepared = match prep::prepare(global.config.as_deref()).await {
        Ok(p) => p,
        Err(e) => {
            drop(tx);
            let _ = renderer_task.await;
            return Err(e);
        }
    };

    // Fold `toolchain`-tagged targets out of the human view now that the
    // graph is known. They still build; failures still surface (TDD-0017).
    if !args.show_toolchains {
        *hidden.lock().expect("hidden set mutex") = prepared.graph.ids_with_tag("toolchain");
    }

    // Resolve selection over the merged graph: positional patterns →
    // optional --affected filter → final list. The pattern language
    // (globs + exclusions, tags, test mode - TDD-0011) lives in
    // `selection`. test_mode comes from the caller so `giant build`
    // and `giant test` share this code with one flag flipped.
    let opts = selection::SelectionOpts {
        tags: args.tags.clone(),
        no_tags: args.no_tags.clone(),
    };
    // `failed-last`: re-select the targets that failed in the most recent
    // build (TDD-0011), recorded under the state dir. Resolved here rather
    // than in `resolve_patterns` (which is graph-pure) since it reads state.
    let pattern_selection: Vec<TargetId> = if args.patterns.len() == 1
        && args.patterns[0] == "failed-last"
    {
        let path = prep::last_failures_path(
            prepared.workspace_root.as_path(),
            &prepared.config.state.dir,
        );
        let failed: Vec<TargetId> = prep::read_last_failures(&path)
            .into_iter()
            .filter(|id| prepared.graph.get(id).is_some())
            .collect();
        if failed.is_empty() {
            drop(tx);
            let _ = renderer_task.await;
            anyhow::bail!("no recent failures recorded - run a build first, then `failed-last`");
        }
        failed
    } else {
        match selection::resolve_patterns(&prepared.graph, &args.patterns, test_mode, &opts) {
            Ok(v) => v,
            Err(e) => {
                drop(tx);
                let _ = renderer_task.await;
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
            drop(tx);
            let _ = renderer_task.await;
            print_note(mode, "no affected targets");
            return Ok(());
        }
        intersected
    } else {
        pattern_selection
    };

    if selection.is_empty() {
        drop(tx);
        let _ = renderer_task.await;
        print_note(mode, "no targets to build");
        return Ok(());
    }

    // Watch: rebuild the affected subset on change, through the engine's
    // `watch.start` - the same loop the stdio session runs (TDD-0021).
    // Runs until Ctrl-C. The renderer turns `watch.affected` events into
    // the per-cycle notes.
    if args.watch {
        print_note(
            mode,
            &format!("watching {} target(s) - Ctrl-C to exit", selection.len()),
        );
        super::session::run_watch_command(
            prepared,
            tx,
            global.config.clone(),
            parallelism,
            selection,
            global.fresh,
        )
        .await?;
        let _ = renderer_task.await;
        print_note(mode, "cancelled");
        return Ok(());
    }

    // Keep cache handles for post-build eviction; `prepared` is consumed
    // by the engine adapter below.
    let cache_for_evict = prepared.cache.clone();
    let cache_cfg = prepared.config.cache.clone();

    // Dispatch the build through the engine - the same `Command::Build`
    // path the stdio session uses (TDD-0021). Events flow to our renderer
    // via `tx`; the renderer captures the build's pass/fail counts.
    drop(cancel);
    super::session::run_one_build(
        prepared,
        tx,
        global.config.clone(),
        parallelism,
        selection,
        global.fresh,
    )
    .await?;

    let counts = renderer_task.await.ok().flatten().unwrap_or_default();

    // Post-build cache eviction (TDD-0012). Silent: runs only if the
    // local cache is over its size limit. Synchronous because the CLI
    // exits after this call.
    if counts.failed == 0 {
        let _ = maybe_evict(&cache_for_evict, &cache_cfg).await;
    }

    // The renderer already printed the failed-target list; just request a
    // non-zero exit, no extra banner.
    if counts.failed > 0 {
        return Err(super::SilentExit.into());
    }
    Ok(())
}

/// If the cache is over its configured trigger, evict down to the
/// configured target. No-op when `max_size_gb` is unset (eviction
/// disabled) or the cache is comfortably under the trigger.
async fn maybe_evict(
    cache: &crate::cache::LocalCache,
    cfg: &crate::config::CacheConfig,
) -> Result<(), crate::cache::CacheError> {
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
    // 5-minute recency buffer per TDD-0012; protects in-flight builds
    // in other terminals from having their AC entries evicted.
    let _ = cache
        .evict_to(target, std::time::Duration::from_secs(5 * 60))
        .await?;
    Ok(())
}

/// One-off informational line outside the event stream - uses the same
/// theme the renderer would have used so visual style is consistent.
fn print_note(mode: renderer::Mode, msg: &str) {
    print!("{}", renderer::note(&mode.theme(), msg));
}

/// Resolve the list of changed files from `--file` (explicit) or `--base`
/// (git diff). Returns workspace-relative paths.
pub(super) fn resolve_changed_files(
    args: &BuildArgs,
    workspace_root: &Path,
) -> anyhow::Result<Vec<PathBuf>> {
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
