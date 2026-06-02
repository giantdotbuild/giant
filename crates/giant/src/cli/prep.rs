//! Shared CLI setup: locate config, build the graph from the static
//! `targets:`, and open the cache.
//!
//! Used by `giant build`, `giant affected`, `giant graph`, and
//! `giant explain` - anything that needs the build graph before it does
//! its specific work.

use crate::cache::LocalCache;
use crate::config::Config;
use crate::graph::BuildGraph;
use crate::paths::AbsPath;
use std::path::{Path, PathBuf};

/// Everything a subcommand needs to operate on the graph.
pub struct Prepared {
    pub graph: BuildGraph,
    pub cache: LocalCache,
    pub workspace_root: AbsPath,
    /// Loaded config. Kept around for subcommands that need cache or
    /// remote-cache settings.
    #[allow(dead_code)]
    pub config: Config,
}

/// Locate + load `giant.yaml`/`giant.json`, build the graph from the
/// static `targets:`, and open the local cache.
pub async fn prepare(config_path: Option<&Path>) -> anyhow::Result<Prepared> {
    let (config, workspace_root) = Config::scan_workspace(config_path)?;
    let workspace_abs = AbsPath::new(workspace_root);

    let mut graph = BuildGraph::new();
    for target in config.targets.iter().cloned() {
        graph.add_target(target)?;
    }
    graph.build_edges_and_validate()?;

    let cache_root = resolve_cache_dir(&config.cache.dir)?;
    std::fs::create_dir_all(&cache_root)?;
    let cache = LocalCache::open(AbsPath::new(cache_root)).await?;

    Ok(Prepared {
        graph,
        cache,
        workspace_root: workspace_abs,
        config,
    })
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
