//! Parallel executor.
//!
//! Dispatches targets in topological order, up to a parallelism budget,
//! via a `JoinSet`. Each target computes a cache key, consults the cache
//! (local AC → remote AC → `exists:` check), and restores on a hit or
//! runs the command and stores outputs on a miss. The shape matches
//! TDD-0009.

mod key;
mod run;
mod sandbox;

use key::compute_cache_key;
pub use key::{CacheKeyBreakdown, FileInputContribution, compute_cache_key_with_breakdown};
use run::{result_output_hash, run_target, try_cache_hit, try_exists_check, try_remote_hit};
pub use sandbox::SandboxPolicy;

use crate::cache::LocalCache;
use crate::events::{Event, EventSender, TargetCounts, TargetResultKind};
use crate::graph::BuildGraph;
use crate::model::{CacheKey, ContentHash, TargetId, TargetSpec};
use crate::paths::AbsPath;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),

    #[error("cache: {0}")]
    Cache(#[from] crate::cache::CacheError),

    #[error("graph: {0}")]
    Graph(#[from] crate::graph::GraphError),

    #[error("target {0:?} not in graph")]
    TargetNotFound(TargetId),

    #[error("dependency {dep:?} failed for {parent:?}")]
    DependencyFailed { parent: TargetId, dep: TargetId },

    #[error("cancelled")]
    Cancelled,
}

/// What the caller hands the executor.
pub struct BuildJob {
    pub graph: Arc<BuildGraph>,
    pub selection: Vec<TargetId>,
    pub cache: LocalCache,
    pub workspace_root: AbsPath,
    pub parallelism: usize,
    pub fresh: bool,
    pub events: EventSender,
    pub cancel: CancellationToken,
    pub build_id: String,
    /// Log capture / replay policy. Default = capture both, replay on
    /// cache hits, 5 MiB per stream.
    pub log_capture: LogCapture,
    /// Sandbox policy, set by the CLI when `--sandbox` is on. `None` = run
    /// commands directly, exactly as today (ADR-0030).
    pub sandbox: Option<SandboxPolicy>,
    /// Optional remote cache. Inserted by the CLI when configured;
    /// always `None` when the `remote` feature is off.
    #[cfg(feature = "remote")]
    pub remote: Option<crate::remote::RemoteCache>,
    /// Background uploader sink. Same lifecycle as `remote`.
    #[cfg(feature = "remote")]
    pub upload_tx: Option<tokio::sync::mpsc::Sender<crate::remote::UploadJob>>,
}

/// Per-build log capture/replay policy. Bundled so the configuration
/// gets passed through one field rather than three.
#[derive(Debug, Clone, Copy)]
pub struct LogCapture {
    /// Write stdout/stderr to CAS alongside outputs when a target
    /// builds, so a future cache hit can replay them.
    pub capture: bool,
    /// On cache hits (local or remote), emit synthetic `TargetLog`
    /// events from the stored blob so the porcelain sees the same
    /// output as a fresh build.
    pub replay: bool,
    /// Per-stream byte cap for capture. Lines beyond the cap stream
    /// live but don't make it to the blob.
    pub cap_bytes: usize,
}

impl Default for LogCapture {
    fn default() -> Self {
        Self {
            capture: true,
            replay: true,
            cap_bytes: 5 * 1024 * 1024,
        }
    }
}

impl LogCapture {
    pub fn from_cache_config(c: &crate::config::CacheConfig) -> Self {
        Self {
            capture: c.capture_logs,
            replay: c.replay_logs,
            cap_bytes: c.log_capture_cap_bytes,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuildSummary {
    pub counts: TargetCounts,
    pub duration: Duration,
    pub failed_targets: Vec<TargetId>,
    pub cache_keys: HashMap<TargetId, CacheKey>,
}

/// Outcome for one target. The target's cache key travels separately in
/// `CompletionMsg`, so it isn't repeated here.
#[derive(Debug, Clone)]
enum TargetResult {
    Built {
        duration: Duration,
        outputs: Vec<OutputFile>,
    },
    CacheHit {
        /// Carried out of try_cache_hit so the dispatcher doesn't re-read AC.
        output_hash: ContentHash,
    },
    /// Restored from the remote cache. We've already written the outputs
    /// to the workspace AND populated the local cache, so future runs
    /// hit locally. Constructed only when the `remote` feature is on;
    /// the variant stays defined either way so the public TargetResultKind
    /// (in `events::Event`) lines up across builds.
    #[allow(dead_code)]
    RemoteCacheHit {
        output_hash: ContentHash,
    },
    /// `exists:` check returned 0 - the artifact lives outside the local
    /// filesystem (registry, S3, etc.). No local outputs were produced or
    /// restored; downstream consumers see the empty-outputs hash for this
    /// target.
    ExternalCacheHit {
        output_hash: ContentHash,
    },
    Failed {
        error: String,
    },
}

impl TargetResult {
    fn kind(&self) -> TargetResultKind {
        match self {
            Self::Built { .. } => TargetResultKind::Built,
            Self::CacheHit { .. } => TargetResultKind::CacheHit,
            Self::RemoteCacheHit { .. } => TargetResultKind::RemoteCacheHit,
            Self::ExternalCacheHit { .. } => TargetResultKind::ExternalCacheHit,
            Self::Failed { .. } => TargetResultKind::Failed,
        }
    }
}

#[derive(Debug, Clone)]
struct OutputFile {
    rel_path: String,
    content_hash: ContentHash,
    size: u64,
    executable: bool,
    mode: String,
}

/// Shared per-target context for the worker. Cheap to clone; mostly
/// `Arc`-backed handles.
#[derive(Clone)]
struct TargetCtx {
    cache: LocalCache,
    workspace_root: AbsPath,
    fresh: bool,
    events: EventSender,
    cancel: CancellationToken,
    build_id: String,
    log_capture: LogCapture,
    /// Sandbox policy; `None` runs commands directly (ADR-0030).
    sandbox: Option<SandboxPolicy>,
    /// Optional remote cache. `None` when the binary is built without
    /// the `remote` feature or when the user has it disabled. Lookup
    /// chain consults it between local AC and the `exists:` check.
    #[cfg(feature = "remote")]
    remote: Option<crate::remote::RemoteCache>,
    /// Sender for background uploads. `None` when remote disabled.
    /// Workers push completed builds onto this; the build never waits.
    #[cfg(feature = "remote")]
    upload_tx: Option<tokio::sync::mpsc::Sender<crate::remote::UploadJob>>,
}

/// What a worker reports when it finishes a target.
struct CompletionMsg {
    id: TargetId,
    cache_key: CacheKey,
    result: TargetResult,
    /// `outputs_content_hash` for downstream dep keys (TDD-0009 §Early
    /// cutoff). `None` only when the target failed.
    output_hash: Option<ContentHash>,
}

/// Run the build.
pub async fn build(job: BuildJob) -> Result<BuildSummary, ExecutorError> {
    let started = Instant::now();
    let parallelism = job.parallelism.max(1);

    // 1. Closure of selection over deps; topo order restricted to subgraph.
    let in_subgraph = job.graph.closure_over_deps(job.selection.iter());
    let order: Vec<TargetId> = job
        .graph
        .topo_order()?
        .into_iter()
        .filter(|id| in_subgraph.contains(id))
        .collect();

    emit(
        &job.events,
        Event::BuildStarted {
            id: job.build_id.clone(),
            selection: job
                .selection
                .iter()
                .map(|t| t.as_str().to_string())
                .collect(),
            target_ids: order.clone(),
            parallelism,
        },
    )
    .await;

    // 2. Initialize the dispatcher state.
    //
    // `pending_deps[T]` counts unmet deps of T (any disposition: success,
    // failure, or skipped). When it reaches zero, T is *ready* - meaning
    // its deps' state is fully known, not necessarily that they all
    // succeeded. At dispatch time we re-check whether any dep failed and
    // skip accordingly.
    let mut pending_deps: HashMap<TargetId, usize> = HashMap::with_capacity(order.len());
    let mut ready: VecDeque<TargetId> = VecDeque::new();
    let mut running: HashSet<TargetId> = HashSet::new();
    let mut failed_or_skipped: HashSet<TargetId> = HashSet::new();
    let mut dep_output_hashes: HashMap<TargetId, ContentHash> = HashMap::new();
    let mut cache_keys: HashMap<TargetId, CacheKey> = HashMap::new();
    let mut counts = TargetCounts::default();
    let mut failed_targets: Vec<TargetId> = Vec::new();

    for id in &order {
        // `graph.direct_deps` returns the union of explicit and
        // inferred deps. Inferred-only deps must gate dispatch too,
        // otherwise downstream races ahead before its upstream's
        // outputs exist on disk.
        let unmet = job
            .graph
            .direct_deps(id)
            .into_iter()
            .filter(|d| in_subgraph.contains(d))
            .count();
        pending_deps.insert(id.clone(), unmet);
        if unmet == 0 {
            ready.push_back(id.clone());
        }
    }

    let ctx = TargetCtx {
        cache: job.cache.clone(),
        workspace_root: job.workspace_root.clone(),
        fresh: job.fresh,
        events: job.events.clone(),
        cancel: job.cancel.clone(),
        build_id: job.build_id.clone(),
        log_capture: job.log_capture,
        sandbox: job.sandbox.clone(),
        #[cfg(feature = "remote")]
        remote: job.remote.clone(),
        #[cfg(feature = "remote")]
        upload_tx: job.upload_tx.clone(),
    };

    // 3. Dispatch loop.
    //
    // - Drain `ready` up to `parallelism` in-flight tasks.
    // - At each dispatch, check whether any of T's deps failed - if so
    //   we synthesise a Skipped completion inline (no worker spawn) and
    //   immediately propagate to downstream.
    // - `join_set.join_next()` is the heartbeat; whenever a worker
    //   finishes, we update state and refill the ready queue.
    let mut join_set: JoinSet<Result<CompletionMsg, ExecutorError>> = JoinSet::new();
    let mut handled_completions: Vec<TargetId> = Vec::new();

    loop {
        if job.cancel.is_cancelled() {
            join_set.abort_all();
            // Drain so abort takes effect; ignore results.
            while join_set.join_next().await.is_some() {}
            return Err(ExecutorError::Cancelled);
        }

        // Dispatch as many ready targets as the parallelism budget allows.
        while running.len() < parallelism
            && let Some(tid) = ready.pop_front()
        {
            let spec = match job.graph.get(&tid) {
                Some(s) => s.clone(),
                None => return Err(ExecutorError::TargetNotFound(tid)),
            };

            // Direct deps (explicit ∪ inferred), via the graph.
            let direct = job.graph.direct_deps(&tid);

            // Check: did any of this target's deps fail / get skipped?
            if let Some(bad) = direct.iter().find(|d| failed_or_skipped.contains(*d)) {
                let reason = format!("dep '{bad}' failed");
                failed_or_skipped.insert(tid.clone());
                counts.skipped += 1;
                emit_finished(
                    &ctx.events,
                    &ctx.build_id,
                    &tid,
                    TargetResultKind::Skipped,
                    0,
                    vec![],
                    Some(reason),
                )
                .await;
                handled_completions.push(tid);
                continue;
            }

            // Build dep_outs from already-completed deps.
            let dep_outs: Vec<ContentHash> = direct
                .iter()
                .map(|d| {
                    dep_output_hashes
                        .get(d)
                        .copied()
                        .expect("dep must be completed by ready-time")
                })
                .collect();

            running.insert(tid.clone());
            let ctx2 = ctx.clone();
            let tid2 = tid.clone();
            join_set.spawn(async move { dispatch_target(tid2, spec, dep_outs, ctx2).await });
        }

        // Propagate any inline-handled completions through pending_deps.
        for id in handled_completions.drain(..) {
            propagate(&job.graph, &in_subgraph, &id, &mut pending_deps, &mut ready);
        }

        // Are we done?
        if running.is_empty() && ready.is_empty() {
            break;
        }

        // Block on the next worker completion.
        let next = match join_set.join_next().await {
            Some(Ok(Ok(msg))) => msg,
            Some(Ok(Err(e))) => return Err(e),
            Some(Err(je)) => return Err(ExecutorError::Io(std::io::Error::other(je.to_string()))),
            None => break,
        };

        running.remove(&next.id);
        cache_keys.insert(next.id.clone(), next.cache_key);
        if let Some(oh) = next.output_hash {
            dep_output_hashes.insert(next.id.clone(), oh);
        }

        match &next.result {
            TargetResult::Built { .. } => counts.built += 1,
            TargetResult::CacheHit { .. }
            | TargetResult::RemoteCacheHit { .. }
            | TargetResult::ExternalCacheHit { .. } => {
                // Remote and external hits get bundled into cache_hit
                // for the summary; the TargetResultKind on the event
                // stays distinct so NDJSON consumers can break them out.
                counts.cache_hit += 1;
            }
            TargetResult::Failed { error, .. } => {
                counts.failed += 1;
                failed_or_skipped.insert(next.id.clone());
                failed_targets.push(next.id.clone());
                tracing::warn!(target=%next.id, error=%error, "target failed");
            }
        }

        propagate(
            &job.graph,
            &in_subgraph,
            &next.id,
            &mut pending_deps,
            &mut ready,
        );
    }

    let duration = started.elapsed();
    let ok = counts.failed == 0;
    emit(
        &job.events,
        Event::BuildFinished {
            id: job.build_id.clone(),
            ok,
            duration_ms: duration.as_millis() as u64,
            counts: counts.clone(),
        },
    )
    .await;

    Ok(BuildSummary {
        counts,
        duration,
        failed_targets,
        cache_keys,
    })
}

/// Decrement downstream pending-dep counts and push any that hit zero
/// onto the ready queue.
fn propagate(
    graph: &BuildGraph,
    in_subgraph: &HashSet<TargetId>,
    just_done: &TargetId,
    pending_deps: &mut HashMap<TargetId, usize>,
    ready: &mut VecDeque<TargetId>,
) {
    for downstream in graph.direct_downstream(just_done) {
        if !in_subgraph.contains(&downstream) {
            continue;
        }
        let Some(count) = pending_deps.get_mut(&downstream) else {
            continue;
        };
        if *count > 0 {
            *count -= 1;
            if *count == 0 {
                ready.push_back(downstream);
            }
        }
    }
}

/// Per-target worker: compute key, look up, run if needed, emit events,
/// return a completion message.
async fn dispatch_target(
    id: TargetId,
    spec: TargetSpec,
    dep_outs: Vec<ContentHash>,
    ctx: TargetCtx,
) -> Result<CompletionMsg, ExecutorError> {
    let key = compute_cache_key(&spec, &ctx.workspace_root, &ctx.cache, &dep_outs).await?;

    let _ = ctx
        .events
        .send(Event::TargetStarted {
            build: ctx.build_id.clone(),
            id: id.clone(),
            cache_key: key.to_hex(),
            command: spec.command.clone(),
        })
        .await;

    // Lookup chain when not bypassed by `--fresh` / a forced-fresh set:
    //   1. local AC cache       - only when the target is cacheable
    //   2. remote AC cache      - only when cacheable and `remote_cache`
    //      (feature-gated; populates local on hit)
    //   3. `exists:` check - for artifacts that live elsewhere (Docker
    //      registry, S3, etc.). Runs regardless of `cache:` - it's the
    //      target's own external-existence test, not Giant's cache. The
    //      command runs with $GIANT_CACHE_KEY in env; exit 0 →
    //      ExternalCacheHit, skip the build.
    //   4. run the target's command.
    //
    // `cache: false` (or a `test:` target without an explicit `cache:`)
    // skips steps 1-2 and, on the store side (`run_target`), the AC write
    // and upload - so the command runs and nothing is cached.
    let bypass_lookup = ctx.fresh;
    let cacheable = spec.is_cacheable();
    let (result, output_hash) = if bypass_lookup {
        let r = run_target(&ctx, &spec, key).await;
        let oh = result_output_hash(&r);
        (r, oh)
    } else if cacheable && let Some((r, oh)) = try_cache_hit(&ctx, &spec.id, &key).await? {
        (r, Some(oh))
    } else if cacheable
        && spec.remote_cache
        && let Some((r, oh)) = try_remote_hit(&ctx, &spec.id, &key).await?
    {
        (r, Some(oh))
    } else if let Some((r, oh)) = try_exists_check(&ctx, &spec, key).await {
        (r, Some(oh))
    } else {
        let r = run_target(&ctx, &spec, key).await;
        let oh = result_output_hash(&r);
        (r, oh)
    };

    let duration_ms = match &result {
        TargetResult::Built { duration, .. } => duration.as_millis() as u64,
        _ => 0,
    };
    let outputs_paths: Vec<String> = match &result {
        TargetResult::Built { outputs, .. } => outputs.iter().map(|o| o.rel_path.clone()).collect(),
        _ => vec![],
    };
    let err: Option<String> = if let TargetResult::Failed { error, .. } = &result {
        Some(error.clone())
    } else {
        None
    };

    emit_finished(
        &ctx.events,
        &ctx.build_id,
        &id,
        result.kind(),
        duration_ms,
        outputs_paths,
        err,
    )
    .await;

    Ok(CompletionMsg {
        id,
        cache_key: key,
        result,
        output_hash,
    })
}

async fn emit(events: &EventSender, ev: Event) {
    let _ = events.send(ev).await;
}

async fn emit_finished(
    events: &EventSender,
    build_id: &str,
    id: &TargetId,
    result: TargetResultKind,
    duration_ms: u64,
    outputs: Vec<String>,
    error: Option<String>,
) {
    let _ = events
        .send(Event::TargetFinished {
            build: build_id.to_string(),
            id: id.clone(),
            result,
            duration_ms,
            exit_code: None,
            outputs,
            error,
        })
        .await;
}
