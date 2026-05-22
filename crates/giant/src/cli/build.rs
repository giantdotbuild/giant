//! `giant build` subcommand.

use crate::events::Event;
use crate::executor::{BuildJob, build};
use crate::git;
use crate::model::TargetId;
use crate::renderer::{self, ColorChoice, Renderer};
use crate::selection;
use clap::Args;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    test_mode: selection::TestMode,
) -> anyhow::Result<()> {
    // Event channel + renderer. Used by both the bootstrap and the main
    // build, so we set it up before calling `prep::prepare`. id_width
    // starts at 0 and gets updated once we have a selection; bootstrap
    // log lines render with width=0 (just the prefix), which is fine.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(1024);
    let ndjson = matches!(args.events, Some(EventsFormat::Ndjson));
    let mode = renderer::detect_mode(args.color, ndjson);
    let quiet = args.quiet;
    let renderer_task = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut out = tokio::io::stdout();
        // id_width starts at 0; it gets recomputed inside the renderer
        // once `BuildStarted` arrives with the full target list.
        let mut r = Renderer::new(mode, 0, quiet);
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
    });

    let cancel = CancellationToken::new();
    let parallelism = args.jobs.unwrap_or_else(prep::num_cpus_estimate);

    // Load config, open cache, run discovery bootstrap, merge graph.
    let prepared = match prep::prepare(
        global.config.as_deref(),
        parallelism,
        global.fresh,
        tx.clone(),
        cancel.clone(),
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            drop(tx);
            let _ = renderer_task.await;
            return Err(e);
        }
    };

    // Resolve selection over the merged graph: positional patterns →
    // optional --affected filter → final list. The pattern language
    // (globs + exclusions, tags, test mode - TDD-0011) lives in
    // `selection`. test_mode comes from the caller so `giant build`
    // and `giant test` share this code with one flag flipped.
    let opts = selection::SelectionOpts {
        tags: args.tags.clone(),
        no_tags: args.no_tags.clone(),
    };
    let pattern_selection: Vec<TargetId> =
        match selection::resolve_patterns(&prepared.graph, &args.patterns, test_mode, &opts) {
            Ok(v) => v,
            Err(e) => {
                drop(tx);
                let _ = renderer_task.await;
                return Err(e.into());
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

    let build_id = format!("b_{}", prep::short_random());

    let (remote, upload_tx, upload_handle) = prep::open_remote(&prepared.config)?;

    // Keep a handle to the cache so we can run post-build eviction
    // after BuildJob consumes its copy.
    let cache_for_evict = prepared.cache.clone();
    let cache_cfg = prepared.config.cache.clone();
    let log_capture = crate::executor::LogCapture::from_cache_config(&cache_cfg);

    let job = BuildJob {
        graph: Arc::new(prepared.graph),
        selection,
        cache: prepared.cache,
        workspace_root: prepared.workspace_root,
        parallelism,
        fresh: global.fresh,
        force_fresh: None,
        events: tx,
        cancel,
        build_id,
        log_capture,
        #[cfg(feature = "remote")]
        remote,
        #[cfg(feature = "remote")]
        upload_tx: upload_tx.clone(),
    };
    #[cfg(not(feature = "remote"))]
    let _ = (remote, upload_tx);
    let summary = build(job).await?;

    // Drop the upload sender so the background task drains, then wait
    // for it (bounded by reqwest's internal timeouts). Local + remote
    // caches now reflect the build state.
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

    // Post-build cache eviction (TDD-0012). Silent: runs only if the
    // local cache is over its size limit. Synchronous because the CLI
    // exits after this call; we don't have a long-lived runtime to
    // hand the work to.
    if summary.counts.failed == 0 {
        let _ = maybe_evict(&cache_for_evict, &cache_cfg).await;
    }

    // The renderer already prints the failed-target list in the
    // summary block; just request a non-zero exit, no extra banner.
    if summary.counts.failed > 0 {
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
