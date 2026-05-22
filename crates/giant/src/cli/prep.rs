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

    // Worklist-based discovery (TDD-0003 revised).
    //
    // Top-level `include:` entries seed `pending`. Each round, every
    // pending discovery is dispatched (or short-circuited by its
    // sidecar) in parallel through the executor. Any `include:`
    // entries that those discoveries emit get appended to
    // `next_pending`, and we loop. Termination is "worklist empty"
    // rather than a fixed wave count.
    //
    // Cycle / runaway detection: every enqueued target records its
    // chain depth (parent's depth + 1). If a target would exceed
    // `MAX_CHAIN_GENERATIONS`, we abort with the full chain so the
    // user sees which discoveries kept emitting new descendants. A
    // dedup `seen` set silently drops already-known targets.
    if !config.include.is_empty() {
        const MAX_CHAIN_GENERATIONS: u32 = 8;

        let mut pending: Vec<TargetId> = config.include.iter().map(|t| t.id.clone()).collect();
        let mut seen: std::collections::HashSet<TargetId> = pending.iter().cloned().collect();
        let mut chain_depth: std::collections::HashMap<TargetId, u32> =
            pending.iter().cloned().map(|id| (id, 0)).collect();
        let mut parent_of: std::collections::HashMap<TargetId, TargetId> =
            std::collections::HashMap::new();
        let mut round: u32 = 0;

        while !pending.is_empty() {
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
            let mut to_dispatch: Vec<TargetId> = Vec::with_capacity(pending.len());
            let mut sidecar_hits: Vec<(TargetId, discovery::DiscoverySidecar)> = Vec::new();
            for id in &pending {
                let spec = graph.get(id).expect("present in pending worklist").clone();
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
                    build_id: format!("bootstrap_r{round}_{}", short_random()),
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
                        "discovery failed in round {round}: {} include target(s) failed",
                        bootstrap.counts.failed
                    );
                }
            }

            let mut next_pending: Vec<TargetId> = Vec::new();

            // Sidecar hits: merge the cached targets/include directly
            // into the graph. No re-parse, no re-materialize, no
            // sidecar rewrite. Nested includes feed the next round
            // the same way they would from a cold run.
            for (parent_id, sidecar) in sidecar_hits {
                let fragment = discovery::fragment_from_sidecar(&sidecar);
                let new_includes = discovery::merge_into(&mut graph, fragment)?;
                enqueue_descendants(
                    &parent_id,
                    new_includes,
                    &mut seen,
                    &mut chain_depth,
                    &mut parent_of,
                    &mut next_pending,
                    MAX_CHAIN_GENERATIONS,
                )?;
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

            for (parent_id, spec, abs) in dispatched_outputs {
                let fragment = discovery::parse_fragment(&abs)?;

                // Cooperative protocol (ADR-0013): if the discovery
                // emitted a `reads` manifest, materialize it and write
                // the sidecar so the next run can short-circuit. If
                // absent, this run is uncacheable - warn in lenient
                // mode, error in strict mode (discovery.strict).
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
                                "discovery '{parent_id}' emitted no `reads` manifest. Strict mode \
                                 (discovery.strict: true) requires every discovery to declare \
                                 what it read so the engine can verify the cached output on \
                                 later runs. Add a `reads` block to the script's output, or \
                                 set discovery.strict: false to fall back to lenient mode."
                            );
                        }
                        tracing::warn!(
                            target = %parent_id,
                            "discovery emitted no `reads` manifest; output cannot be cached \
                             across runs. Have the discovery emit `reads.files` / \
                             `reads.dirs` to enable warm-skip (TDD-0015)."
                        );
                    }
                }

                let new_includes = discovery::merge_into(&mut graph, fragment)?;
                enqueue_descendants(
                    &parent_id,
                    new_includes,
                    &mut seen,
                    &mut chain_depth,
                    &mut parent_of,
                    &mut next_pending,
                    MAX_CHAIN_GENERATIONS,
                )?;
            }

            // Validate edges between rounds: the next round's includes
            // may declare deps on this round's targets, and we need
            // the graph consistent before executing them.
            graph.build_edges_and_validate()?;

            pending = next_pending;
            round += 1;
        }
    }

    Ok(Prepared {
        graph,
        cache,
        workspace_root: workspace_abs,
        config,
    })
}

/// Enqueue a parent discovery's emitted includes onto the worklist.
/// Skips already-seen IDs, records each new entry's chain depth, and
/// errors out with the full chain if the depth would exceed
/// `max_generations` - that's where a runaway-emitter cycle gets
/// caught.
#[allow(clippy::too_many_arguments)]
fn enqueue_descendants(
    parent_id: &TargetId,
    new_includes: Vec<TargetId>,
    seen: &mut std::collections::HashSet<TargetId>,
    chain_depth: &mut std::collections::HashMap<TargetId, u32>,
    parent_of: &mut std::collections::HashMap<TargetId, TargetId>,
    next_pending: &mut Vec<TargetId>,
    max_generations: u32,
) -> anyhow::Result<()> {
    let parent_gen = *chain_depth.get(parent_id).unwrap_or(&0);
    let child_gen = parent_gen + 1;
    if child_gen > max_generations {
        let chain = format_chain(parent_of, parent_id);
        anyhow::bail!(
            "discovery chain exceeded {max_generations} generations: {chain} keeps emitting new \
             includes. Last attempted descendants: {:?}",
            new_includes,
        );
    }
    for nid in new_includes {
        if seen.insert(nid.clone()) {
            chain_depth.insert(nid.clone(), child_gen);
            parent_of.insert(nid.clone(), parent_id.clone());
            next_pending.push(nid);
        }
    }
    Ok(())
}

fn format_chain(
    parent_of: &std::collections::HashMap<TargetId, TargetId>,
    leaf: &TargetId,
) -> String {
    let mut chain = vec![leaf.clone()];
    let mut cur = leaf;
    while let Some(p) = parent_of.get(cur) {
        chain.push(p.clone());
        cur = p;
    }
    chain.reverse();
    chain
        .iter()
        .map(|id| id.as_str())
        .collect::<Vec<_>>()
        .join(" → ")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn t(s: &str) -> TargetId {
        TargetId::new(s)
    }

    #[test]
    fn worklist_enqueue_dedups_already_seen() {
        let mut seen: HashSet<TargetId> = [t("a"), t("b")].into();
        let mut chain_depth: HashMap<TargetId, u32> = [(t("a"), 0), (t("b"), 0)].into();
        let mut parent_of: HashMap<TargetId, TargetId> = HashMap::new();
        let mut next: Vec<TargetId> = Vec::new();
        enqueue_descendants(
            &t("a"),
            vec![t("b"), t("c")], // b already seen, c is fresh
            &mut seen,
            &mut chain_depth,
            &mut parent_of,
            &mut next,
            8,
        )
        .unwrap();
        assert_eq!(next, vec![t("c")]);
        assert_eq!(chain_depth[&t("c")], 1);
        assert_eq!(parent_of[&t("c")], t("a"));
    }

    #[test]
    fn worklist_chain_overflow_reports_full_chain() {
        // Build a chain root → g1 → g2 → ... → g7 (7 generations
        // already). g7 emits g8 - that fits (depth = 8 = max). Then
        // g8 trying to emit g9 trips the limit.
        let mut seen: HashSet<TargetId> = HashSet::new();
        seen.insert(t("root"));
        let mut chain_depth: HashMap<TargetId, u32> = HashMap::new();
        chain_depth.insert(t("root"), 0);
        let mut parent_of: HashMap<TargetId, TargetId> = HashMap::new();
        let mut next: Vec<TargetId> = Vec::new();

        // root → g1 ... → g8 (all within budget).
        let mut prev = t("root");
        for i in 1..=8 {
            let child = t(&format!("g{i}"));
            enqueue_descendants(
                &prev,
                vec![child.clone()],
                &mut seen,
                &mut chain_depth,
                &mut parent_of,
                &mut next,
                8,
            )
            .unwrap();
            prev = child;
        }
        // g8 → g9 would be generation 9, over the cap.
        let err = enqueue_descendants(
            &t("g8"),
            vec![t("g9")],
            &mut seen,
            &mut chain_depth,
            &mut parent_of,
            &mut next,
            8,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("exceeded 8 generations"), "got: {err}");
        assert!(
            err.contains("root → g1 → g2 → g3 → g4 → g5 → g6 → g7 → g8"),
            "expected chain in error: {err}",
        );
    }

    #[test]
    fn worklist_chain_depth_propagates_through_parents() {
        let mut seen: HashSet<TargetId> = [t("root")].into();
        let mut chain_depth: HashMap<TargetId, u32> = [(t("root"), 0)].into();
        let mut parent_of: HashMap<TargetId, TargetId> = HashMap::new();
        let mut next: Vec<TargetId> = Vec::new();
        enqueue_descendants(
            &t("root"),
            vec![t("child")],
            &mut seen,
            &mut chain_depth,
            &mut parent_of,
            &mut next,
            8,
        )
        .unwrap();
        enqueue_descendants(
            &t("child"),
            vec![t("grandchild")],
            &mut seen,
            &mut chain_depth,
            &mut parent_of,
            &mut next,
            8,
        )
        .unwrap();
        assert_eq!(chain_depth[&t("child")], 1);
        assert_eq!(chain_depth[&t("grandchild")], 2);
    }
}
