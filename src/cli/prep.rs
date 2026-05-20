//! Shared CLI setup: locate config, open cache, run the discovery
//! bootstrap pass, and return the merged build graph.
//!
//! Used by `giant build`, `giant affected`, and (future) `giant graph`
//! / `giant explain` - anything that needs the final post-discovery
//! graph before it does its specific work.

use crate::cache::LocalCache;
use crate::config::Config;
use crate::discovery;
use crate::events::{Event, EventSender};
use crate::executor::{BuildJob, build};
use crate::graph::BuildGraph;
use crate::model::TargetId;
use crate::paths::AbsPath;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Everything a subcommand needs after the bootstrap pass.
pub struct Prepared {
    pub graph: BuildGraph,
    pub cache: LocalCache,
    pub workspace_root: AbsPath,
    /// Loaded config. Kept around for subcommands that need cache or
    /// remote-cache settings.
    #[allow(dead_code)]
    pub config: Config,
}

/// Locate + load `giant.yaml`/`giant.json`, open the local cache, run
/// any `include:` discovery targets, merge their JSON outputs into the
/// graph, and return the result.
///
/// `events` receives bootstrap events (target.started, target.log, etc.).
/// Pass a null sink if you don't want them surfaced - see
/// `null_event_sink()` below.
pub async fn prepare(
    config_path: Option<&Path>,
    parallelism: usize,
    fresh: bool,
    events: EventSender,
    cancel: CancellationToken,
) -> anyhow::Result<Prepared> {
    let (config, workspace_root) = load_config(config_path)?;
    let workspace_abs = AbsPath::new(workspace_root);

    let mut graph = BuildGraph::new();
    for target in config.include.iter().chain(config.targets.iter()).cloned() {
        graph.add_target(target)?;
    }
    graph.build_edges_and_validate()?;

    let cache_root = resolve_cache_dir(&config.cache.dir)?;
    std::fs::create_dir_all(&cache_root)?;
    let cache = LocalCache::open(AbsPath::new(cache_root)).await?;

    if !config.include.is_empty() {
        let include_ids: Vec<TargetId> = config.include.iter().map(|t| t.id.clone()).collect();
        let bootstrap_job = BuildJob {
            graph: Arc::new(graph.clone()),
            selection: include_ids.clone(),
            cache: cache.clone(),
            workspace_root: workspace_abs.clone(),
            parallelism,
            fresh,
            events,
            cancel,
            build_id: format!("bootstrap_{}", short_random()),
            // Discovery doesn't currently use the remote cache -
            // discoveries are per-workspace dynamic and aren't worth
            // pushing to a shared server. Easy to revisit if a real
            // use case appears.
            #[cfg(feature = "remote")]
            remote: None,
            #[cfg(feature = "remote")]
            upload_tx: None,
        };
        let bootstrap = build(bootstrap_job).await?;
        if bootstrap.counts.failed > 0 {
            anyhow::bail!(
                "discovery failed: {} include target(s) failed",
                bootstrap.counts.failed
            );
        }

        // Merge each discovery target's output JSON. Collect outputs first
        // to release the immutable borrow of `graph`.
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

    Ok(Prepared {
        graph,
        cache,
        workspace_root: workspace_abs,
        config,
    })
}

/// An event sink that silently drops everything. Use for subcommands
/// (affected, graph, explain) that need the bootstrap to run but
/// shouldn't dump per-target events to the user.
pub fn null_event_sink() -> (EventSender, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(1024);
    let handle = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    (tx, handle)
}

/// Walk up from cwd looking for `giant.yaml` / `giant.json`.
pub fn load_config(explicit: Option<&Path>) -> anyhow::Result<(Config, PathBuf)> {
    if let Some(path) = explicit {
        let abs = std::fs::canonicalize(path)?;
        let dir = abs
            .parent()
            .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
        let cfg = Config::load(&abs)?;
        return Ok((cfg, dir.to_path_buf()));
    }
    let cwd = std::env::current_dir()?;
    let mut here: &Path = &cwd;
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

pub fn resolve_cache_dir(raw: &str) -> anyhow::Result<PathBuf> {
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

pub fn num_cpus_estimate() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

pub fn short_random() -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{nanos:08x}")
}

/// Returned by `open_remote`. All three fields are `None` when the
/// remote is disabled in config or the `remote` feature is off; in
/// that case callers don't have to do anything special - the executor
/// silently skips the remote lookup chain.
#[cfg(feature = "remote")]
pub type OpenedRemote = (
    Option<crate::remote::RemoteCache>,
    Option<tokio::sync::mpsc::Sender<crate::remote::UploadJob>>,
    Option<tokio::task::JoinHandle<()>>,
);

/// Open the remote cache + background uploader if configured.
#[cfg(feature = "remote")]
pub fn open_remote(config: &Config) -> anyhow::Result<OpenedRemote> {
    if !config.remote.enabled {
        return Ok((None, None, None));
    }
    let resolved = crate::remote::RemoteCacheConfig::from_config(&config.remote)
        .map_err(|e| anyhow::anyhow!("remote cache config: {e}"))?;
    let remote = crate::remote::RemoteCache::open(resolved)
        .map_err(|e| anyhow::anyhow!("open remote cache: {e}"))?;
    let (tx, handle) = crate::remote::spawn_uploader(remote.clone());
    Ok((Some(remote), Some(tx), Some(handle)))
}

#[cfg(not(feature = "remote"))]
pub fn open_remote(config: &Config) -> anyhow::Result<(Option<()>, Option<()>, Option<()>)> {
    // When the user has cache.remote.enabled: true in config but the
    // binary was built without the `remote` feature, log once and
    // proceed with local-only behaviour (TDD-0006).
    let _ = config;
    Ok((None, None, None))
}
