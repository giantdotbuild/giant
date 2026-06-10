//! Shared CLI setup: locate config, build the graph from the static
//! `targets:`, and open the cache.
//!
//! Used by `giant build`, `giant affected`, `giant graph`, and
//! `giant explain` - anything that needs the build graph before it does
//! its specific work.

use crate::cache::LocalCache;
use crate::config::Config;
use crate::graph::BuildGraph;
use crate::model::TargetId;
use crate::paths::AbsPath;
use std::path::{Path, PathBuf};

/// Path of the file recording the most recent build's failed targets,
/// under the workspace state directory (for the `failed-last` selector).
pub fn last_failures_path(workspace_root: &Path, state_dir: &str) -> PathBuf {
    workspace_root.join(state_dir).join("last-failures.json")
}

/// Record the failed target labels from a build so `failed-last` can
/// re-select them. Empty on a clean build (which clears any prior set).
/// Best-effort: a write failure never fails the build.
pub fn write_last_failures(path: &Path, failed: &[TargetId]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let labels: Vec<&str> = failed.iter().map(TargetId::as_str).collect();
    if let Ok(json) = serde_json::to_vec(&labels) {
        let _ = std::fs::write(path, json);
    }
}

/// Read the failed labels recorded by the last build (empty if none).
pub fn read_last_failures(path: &Path) -> Vec<TargetId> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    serde_json::from_slice::<Vec<String>>(&bytes)
        .unwrap_or_default()
        .into_iter()
        .map(TargetId::new)
        .collect()
}

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
    // A github_actions remote outside Actions is a no-op rather than an
    // error: the config is committed once and local builds simply run
    // without a remote. Inside Actions (GITHUB_ACTIONS is set by the
    // runner) a missing token still fails loudly - that means the
    // credential-export step is missing from the workflow.
    if config.remote.kind == crate::config::RemoteKind::GithubActions
        && std::env::var_os("GITHUB_ACTIONS").is_none()
    {
        tracing::info!("remote cache (github_actions) inactive outside GitHub Actions");
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
    // proceed with local-only behaviour.
    let _ = config;
    Ok((None, None, None))
}
