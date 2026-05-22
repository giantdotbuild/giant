//! Parallel executor.
//!
//! Phase-1 slice: serial dispatch in topological order. Computes a
//! cache key per target, looks it up locally, restores or runs and
//! stores. The shape matches TDD-0009; parallelism and remote cache
//! land in later slices.

use crate::cache::{AcEntry, LocalCache, OutputEntry};
use crate::events::{Event, EventSender, LogStream, TargetCounts, TargetResultKind};
use crate::graph::BuildGraph;
use crate::model::{CacheKey, ContentHash, Input, TargetId, TargetSpec};
use crate::paths::AbsPath;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Built-in env contributions for the cache key (see TDD-0007).
const GIANT_VERSION: &str = env!("CARGO_PKG_VERSION");
const TARGET_TRIPLE: &str = env!("GIANT_TARGET_TRIPLE");

/// Schema version for the cache-key composition. Bump on any change.
const KEY_SCHEMA: &str = "v1";

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

/// Outcome for one target.
#[derive(Debug, Clone)]
enum TargetResult {
    Built {
        key: CacheKey,
        duration: Duration,
        outputs: Vec<OutputFile>,
    },
    CacheHit {
        key: CacheKey,
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
        key: CacheKey,
        output_hash: ContentHash,
    },
    /// `exists:` check returned 0 - the artifact lives outside the local
    /// filesystem (registry, S3, etc.). No local outputs were produced or
    /// restored; downstream consumers see the empty-outputs hash for this
    /// target.
    ExternalCacheHit {
        key: CacheKey,
        output_hash: ContentHash,
    },
    Failed {
        key: Option<CacheKey>,
        error: String,
    },
}

impl TargetResult {
    fn key(&self) -> Option<CacheKey> {
        match self {
            Self::Built { key, .. }
            | Self::CacheHit { key, .. }
            | Self::RemoteCacheHit { key, .. }
            | Self::ExternalCacheHit { key, .. } => Some(*key),
            Self::Failed { key, .. } => *key,
        }
    }

    fn kind(&self) -> TargetResultKind {
        match self {
            Self::Built { .. } => TargetResultKind::Built,
            Self::CacheHit { .. } => TargetResultKind::CacheHit,
            Self::RemoteCacheHit { .. } => TargetResultKind::RemoteCacheHit,
            Self::ExternalCacheHit { .. } => TargetResultKind::ExternalCacheHit,
            Self::Failed { .. } => TargetResultKind::Failed,
        }
    }

    /// Sentinel used when the dispatcher synthesises a Skipped completion
    /// inline (no worker spawned).
    fn skipped() -> Self {
        Self::Failed {
            key: None,
            error: "skipped".into(),
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
    let mut handled_completions: Vec<(TargetId, TargetResult)> = Vec::new();

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
                    None,
                    vec![],
                    Some(reason),
                )
                .await;
                handled_completions.push((tid, TargetResult::skipped()));
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
        for (id, _result) in handled_completions.drain(..) {
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

    // Lookup chain when not --fresh:
    //   1. local AC cache
    //   2. remote AC cache (feature-gated; populates local on hit)
    //   3. `exists:` check - for artifacts that live elsewhere (Docker
    //      registry, S3, etc.). The command runs with $GIANT_CACHE_KEY in
    //      env. Exit 0 → ExternalCacheHit, skip the build.
    //   4. run the target's command.
    let (result, output_hash) = if !ctx.fresh {
        if let Some((r, oh)) = try_cache_hit(&ctx, &spec.id, &key).await? {
            (r, Some(oh))
        } else if let Some((r, oh)) = try_remote_hit(&ctx, &spec.id, &key).await? {
            (r, Some(oh))
        } else if let Some((r, oh)) = try_exists_check(&ctx, &spec, key).await {
            (r, Some(oh))
        } else {
            let r = run_target(&ctx, &spec, key).await;
            let oh = result_output_hash(&r);
            (r, oh)
        }
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
        result.key().map(|k| k.to_hex()),
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

fn result_output_hash(r: &TargetResult) -> Option<ContentHash> {
    match r {
        TargetResult::Built { outputs, .. } => Some(compute_outputs_content_hash(outputs)),
        TargetResult::CacheHit { output_hash, .. }
        | TargetResult::RemoteCacheHit { output_hash, .. }
        | TargetResult::ExternalCacheHit { output_hash, .. } => Some(*output_hash),
        TargetResult::Failed { .. } => None,
    }
}

/// Breakdown of what went into a target's cache key. Populated when the
/// caller asks for it (see `compute_cache_key_with_breakdown`); the
/// dispatcher's `compute_cache_key` just discards it. Used by
/// `giant explain` to show users where the bytes that produced the
/// final hash came from.
#[derive(Debug, Clone)]
pub struct CacheKeyBreakdown {
    pub schema: String,
    pub command: String,
    pub cwd: String,
    pub user_env: std::collections::BTreeMap<String, String>,
    pub built_in_env: std::collections::BTreeMap<String, String>,
    pub file_inputs: Vec<FileInputContribution>,
    pub structural_inputs: Vec<StructuralContribution>,
    /// dep_id → output_content_hash. The caller fills this in *after*
    /// the hash is computed (the inner compose has no view of dep IDs).
    pub dep_outputs: std::collections::BTreeMap<TargetId, ContentHash>,
}

#[derive(Debug, Clone)]
pub struct FileInputContribution {
    pub rel_path: String,
    pub content_hash: ContentHash,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct StructuralContribution {
    pub files: Vec<String>,
    pub lines: Vec<String>,
    pub scope: Vec<String>,
    pub fingerprint: ContentHash,
}

/// Compute the cache key for a target. See TDD-0009 §Cache key composition.
///
/// `dep_output_hashes` is each direct dep's output content hash - *not* its
/// cache key. This is the early-cutoff property: byte-identical upstream
/// rebuilds leave downstream cache keys unchanged.
async fn compute_cache_key(
    spec: &TargetSpec,
    workspace_root: &AbsPath,
    cache: &LocalCache,
    dep_output_hashes: &[ContentHash],
) -> Result<CacheKey, ExecutorError> {
    let spec = spec.clone();
    let workspace_root = workspace_root.clone();
    let cache = cache.clone();
    let dep_output_hashes = dep_output_hashes.to_vec();
    let hash = tokio::task::spawn_blocking(move || {
        compose_cache_key_blocking(&spec, &workspace_root, &cache, &dep_output_hashes, None)
    })
    .await
    .map_err(|e| ExecutorError::Io(std::io::Error::other(e.to_string())))??;
    Ok(CacheKey::new(hash))
}

/// Like `compute_cache_key` but also returns a `CacheKeyBreakdown` so
/// callers (`giant explain`) can show users what fed into the hash.
pub async fn compute_cache_key_with_breakdown(
    spec: &TargetSpec,
    workspace_root: &AbsPath,
    cache: &LocalCache,
    dep_outputs: std::collections::BTreeMap<TargetId, ContentHash>,
) -> Result<(CacheKey, CacheKeyBreakdown), ExecutorError> {
    let spec = spec.clone();
    let workspace_root = workspace_root.clone();
    let cache = cache.clone();
    let dep_output_hashes: Vec<ContentHash> = dep_outputs.values().copied().collect();
    let (key, mut bd) = tokio::task::spawn_blocking(
        move || -> Result<(ContentHash, CacheKeyBreakdown), std::io::Error> {
            let mut bd = empty_breakdown(&spec);
            let h = compose_cache_key_blocking(
                &spec,
                &workspace_root,
                &cache,
                &dep_output_hashes,
                Some(&mut bd),
            )?;
            Ok((h, bd))
        },
    )
    .await
    .map_err(|e| ExecutorError::Io(std::io::Error::other(e.to_string())))??;
    // Fill in dep_outputs with real IDs (the inner compose has no view).
    bd.dep_outputs = dep_outputs;
    Ok((CacheKey::new(key), bd))
}

fn empty_breakdown(spec: &TargetSpec) -> CacheKeyBreakdown {
    let mut built_in = std::collections::BTreeMap::new();
    built_in.insert("GIANT_TARGET_TRIPLE".into(), TARGET_TRIPLE.into());
    built_in.insert("GIANT_VERSION".into(), GIANT_VERSION.into());
    CacheKeyBreakdown {
        schema: KEY_SCHEMA.to_string(),
        command: spec.command.clone(),
        cwd: spec.cwd.as_path().to_string_lossy().into_owned(),
        user_env: spec
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        built_in_env: built_in,
        file_inputs: Vec::new(),
        structural_inputs: Vec::new(),
        dep_outputs: std::collections::BTreeMap::new(),
    }
}

/// The actual cache-key hash composition. Sync; caller wraps in
/// spawn_blocking. If `breakdown` is `Some`, populates `file_inputs` and
/// `structural_inputs` alongside hashing so `giant explain` can show
/// exactly what bytes fed the hash.
fn compose_cache_key_blocking(
    spec: &TargetSpec,
    workspace_root: &AbsPath,
    cache: &LocalCache,
    dep_output_hashes: &[ContentHash],
    mut breakdown: Option<&mut CacheKeyBreakdown>,
) -> Result<ContentHash, std::io::Error> {
    let mut h = ContentHash::hasher();
    h.update(KEY_SCHEMA.as_bytes());
    h.update(b"\0");

    // command
    h.update(b"cmd\0");
    h.update(spec.command.as_bytes());
    h.update(b"\0");

    // cwd
    h.update(b"cwd\0");
    h.update(spec.cwd.as_path().to_string_lossy().as_bytes());
    h.update(b"\0");

    // env (sorted by key) + built-in target triple + version
    h.update(b"env\0");
    let mut env_keys: Vec<&String> = spec.env.keys().collect();
    env_keys.sort();
    for k in env_keys {
        h.update(k.as_bytes());
        h.update(b"=");
        h.update(spec.env[k].as_bytes());
        h.update(b"\0");
    }
    h.update(b"GIANT_TARGET_TRIPLE=");
    h.update(TARGET_TRIPLE.as_bytes());
    h.update(b"\0");
    h.update(b"GIANT_VERSION=");
    h.update(GIANT_VERSION.as_bytes());
    h.update(b"\0");

    // file inputs (expand globs, sort, hash content)
    h.update(b"file_inputs\0");
    let mut file_globs: Vec<&str> = Vec::new();
    // Collect structural-input specs as we walk; they hash separately
    // so the section is independent of file-input order.
    let mut structurals: Vec<StructuralSpec> = Vec::new();
    for input in &spec.inputs {
        match input {
            Input::File { glob } => {
                file_globs.push(glob.as_str());
            }
            Input::Structural {
                files,
                lines,
                scope,
            } => {
                structurals.push(StructuralSpec {
                    files: files.iter().map(|g| g.as_str().to_string()).collect(),
                    lines: lines.clone(),
                    scope: scope
                        .iter()
                        .map(|s| s.as_path().to_string_lossy().into_owned())
                        .collect(),
                });
            }
        }
    }
    // Canonicalise the structural specs ahead of time so the
    // parallel-computed section is deterministic on both sides of
    // the join. Sorts are cheap on a handful of specs.
    for s in &mut structurals {
        s.files.sort();
        s.lines.sort();
        s.scope.sort();
    }
    structurals.sort();

    // Independent work - run concurrently. The file-input branch
    // walks + hashes; the structural branch consults the gix
    // fast-path / sidecar. Neither touches the other's data, so
    // rayon::join gives us a clean wall-time overlap of the slower
    // of the two phases.
    let (file_result, structural_result): (
        Result<Vec<FileInputItem>, std::io::Error>,
        Result<Vec<StructuralFingerprint>, std::io::Error>,
    ) = rayon::join(
        || compute_file_inputs(workspace_root, cache, &file_globs),
        || compute_structural_inputs(workspace_root, cache, &spec.id, &structurals),
    );
    let file_items = file_result?;
    let structural_items = structural_result?;

    // Hash file_inputs section in deterministic order.
    for item in &file_items {
        h.update(item.rel_path.as_bytes());
        h.update(b"\0");
        h.update(item.content_hash.as_bytes());
        h.update(b"\0");
        if let Some(bd) = breakdown.as_deref_mut() {
            bd.file_inputs.push(FileInputContribution {
                rel_path: item.rel_path.clone(),
                content_hash: item.content_hash,
                size: item.size,
            });
        }
    }

    // Hash structural_inputs section. Same shape as before.
    h.update(b"structural_inputs\0");
    for (s, fp) in structurals.iter().zip(structural_items.iter()) {
        for f in &s.files {
            h.update(f.as_bytes());
            h.update(b"\0");
        }
        h.update(b"|\0");
        for l in &s.lines {
            h.update(l.as_bytes());
            h.update(b"\0");
        }
        h.update(b"|\0");
        for sc in &s.scope {
            h.update(sc.as_bytes());
            h.update(b"\0");
        }
        h.update(b"|\0");
        h.update(fp.fingerprint.as_bytes());
        h.update(b"\0");
        if let Some(bd) = breakdown.as_deref_mut() {
            bd.structural_inputs.push(StructuralContribution {
                files: s.files.clone(),
                lines: s.lines.clone(),
                scope: s.scope.clone(),
                fingerprint: fp.fingerprint,
            });
        }
    }

    // dep output content hashes (sorted). The section header changed
    // from "dep_keys" to "dep_outputs" - old cached entries from a
    // pre-early-cutoff build are stale and correctly miss.
    h.update(b"dep_outputs\0");
    let mut sorted: Vec<[u8; 32]> = dep_output_hashes.iter().map(|h| *h.as_bytes()).collect();
    sorted.sort();
    for hb in &sorted {
        h.update(hb);
        h.update(b"\0");
    }

    Ok(h.finalize())
}

/// One hashed file input ready to be folded into the cache-key hash.
/// Order is fixed by `compute_file_inputs` (sorted by `rel_path`).
struct FileInputItem {
    rel_path: String,
    content_hash: ContentHash,
    size: u64,
}

/// Fingerprint of one structural input - paired positionally with the
/// canonicalised `StructuralSpec` it was computed for.
struct StructuralFingerprint {
    fingerprint: ContentHash,
}

/// Walk + hash file inputs. Walk is parallel via `expand_globs_batched`;
/// hashing is parallel via rayon over the sorted path list. Sequential
/// hashing was visibly slow on warm runs of large discovery targets
/// (~1 ms per file × 70+ files); rayon brings that to ~10 ms.
fn compute_file_inputs(
    workspace_root: &AbsPath,
    cache: &LocalCache,
    globs: &[&str],
) -> Result<Vec<FileInputItem>, std::io::Error> {
    use rayon::prelude::*;

    let mut paths = expand_globs_batched(workspace_root.as_path(), globs, cache)?;
    paths.sort();
    paths.dedup();

    paths
        .par_iter()
        .map(|p| {
            let rel = p
                .strip_prefix(workspace_root.as_path())
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned();
            let content_hash = ContentHash::of_file(p)?;
            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            Ok(FileInputItem {
                rel_path: rel,
                content_hash,
                size,
            })
        })
        .collect()
}

/// Compute structural-input fingerprints. Sequential across structural
/// specs (usually only 1-2 per target) since `compute_fingerprint`
/// uses gix and a per-target sidecar - contention rather than gain if
/// parallelised at this layer.
fn compute_structural_inputs(
    workspace_root: &AbsPath,
    cache: &LocalCache,
    target_id: &TargetId,
    structurals: &[StructuralSpec],
) -> Result<Vec<StructuralFingerprint>, std::io::Error> {
    structurals
        .iter()
        .map(|s| {
            crate::structural::compute_fingerprint(
                workspace_root,
                cache,
                target_id,
                &s.files,
                &s.lines,
                &s.scope,
            )
            .map(|fingerprint| StructuralFingerprint { fingerprint })
            .map_err(|e| std::io::Error::other(e.to_string()))
        })
        .collect()
}

/// Internal canonical representation of one structural input, used to
/// build a deterministic section of the cache key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct StructuralSpec {
    files: Vec<String>,
    lines: Vec<String>,
    scope: Vec<String>,
}

/// Expand a set of input globs against the workspace at most once.
///
/// Without this, declaring N globs that contain `**` would walk the
/// workspace N times. We split the input list into:
///   - **literal paths**: no glob metachars; resolved via single `stat`
///   - **shallow globs**: no `**`; cheap, resolved by `glob::glob` (it
///     reads only directories the pattern actually traverses)
///   - **recursive globs**: contain `**`; matched against ONE shared
///     workspace walk (`walkdir`) that visits each directory once.
///
/// The shared walk also prunes known noise (`.git`, `.giant`, the
/// configured cache directory) to keep the visit budget bounded.
fn expand_globs_batched(
    workspace_root: &Path,
    globs: &[&str],
    cache: &LocalCache,
) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut recursive: Vec<String> = Vec::new();

    for &g in globs {
        if g.contains("**") {
            recursive.push(g.to_string());
        } else if has_glob_metachars(g) {
            // Shallow glob - let the glob crate handle it; it'll only
            // read the directories actually referenced by the pattern.
            let full = workspace_root.join(g).to_string_lossy().into_owned();
            let entries = glob::glob(&full)
                .map_err(|e| std::io::Error::other(format!("bad glob {g:?}: {e}")))?;
            for entry in entries.flatten() {
                if entry.is_file() {
                    out.push(entry);
                }
            }
        } else {
            // Literal path - one stat, no walk.
            let p = workspace_root.join(g);
            if p.is_file() {
                out.push(p);
            }
        }
    }

    if recursive.is_empty() {
        return Ok(out);
    }

    // Compile all recursive patterns into a single GlobSet. Internally
    // this builds Aho-Corasick literal prefilters across all patterns,
    // so matching a path against N globs costs roughly the same as
    // matching one. With per-pattern `glob::Pattern::matches_path` in a
    // loop, cost grew O(N) per file visited.
    let mut gs = globset::GlobSetBuilder::new();
    for g in &recursive {
        let glob = globset::Glob::new(g)
            .map_err(|e| std::io::Error::other(format!("bad glob {g:?}: {e}")))?;
        gs.add(glob);
    }
    let glob_set = Arc::new(
        gs.build()
            .map_err(|e| std::io::Error::other(format!("glob set build: {e}")))?,
    );

    // Shared parallel walk for all recursive patterns.
    //
    // Standard filters off so the semantics match `glob::glob` exactly:
    // declared inputs are matched against the literal filesystem, not
    // against what git tracks.
    let cache_dir = cache.root().as_path().to_path_buf();

    // Dot-prefixed VCS/tool dirs and common build outputs: walking
    // them is pure waste for input matching, and excluding them gives
    // a substantial speedup on real-world monorepos. If a user really
    // does have inputs underneath one of these, they'd be in the
    // gitignored territory and using giant from there is unusual; we
    // can revisit if a real case shows up.
    let skip_names: &[&str] = &[
        ".git",
        ".giant",
        ".direnv",
        ".devenv",
        "node_modules",
        "target",
    ];

    let matches: Arc<std::sync::Mutex<Vec<PathBuf>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    let workspace_root_owned = workspace_root.to_path_buf();

    ignore::WalkBuilder::new(workspace_root)
        .standard_filters(false)
        .hidden(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            let path = entry.path();
            if path == cache_dir.as_path() {
                return false;
            }
            if let Some(name) = entry.file_name().to_str()
                && skip_names.contains(&name)
            {
                return false;
            }
            true
        })
        .build_parallel()
        .run(|| {
            let matches = Arc::clone(&matches);
            let workspace_root = workspace_root_owned.clone();
            let glob_set = Arc::clone(&glob_set);
            Box::new(move |result| {
                let Ok(entry) = result else {
                    return ignore::WalkState::Continue;
                };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    return ignore::WalkState::Continue;
                }
                let path = entry.path();
                let rel = path.strip_prefix(&workspace_root).unwrap_or(path);
                if glob_set.is_match(rel) {
                    matches.lock().unwrap().push(path.to_path_buf());
                }
                ignore::WalkState::Continue
            })
        });

    let mut found = Arc::try_unwrap(matches)
        .map(|m| m.into_inner().unwrap())
        .unwrap_or_else(|arc| std::mem::take(&mut *arc.lock().unwrap()));
    out.append(&mut found);

    Ok(out)
}

fn has_glob_metachars(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// Try a local-cache lookup; if hit, restore outputs to workspace and
/// return the (result, output_content_hash) tuple. Returning the hash
/// here saves a re-read on the dispatcher side. Also replays captured
/// stdout/stderr (if the AC entry has blobs and replay is enabled) so
/// cache hits aren't silent.
async fn try_cache_hit(
    ctx: &TargetCtx,
    target_id: &TargetId,
    key: &CacheKey,
) -> Result<Option<(TargetResult, ContentHash)>, ExecutorError> {
    let cache = &ctx.cache;
    let workspace_root = &ctx.workspace_root;
    let Some(entry) = cache.get_ac(key).await? else {
        return Ok(None);
    };
    // Verify each output blob exists. If any are missing, treat as miss.
    for out in &entry.outputs {
        let Ok(bytes) = const_hex::decode(&out.content_hash) else {
            return Ok(None);
        };
        let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) else {
            return Ok(None);
        };
        let hash = ContentHash::from_raw(arr);
        if !cache.has_cas(&hash).await {
            return Ok(None);
        }
    }
    // Restore each output: write blob bytes into the workspace path.
    for out in &entry.outputs {
        let Ok(bytes) = const_hex::decode(&out.content_hash) else {
            continue;
        };
        let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) else {
            continue;
        };
        let hash = ContentHash::from_raw(arr);
        let Some(blob) = cache.get_cas(&hash).await? else {
            return Ok(None);
        };
        let path = workspace_root.as_path().join(out.rel_path_string());
        // `atomic_write_output` does the create_dir_all + tmp-then-rename
        // dance so a target writing over its own running binary works
        // (Linux ETXTBSY blocks open-for-write but allows rename-over).
        crate::cache::atomic_write_output(path, blob, out.executable).await?;
    }

    // Read the outputs_content_hash from the entry; this is the value
    // downstream targets feed into their cache keys (early cutoff).
    let output_hash = match const_hex::decode(&entry.outputs_content_hash)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
    {
        Some(arr) => ContentHash::from_raw(arr),
        None => {
            return Err(ExecutorError::Cache(crate::cache::CacheError::Corrupt {
                path: std::path::PathBuf::from(format!("ac/{}", key.to_hex())),
                detail: "outputs_content_hash field is not 32-byte hex".into(),
            }));
        }
    };

    if ctx.log_capture.replay {
        replay_logs(ctx, target_id, &entry).await;
    }

    Ok(Some((
        TargetResult::CacheHit {
            key: *key,
            output_hash,
        },
        output_hash,
    )))
}

/// Emit captured stdout/stderr from an AC entry as `TargetLog`
/// events. Missing blobs / empty entries are silently skipped - the
/// target predates log capture or had no output to begin with.
async fn replay_logs(ctx: &TargetCtx, target_id: &TargetId, entry: &crate::cache::AcEntry) {
    if let Some(hex) = entry.stdout_blob.as_deref() {
        replay_one_stream(ctx, target_id, hex, LogStream::Stdout).await;
    }
    if let Some(hex) = entry.stderr_blob.as_deref() {
        replay_one_stream(ctx, target_id, hex, LogStream::Stderr).await;
    }
}

async fn replay_one_stream(
    ctx: &TargetCtx,
    target_id: &TargetId,
    blob_hex: &str,
    stream: LogStream,
) {
    let Ok(bytes) = const_hex::decode(blob_hex) else {
        return;
    };
    let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) else {
        return;
    };
    let hash = ContentHash::from_raw(arr);
    let Ok(Some(blob)) = ctx.cache.get_cas(&hash).await else {
        return;
    };
    let text = String::from_utf8_lossy(&blob);
    for line in text.lines() {
        let _ = ctx
            .events
            .send(Event::TargetLog {
                build: ctx.build_id.clone(),
                id: target_id.clone(),
                stream,
                line: line.to_string(),
                truncated: false,
            })
            .await;
    }
}

/// Try the remote cache. Hits restore outputs to the workspace AND
/// populate the local cache so the next run hits locally without
/// touching the remote. Misses / errors return `Ok(None)` so the
/// dispatcher falls through.
///
/// Feature-gated: a no-op stub when `remote` is off.
#[cfg(feature = "remote")]
async fn try_remote_hit(
    ctx: &TargetCtx,
    target_id: &TargetId,
    key: &CacheKey,
) -> Result<Option<(TargetResult, ContentHash)>, ExecutorError> {
    let Some(remote) = ctx.remote.as_ref() else {
        return Ok(None);
    };
    let Ok(Some(entry)) = remote.get_ac(key).await else {
        return Ok(None);
    };

    // Fetch every referenced blob from remote, write to local CAS,
    // restore to the workspace. Any failure mid-restore → treat as a
    // miss (the local AC entry would be inconsistent if we wrote it).
    for out in &entry.outputs {
        let Ok(bytes) = const_hex::decode(&out.content_hash) else {
            return Ok(None);
        };
        let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) else {
            return Ok(None);
        };
        let hash = ContentHash::from_raw(arr);

        let blob = match remote.get_cas(&hash).await {
            Ok(Some(b)) => b,
            _ => return Ok(None),
        };
        // Verify the blob we got actually hashes to what the AC claims.
        // Cheap insurance against a corrupted or hostile server.
        if ContentHash::of_bytes(&blob) != hash {
            tracing::warn!(
                "remote CAS blob {} content does not match its name; treating as miss",
                hash.to_hex()
            );
            return Ok(None);
        }
        ctx.cache.put_cas(blob.clone()).await?;

        let dst = ctx.workspace_root.as_path().join(&out.path);
        crate::cache::atomic_write_output(dst, blob, out.executable).await?;
    }

    // Fetch the captured stdout/stderr blobs into local CAS too, so a
    // future local hit can replay without touching the remote. Missing
    // blobs on the remote are tolerated - older entries may not have
    // logs at all.
    for hex in entry
        .stdout_blob
        .iter()
        .chain(entry.stderr_blob.iter())
        .map(|s| s.as_str())
    {
        let Ok(bytes) = const_hex::decode(hex) else {
            continue;
        };
        let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) else {
            continue;
        };
        let hash = ContentHash::from_raw(arr);
        if ctx.cache.has_cas(&hash).await {
            continue;
        }
        if let Ok(Some(blob)) = remote.get_cas(&hash).await {
            if ContentHash::of_bytes(&blob) == hash {
                ctx.cache.put_cas(blob).await?;
            }
        }
    }

    // Write the AC entry to local cache too so the next run goes
    // straight to the local fast path.
    ctx.cache.put_ac(key, &entry).await?;

    let output_hash = match const_hex::decode(&entry.outputs_content_hash)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
    {
        Some(arr) => ContentHash::from_raw(arr),
        None => return Ok(None),
    };

    if ctx.log_capture.replay {
        replay_logs(ctx, target_id, &entry).await;
    }

    Ok(Some((
        TargetResult::RemoteCacheHit {
            key: *key,
            output_hash,
        },
        output_hash,
    )))
}

/// No-op stub used when the `remote` feature is off so the dispatcher
/// chain stays uniform.
#[cfg(not(feature = "remote"))]
async fn try_remote_hit(
    _ctx: &TargetCtx,
    _target_id: &TargetId,
    _key: &CacheKey,
) -> Result<Option<(TargetResult, ContentHash)>, ExecutorError> {
    Ok(None)
}

/// Run the target's `exists:` shell command, if any, to ask "does this
/// artifact already live somewhere external?" (registry, S3, …).
///
/// - No `exists:` declared → return `None` (the dispatcher falls through
///   to `run_target`).
/// - `exists:` exits 0 → external cache hit; the build is skipped.
/// - `exists:` exits non-zero → cache miss; the dispatcher runs the
///   target normally.
/// - The command failing to spawn → log a warning and fall through; we
///   prefer a clean miss over a confusing skipped/failed signal.
///
/// `$GIANT_CACHE_KEY` is in env so users can craft commands like
/// `docker manifest inspect reg.io/img:$GIANT_CACHE_KEY` that key the
/// artifact name on Giant's cache identity.
async fn try_exists_check(
    ctx: &TargetCtx,
    spec: &TargetSpec,
    key: CacheKey,
) -> Option<(TargetResult, ContentHash)> {
    let exists_cmd = spec.exists.as_deref()?;

    let cwd = ctx.workspace_root.as_path().join(spec.cwd.as_path());
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(exists_cmd)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIANT_CACHE_KEY", key.to_hex())
        .env("GIANT_WORKSPACE_ROOT", ctx.workspace_root.as_path());
    apply_color_env(&mut cmd);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target=%spec.id, error=%e, "exists: command failed to spawn");
            return None;
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // exists-check output is purely informational - don't capture
    // it to CAS (no AC entry to attach it to).
    let pump_o = pump_lines(
        stdout,
        ctx.events.clone(),
        ctx.build_id.clone(),
        spec.id.clone(),
        LogStream::Stdout,
        0,
        false,
    );
    let pump_e = pump_lines(
        stderr,
        ctx.events.clone(),
        ctx.build_id.clone(),
        spec.id.clone(),
        LogStream::Stderr,
        0,
        false,
    );

    let status = tokio::select! {
        s = child.wait() => s,
        _ = ctx.cancel.cancelled() => {
            let _ = child.kill().await;
            return None;
        }
    };
    let (_, _) = tokio::join!(pump_o, pump_e);

    match status {
        Ok(s) if s.success() => {
            // The artifact lives elsewhere; we contribute the empty-outputs
            // hash to downstream cache keys. If a future use case needs
            // local outputs *and* an exists check, we can extend this.
            let oh = compute_outputs_content_hash(&[]);
            Some((
                TargetResult::ExternalCacheHit {
                    key,
                    output_hash: oh,
                },
                oh,
            ))
        }
        _ => None,
    }
}

/// Run a target's command end-to-end and store outputs.
async fn run_target(ctx: &TargetCtx, spec: &TargetSpec, key: CacheKey) -> TargetResult {
    let started = Instant::now();

    let cwd = ctx.workspace_root.as_path().join(spec.cwd.as_path());
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&spec.command)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIANT_CACHE_KEY", key.to_hex())
        .env("GIANT_WORKSPACE_ROOT", ctx.workspace_root.as_path());

    // Color preservation: most modern CLIs disable color when they detect
    // stdout is a pipe (we use Stdio::piped). These env vars are the de
    // facto signals to force color anyway. Tools that strictly check
    // isatty(stdout) are unaffected - pty: true (v0.2) covers that case.
    apply_color_env(&mut cmd);

    // Per-target env overrides take precedence over our color signals.
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    // Ensure the parent directory of each declared output exists. Many
    // commands use `> outdir/file` redirects; with parent dirs absent
    // the shell fails before the user's command sees the workspace.
    // Cheap, idempotent, and matches the "you declared this output,
    // the engine handles the boilerplate" philosophy.
    for out_path in &spec.outputs {
        let abs = ctx.workspace_root.as_path().join(out_path.as_path());
        if let Some(parent) = abs.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return TargetResult::Failed {
                key: Some(key),
                error: format!("spawn failed: {e}"),
            };
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let target_id = spec.id.clone();
    let build_id = ctx.build_id.clone();

    let pump_stdout = pump_lines(
        stdout,
        ctx.events.clone(),
        build_id.clone(),
        target_id.clone(),
        LogStream::Stdout,
        ctx.log_capture.cap_bytes,
        ctx.log_capture.capture,
    );
    let pump_stderr = pump_lines(
        stderr,
        ctx.events.clone(),
        build_id,
        target_id,
        LogStream::Stderr,
        ctx.log_capture.cap_bytes,
        ctx.log_capture.capture,
    );

    let status = tokio::select! {
        s = child.wait() => s,
        _ = ctx.cancel.cancelled() => {
            let _ = child.kill().await;
            return TargetResult::Failed { key: Some(key), error: "cancelled".into() };
        }
    };
    let (stdout_bytes, stderr_bytes) = tokio::join!(pump_stdout, pump_stderr);

    let exit = match status {
        Ok(s) => s,
        Err(e) => {
            return TargetResult::Failed {
                key: Some(key),
                error: format!("wait failed: {e}"),
            };
        }
    };
    if !exit.success() {
        return TargetResult::Failed {
            key: Some(key),
            error: format!("exit code {}", exit.code().unwrap_or(-1)),
        };
    }

    // Capture and store outputs.
    let outputs = match capture_outputs(&ctx.cache, &ctx.workspace_root, spec).await {
        Ok(o) => o,
        Err(e) => {
            return TargetResult::Failed {
                key: Some(key),
                error: format!("capture outputs: {e}"),
            };
        }
    };

    // Persist captured stdout/stderr to CAS so a future cache hit can
    // replay them. Empty streams (or capture disabled) → None.
    let stdout_blob = if ctx.log_capture.capture && !stdout_bytes.is_empty() {
        match ctx.cache.put_cas(stdout_bytes.clone()).await {
            Ok(h) => Some(h.to_hex()),
            Err(e) => {
                return TargetResult::Failed {
                    key: Some(key),
                    error: format!("write stdout blob: {e}"),
                };
            }
        }
    } else {
        None
    };
    let stderr_blob = if ctx.log_capture.capture && !stderr_bytes.is_empty() {
        match ctx.cache.put_cas(stderr_bytes.clone()).await {
            Ok(h) => Some(h.to_hex()),
            Err(e) => {
                return TargetResult::Failed {
                    key: Some(key),
                    error: format!("write stderr blob: {e}"),
                };
            }
        }
    } else {
        None
    };

    // Write AC entry.
    let outputs_hash = compute_outputs_content_hash(&outputs);
    let ac = AcEntry {
        schema: crate::cache::AC_SCHEMA,
        target_id: spec.id.as_str().to_string(),
        cache_key: key.to_hex(),
        command: spec.command.clone(),
        cwd: spec.cwd.as_path().to_string_lossy().into_owned(),
        outputs: outputs.iter().map(OutputFile::to_entry).collect(),
        outputs_content_hash: outputs_hash.to_hex(),
        stdout_blob,
        stderr_blob,
        exit_code: 0,
        duration_ms: started.elapsed().as_millis() as u64,
        built_at: chrono::Utc::now().to_rfc3339(),
        built_by: None,
    };
    if let Err(e) = ctx.cache.put_ac(&key, &ac).await {
        return TargetResult::Failed {
            key: Some(key),
            error: format!("cache write: {e}"),
        };
    }

    // Queue background remote upload. Reads each blob back from local
    // CAS - they're hot in the OS page cache from being written moments
    // ago, so the read is cheap. The build never waits on this.
    #[cfg(feature = "remote")]
    if let Some(tx) = ctx.upload_tx.as_ref() {
        let mut blobs = Vec::with_capacity(outputs.len() + 2);
        for o in &outputs {
            match ctx.cache.get_cas(&o.content_hash).await {
                Ok(Some(bytes)) => blobs.push((o.content_hash, bytes)),
                _ => {
                    tracing::warn!("local CAS read failed for upload of {}", o.rel_path);
                }
            }
        }
        // Also ship captured stdout/stderr so other machines hitting
        // this AC entry can replay the logs.
        for hex in ac
            .stdout_blob
            .iter()
            .chain(ac.stderr_blob.iter())
            .map(|s| s.as_str())
        {
            if let Ok(bytes) = const_hex::decode(hex) {
                if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
                    let h = ContentHash::from_raw(arr);
                    if let Ok(Some(b)) = ctx.cache.get_cas(&h).await {
                        blobs.push((h, b));
                    }
                }
            }
        }
        let job = crate::remote::UploadJob {
            cache_key: key,
            ac_entry: ac,
            blobs,
        };
        // try_send: if the uploader is backlogged, drop the job rather
        // than block the build. Better local progress than a stuck queue.
        let _ = tx.try_send(job);
    }

    TargetResult::Built {
        key,
        duration: started.elapsed(),
        outputs,
    }
}

/// Read declared output files, hash them, and store them in CAS.
async fn capture_outputs(
    cache: &LocalCache,
    workspace_root: &AbsPath,
    spec: &TargetSpec,
) -> Result<Vec<OutputFile>, std::io::Error> {
    let mut outputs = Vec::with_capacity(spec.outputs.len());
    for out_path in &spec.outputs {
        let abs = workspace_root.as_path().join(out_path.as_path());
        let metadata = tokio::fs::metadata(&abs).await?;
        if metadata.is_dir() {
            return Err(std::io::Error::other(format!(
                "declared output {:?} is a directory; v1 supports single files",
                out_path.as_path()
            )));
        }
        let bytes = tokio::fs::read(&abs).await?;
        let size = bytes.len() as u64;
        let executable;
        let mode;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = metadata.permissions().mode();
            executable = m & 0o111 != 0;
            mode = format!("{:o}", m & 0o7777);
        }
        #[cfg(not(unix))]
        {
            executable = false;
            mode = "0644".into();
        }
        let hash = cache
            .put_cas(bytes)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        outputs.push(OutputFile {
            rel_path: out_path.as_path().to_string_lossy().into_owned(),
            content_hash: hash,
            size,
            executable,
            mode,
        });
    }
    Ok(outputs)
}

/// Hash of the sorted outputs vector, for early-cutoff and AC metadata.
fn compute_outputs_content_hash(outputs: &[OutputFile]) -> ContentHash {
    let mut h = ContentHash::hasher();
    let mut sorted: Vec<&OutputFile> = outputs.iter().collect();
    sorted.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    for o in sorted {
        h.update(o.rel_path.as_bytes());
        h.update(b"\0");
        h.update(o.content_hash.as_bytes());
        h.update(b"\0");
    }
    h.finalize()
}

impl OutputFile {
    fn to_entry(&self) -> OutputEntry {
        OutputEntry {
            path: self.rel_path.clone(),
            content_hash: self.content_hash.to_hex(),
            size: self.size,
            executable: self.executable,
            mode: self.mode.clone(),
            symlink_target: None,
        }
    }
}

impl OutputEntry {
    fn rel_path_string(&self) -> String {
        self.path.clone()
    }
}

/// Set color-forcing env vars on a child command. Each variable is the
/// well-known signal for an ecosystem; tools that don't recognise theirs
/// just ignore it. The user's `env:` map is applied *after* these and
/// can override any of them.
fn apply_color_env(cmd: &mut Command) {
    // npm / node ecosystem
    cmd.env("FORCE_COLOR", "1");
    // BSD / macOS convention; respected by many CLIs
    cmd.env("CLICOLOR_FORCE", "1");
    cmd.env("CLICOLOR", "1");
    // python's "do you want color?" hint
    cmd.env("PY_COLORS", "1");
    // cargo
    cmd.env("CARGO_TERM_COLOR", "always");
    // many TUI-aware tools probe TERM; set something modest if absent.
    // Don't override if the parent already passed a TERM through.
    if std::env::var_os("TERM").is_none() {
        cmd.env("TERM", "xterm-256color");
    }
}

/// Pump stdout/stderr from a child into log events while also
/// accumulating the bytes into a buffer that can be written to CAS
/// after the target completes (for cache-hit replay).
///
/// Returns the captured bytes. Accumulation stops at `cap_bytes` per
/// stream; lines beyond the cap still stream live but aren't written
/// to the blob. A `[truncated]` marker is appended so a future replay
/// shows the cutoff.
async fn pump_lines<R>(
    reader: Option<R>,
    events: EventSender,
    build_id: String,
    target_id: TargetId,
    stream: LogStream,
    cap_bytes: usize,
    capture: bool,
) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut buf: Vec<u8> = if capture {
        Vec::with_capacity(1024)
    } else {
        Vec::new()
    };
    let mut hit_cap = false;
    let Some(r) = reader else { return buf };
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if capture && !hit_cap {
            // +1 for the newline we re-append.
            let needed = line.len() + 1;
            if buf.len() + needed <= cap_bytes {
                buf.extend_from_slice(line.as_bytes());
                buf.push(b'\n');
            } else {
                buf.extend_from_slice(b"[giant: log truncated at capture cap]\n");
                hit_cap = true;
            }
        }
        let truncated = line.len() > 8 * 1024;
        let line = if truncated {
            line[..8 * 1024].to_string()
        } else {
            line
        };
        let _ = events
            .send(Event::TargetLog {
                build: build_id.clone(),
                id: target_id.clone(),
                stream,
                line,
                truncated,
            })
            .await;
    }
    buf
}

async fn emit(events: &EventSender, ev: Event) {
    let _ = events.send(ev).await;
}

#[allow(clippy::too_many_arguments)]
async fn emit_finished(
    events: &EventSender,
    build_id: &str,
    id: &TargetId,
    result: TargetResultKind,
    duration_ms: u64,
    cache_key: Option<String>,
    outputs: Vec<String>,
    error: Option<String>,
) {
    let _ = cache_key; // for now we don't include cache_key in the event payload
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
