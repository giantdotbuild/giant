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

    // Wave-based discovery (TDD-0003 §Recursive discovery).
    //
    // Each wave is a parallel build of the current set of include
    // targets. Their outputs are parsed; any nested `include:` entries
    // they emit feed the next wave. Loop until no new includes appear,
    // or hit MAX_DISCOVERY_DEPTH (cycle / runaway safety net).
    //
    // Cycle detection: we track every include target ID we've already
    // built in `seen`. If a later wave emits an include we've already
    // processed, it's silently skipped - same target can't run twice.
    if !config.include.is_empty() {
        const MAX_DISCOVERY_DEPTH: u32 = 32;
        let mut current_wave: Vec<TargetId> = config.include.iter().map(|t| t.id.clone()).collect();
        let mut seen: std::collections::HashSet<TargetId> = current_wave.iter().cloned().collect();
        let mut depth: u32 = 0;

        while !current_wave.is_empty() {
            if depth >= MAX_DISCOVERY_DEPTH {
                anyhow::bail!(
                    "discovery exceeded {MAX_DISCOVERY_DEPTH} wave depth - \
                     possible cycle? Last wave's target ids: {:?}",
                    current_wave,
                );
            }

            // Sidecar short-circuit: for each pending discovery, try
            // the sidecar before dispatching.
            //
            // - Match: skip the command entirely. Restore the cached
            //   output from the sidecar's recorded fragment so
            //   downstream targets see consistent contents on disk,
            //   and merge directly from the sidecar without re-hashing
            //   (which would rewrite the sidecar with an unchanged
            //   payload but a fresh mtime).
            // - Mismatch / Missing: invalidate the regular AC entry
            //   for this target (cmd + env + cwd + empty inputs is
            //   stable across runs; without invalidation the regular
            //   cache would falsely hit and skip the command), then
            //   queue for dispatch.
            //
            // `--fresh` bypasses the sidecar entirely.
            let mut to_dispatch: Vec<TargetId> = Vec::with_capacity(current_wave.len());
            let mut sidecar_hits: Vec<(TargetId, discovery::DiscoverySidecar)> = Vec::new();
            for id in &current_wave {
                let spec = graph.get(id).expect("present in current wave").clone();
                if fresh {
                    to_dispatch.push(id.clone());
                    continue;
                }
                let key = discovery::discovery_cache_key(&spec);
                let hit = match discovery::read_sidecar(workspace_abs.as_path(), key) {
                    Ok(Some(sidecar)) => {
                        match discovery::verify_reads(&sidecar.reads, workspace_abs.as_path())? {
                            discovery::VerifyOutcome::Match => Some(sidecar),
                            discovery::VerifyOutcome::Mismatch { .. } => None,
                        }
                    }
                    Ok(None) => None,
                    Err(e) => {
                        tracing::warn!(target = %id, error = %e, "sidecar read failed; falling back to cold run");
                        None
                    }
                };
                if let Some(sidecar) = hit {
                    let fragment = discovery::fragment_from_sidecar(&sidecar);
                    let bytes = serde_json::to_vec_pretty(&fragment)?;
                    for out in &spec.outputs {
                        let abs = workspace_abs.as_path().join(out.as_path());
                        if let Some(parent) = abs.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&abs, &bytes)?;
                    }
                    tracing::debug!(target = %id, "discovery restored from sidecar");
                    sidecar_hits.push((id.clone(), sidecar));
                } else {
                    // Invalidate the regular AC entry so the bootstrap
                    // actually re-runs the command instead of hitting
                    // a stale cache entry. The regular key for a
                    // discovery is computed by the executor via
                    // compute_cache_key_with_breakdown; we mirror the
                    // computation here. Empty file_inputs makes this
                    // cheap (no globbing or hashing).
                    let (regular_key, _bd) = crate::executor::compute_cache_key_with_breakdown(
                        &spec,
                        &workspace_abs,
                        &cache,
                        std::collections::BTreeMap::new(),
                    )
                    .await?;
                    cache.delete_ac(&regular_key).await?;
                    to_dispatch.push(id.clone());
                }
            }

            if !to_dispatch.is_empty() {
                let bootstrap_job = BuildJob {
                    graph: Arc::new(graph.clone()),
                    selection: to_dispatch.clone(),
                    cache: cache.clone(),
                    workspace_root: workspace_abs.clone(),
                    parallelism,
                    fresh,
                    events: events.clone(),
                    cancel: cancel.clone(),
                    build_id: format!("bootstrap_w{depth}_{}", short_random()),
                    log_capture: crate::executor::LogCapture::from_cache_config(&config.cache),
                    // Discovery doesn't currently use the remote cache -
                    // discoveries are per-workspace dynamic and aren't
                    // worth pushing to a shared server. Easy to revisit
                    // if a real use case appears.
                    #[cfg(feature = "remote")]
                    remote: None,
                    #[cfg(feature = "remote")]
                    upload_tx: None,
                };
                let bootstrap = build(bootstrap_job).await?;
                if bootstrap.counts.failed > 0 {
                    anyhow::bail!(
                        "discovery failed at wave {depth}: {} include target(s) failed",
                        bootstrap.counts.failed
                    );
                }
            }

            let mut next_wave: Vec<TargetId> = Vec::new();

            // Sidecar hits: merge the cached targets/include directly
            // into the graph. No re-parse, no re-materialize, no
            // sidecar rewrite. Nested includes feed the next wave the
            // same way they would from a cold run.
            for (_id, sidecar) in sidecar_hits {
                let fragment = discovery::fragment_from_sidecar(&sidecar);
                let new_includes = discovery::merge_into(&mut graph, fragment)?;
                for nid in new_includes {
                    if seen.insert(nid.clone()) {
                        next_wave.push(nid);
                    }
                }
            }

            // Dispatched discoveries: parse their freshly-written
            // output file, materialize the reads manifest, write the
            // sidecar for the next run, merge into the graph.
            let dispatched_outputs: Vec<(TargetId, crate::model::TargetSpec, PathBuf)> =
                to_dispatch
                    .iter()
                    .flat_map(|id| {
                        let spec = graph.get(id).expect("present in dispatched set").clone();
                        spec.outputs
                            .iter()
                            .map(|p| {
                                (
                                    id.clone(),
                                    spec.clone(),
                                    workspace_abs.as_path().join(p.as_path()),
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect();

            for (id, spec, abs) in dispatched_outputs {
                let fragment = discovery::parse_fragment(&abs)?;

                // Cooperative protocol (ADR-0013): if the discovery
                // emitted a `reads` manifest, materialize it and write
                // the sidecar so the next run can short-circuit. If
                // absent, this run is uncacheable - warn in lenient
                // mode, error in strict mode (strict not yet wired).
                match &fragment.reads {
                    Some(reads) => {
                        let key = discovery::discovery_cache_key(&spec);
                        let recorded =
                            discovery::materialize_reads(reads, workspace_abs.as_path())?;
                        let sidecar = discovery::DiscoverySidecar::new(
                            key,
                            fragment.targets.clone(),
                            fragment.include.clone(),
                            recorded,
                        );
                        discovery::write_sidecar(workspace_abs.as_path(), &sidecar)?;
                    }
                    None => {
                        if config.discovery.strict {
                            anyhow::bail!(
                                "discovery '{id}' emitted no `reads` manifest. Strict mode \
                                 (discovery.strict: true) requires every discovery to declare \
                                 what it read so the engine can verify the cached output on \
                                 later runs. Add a `reads` block to the script's output, or \
                                 set discovery.strict: false to fall back to lenient mode."
                            );
                        }
                        tracing::warn!(
                            target = %id,
                            "discovery emitted no `reads` manifest; output cannot be cached \
                             across runs. Have the discovery emit `reads.files` / \
                             `reads.dirs` to enable warm-skip (TDD-0015)."
                        );
                    }
                }

                let new_includes = discovery::merge_into(&mut graph, fragment)?;
                for nid in new_includes {
                    if seen.insert(nid.clone()) {
                        next_wave.push(nid);
                    }
                }
            }

            // Validate edges between waves: next-wave includes may
            // declare deps on this-wave targets, and we need the graph
            // consistent before executing them.
            graph.build_edges_and_validate()?;

            current_wave = next_wave;
            depth += 1;
        }
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
