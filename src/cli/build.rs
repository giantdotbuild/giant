//! `giant build` subcommand.

use crate::cache::LocalCache;
use crate::config::Config;
use crate::discovery;
use crate::events::Event;
use crate::executor::{BuildJob, build};
use crate::graph::BuildGraph;
use crate::model::TargetId;
use crate::paths::AbsPath;
use clap::Args;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum EventsFormat {
    Ndjson,
}

pub async fn execute(args: BuildArgs, global: &super::GlobalFlags) -> anyhow::Result<()> {
    // 1. Locate + load the config.
    let (config, workspace_root) = load_config(global.config.as_deref())?;
    let workspace_abs = AbsPath::new(workspace_root);

    // 2. Build initial graph from include + targets. Inference runs over
    //    this pre-merge set first so any inter-include edges (e.g.
    //    discover:docker depending on discover:go's output) are correct
    //    before the bootstrap pass schedules them.
    let mut graph = BuildGraph::new();
    for target in config.include.iter().chain(config.targets.iter()).cloned() {
        graph.add_target(target)?;
    }
    graph.build_edges_and_validate()?;

    // 3. Open the local cache.
    let cache_root = resolve_cache_dir(&config.cache.dir)?;
    std::fs::create_dir_all(&cache_root)?;
    let cache = LocalCache::open(AbsPath::new(cache_root)).await?;

    // 4. Event channel + renderer. Used by both the bootstrap and the
    //    main build.
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
    let parallelism = args.jobs.unwrap_or_else(num_cpus_estimate);

    // 5. Bootstrap pass - if any include: targets exist, build them first,
    //    then read their JSON outputs and merge discovered targets into
    //    the graph. Re-run edge construction so inferred deps connect
    //    static targets to discovered ones.
    if !config.include.is_empty() {
        let include_ids: Vec<TargetId> =
            config.include.iter().map(|t| t.id.clone()).collect();
        let bootstrap_job = BuildJob {
            graph: Arc::new(graph.clone()),
            selection: include_ids.clone(),
            cache: cache.clone(),
            workspace_root: workspace_abs.clone(),
            parallelism,
            fresh: global.fresh,
            events: tx.clone(),
            cancel: cancel.clone(),
            build_id: format!("bootstrap_{}", short_random()),
        };
        let bootstrap = build(bootstrap_job).await?;
        if bootstrap.counts.failed > 0 {
            drop(tx);
            let _ = renderer.await;
            anyhow::bail!(
                "discovery failed: {} include target(s) failed",
                bootstrap.counts.failed
            );
        }

        // Merge each discovery target's output JSON. We collect the
        // (id, output-paths) pairs first to release the immutable borrow
        // of `graph` before calling `merge_into` (which needs &mut).
        let outputs_to_read: Vec<PathBuf> = include_ids
            .iter()
            .flat_map(|id| {
                let spec = graph.get(id).expect("present in initial graph");
                spec.outputs
                    .iter()
                    .map(|p| workspace_abs.as_path().join(p.as_path()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for abs in outputs_to_read {
            let fragment = discovery::parse_fragment(&abs)?;
            discovery::merge_into(&mut graph, fragment)?;
        }
        graph.build_edges_and_validate()?;
    }

    // 6. Resolve selection over the merged graph.
    let selection: Vec<TargetId> = if args.patterns.is_empty() {
        graph
            .iter()
            .filter(|(_, spec)| !spec.test)
            .map(|(id, _)| id.clone())
            .collect()
    } else {
        let mut out = Vec::new();
        for p in &args.patterns {
            let exact = TargetId::new(p);
            if graph.get(&exact).is_some() {
                out.push(exact);
                continue;
            }
            drop(tx);
            let _ = renderer.await;
            anyhow::bail!("no target matches {p:?} (selection-language is v1.1)");
        }
        out
    };

    if selection.is_empty() {
        drop(tx);
        let _ = renderer.await;
        anyhow::bail!("no targets to build");
    }

    // 7. Main build.
    let build_id = format!("b_{}", short_random());
    let job = BuildJob {
        graph: Arc::new(graph),
        selection,
        cache,
        workspace_root: workspace_abs,
        parallelism,
        fresh: global.fresh,
        events: tx,
        cancel,
        build_id,
    };
    let summary = build(job).await?;

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

/// Walk up from cwd looking for `giant.yaml` / `giant.json`.
fn load_config(explicit: Option<&std::path::Path>) -> anyhow::Result<(Config, PathBuf)> {
    if let Some(path) = explicit {
        let abs = std::fs::canonicalize(path)?;
        let dir = abs
            .parent()
            .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
        let cfg = Config::load(&abs)?;
        return Ok((cfg, dir.to_path_buf()));
    }
    let cwd = std::env::current_dir()?;
    let mut here: &std::path::Path = &cwd;
    loop {
        for name in ["giant.yaml", "giant.yml", "giant.json"] {
            let candidate = here.join(name);
            if candidate.is_file() {
                let cfg = Config::load(&candidate)?;
                return Ok((cfg, here.to_path_buf()));
            }
        }
        match here.parent() {
            Some(p) => here = p,
            None => anyhow::bail!("no giant.yaml/giant.json found in cwd or any parent"),
        }
    }
}

fn resolve_cache_dir(raw: &str) -> anyhow::Result<PathBuf> {
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
        home.join(rest)
    } else if raw == "~" {
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?
    } else {
        PathBuf::from(raw)
    };
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(std::env::current_dir()?.join(expanded))
    }
}

fn num_cpus_estimate() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

fn short_random() -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{nanos:08x}")
}
