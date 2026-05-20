//! `giant build` subcommand.

use crate::events::Event;
use crate::executor::{BuildJob, build};
use crate::git;
use crate::model::TargetId;
use crate::selection;
use clap::Args;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::prep;

#[derive(Args, Debug)]
pub struct BuildArgs {
    /// Target IDs to build. Empty = build all non-test targets.
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
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum EventsFormat {
    Ndjson,
}

pub async fn execute(args: BuildArgs, global: &super::GlobalFlags) -> anyhow::Result<()> {
    // Event channel + renderer. Used by both the bootstrap and the main
    // build, so we set it up before calling `prep::prepare`.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(1024);
    let ndjson = matches!(args.events, Some(EventsFormat::Ndjson));
    let renderer = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut out = tokio::io::stdout();
        while let Some(ev) = rx.recv().await {
            let line = if ndjson {
                serde_json::to_string(&ev).unwrap_or_default() + "\n"
            } else {
                render_plain(&ev)
            };
            if !line.is_empty() {
                let _ = out.write_all(line.as_bytes()).await;
                let _ = out.flush().await;
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
            let _ = renderer.await;
            return Err(e);
        }
    };

    // Resolve selection over the merged graph: positional patterns →
    // optional --affected filter → final list.
    let pattern_selection: Vec<TargetId> = if args.patterns.is_empty() {
        prepared
            .graph
            .iter()
            .filter(|(_, spec)| !spec.test)
            .map(|(id, _)| id.clone())
            .collect()
    } else {
        let mut out = Vec::new();
        for p in &args.patterns {
            let exact = TargetId::new(p);
            if prepared.graph.get(&exact).is_some() {
                out.push(exact);
                continue;
            }
            drop(tx);
            let _ = renderer.await;
            anyhow::bail!("no target matches {p:?} (selection-language is v1.1)");
        }
        out
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
            let _ = renderer.await;
            eprintln!("no affected targets");
            return Ok(());
        }
        intersected
    } else {
        pattern_selection
    };

    if selection.is_empty() {
        drop(tx);
        let _ = renderer.await;
        anyhow::bail!("no targets to build");
    }

    let build_id = format!("b_{}", prep::short_random());

    let (remote, upload_tx, upload_handle) = prep::open_remote(&prepared.config)?;

    let job = BuildJob {
        graph: Arc::new(prepared.graph),
        selection,
        cache: prepared.cache,
        workspace_root: prepared.workspace_root,
        parallelism,
        fresh: global.fresh,
        events: tx,
        cancel,
        build_id,
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

    let _ = renderer.await;

    if summary.counts.failed > 0 {
        anyhow::bail!(
            "{} target(s) failed: {}",
            summary.counts.failed,
            summary
                .failed_targets
                .iter()
                .map(|t| t.as_str().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

fn render_plain(ev: &Event) -> String {
    use crate::events::TargetResultKind;
    match ev {
        Event::TargetFinished {
            id,
            result,
            duration_ms,
            error,
            ..
        } => {
            let label = match result {
                TargetResultKind::Built => "built",
                TargetResultKind::CacheHit => "cache",
                TargetResultKind::RemoteCacheHit => "remote",
                TargetResultKind::ExternalCacheHit => "external",
                TargetResultKind::Skipped => "skipped",
                TargetResultKind::Failed => "FAILED",
            };
            if let Some(e) = error {
                format!("{label:>8}  {id}  ({duration_ms}ms) - {e}\n")
            } else {
                format!("{label:>8}  {id}  ({duration_ms}ms)\n")
            }
        }
        Event::TargetLog {
            id, stream, line, ..
        } => {
            let s = match stream {
                crate::events::LogStream::Stdout => "out",
                crate::events::LogStream::Stderr => "err",
            };
            format!("{id} | {s} | {line}\n")
        }
        Event::BuildFinished {
            ok,
            duration_ms,
            counts,
            ..
        } => {
            format!(
                "{} {} built, {} cached, {} failed, {} skipped in {}ms\n",
                if *ok { "OK" } else { "FAIL" },
                counts.built,
                counts.cache_hit,
                counts.failed,
                counts.skipped,
                duration_ms
            )
        }
        _ => String::new(),
    }
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
