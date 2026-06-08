//! Cache-key explanation: walk a target's dep closure, recomputing each
//! ancestor's cache key (consulting the cache for prior AC entries so the
//! displayed key matches what an actual build would compute).
//!
//! This is the compute behind `query.explain` / `query.status` (ADR-0033). The
//! `giant explain` porcelain renders the `query.explained` event these produce;
//! it does not call this directly (ADR-0034).

use crate::cache::LocalCache;
use crate::executor::{CacheKeyBreakdown, compute_cache_key_with_breakdown};
use crate::graph::BuildGraph;
use crate::model::{CacheKey, ContentHash, TargetId};
use crate::paths::AbsPath;
use std::collections::BTreeMap;

/// Compute a target's (cache_key, breakdown, own output hash). Component-based
/// so the session's `query.explain` handler can reuse it (ADR-0033), the same
/// way it reuses `walk_target`.
pub(crate) async fn breakdown_for_target(
    graph: &BuildGraph,
    cache: &LocalCache,
    workspace_root: &AbsPath,
    target_id: &TargetId,
    memo: &mut BTreeMap<TargetId, (CacheKey, Option<ContentHash>)>,
) -> anyhow::Result<(CacheKey, CacheKeyBreakdown, Option<ContentHash>)> {
    let (key, output_hash) = walk_target(graph, cache, workspace_root, target_id, memo).await?;

    let dep_outputs: BTreeMap<TargetId, ContentHash> = graph
        .direct_deps(target_id)
        .into_iter()
        .filter_map(|d| memo.get(&d).and_then(|(_, oh)| oh.map(|h| (d, h))))
        .collect();
    let spec = graph
        .get(target_id)
        .ok_or_else(|| anyhow::anyhow!("target {target_id:?} missing"))?;
    let (verify_key, breakdown) =
        compute_cache_key_with_breakdown(spec, workspace_root, cache, dep_outputs).await?;
    debug_assert_eq!(key, verify_key, "two paths for the same key disagreed");
    Ok((key, breakdown, output_hash))
}

/// Compute (cache_key, output_content_hash) for a target by walking
/// its dep closure. Each dep contributes its real output hash (read
/// from its AC entry if present, sentinel-empty if not). Memoised via
/// `memo` so shared ancestors get computed once. Shared with the
/// session's `query.status` handler (ADR-0033).
pub(crate) async fn walk_target(
    graph: &BuildGraph,
    cache: &LocalCache,
    workspace_root: &AbsPath,
    id: &TargetId,
    memo: &mut BTreeMap<TargetId, (CacheKey, Option<ContentHash>)>,
) -> anyhow::Result<(CacheKey, Option<ContentHash>)> {
    if let Some(v) = memo.get(id) {
        return Ok(*v);
    }
    let spec = graph
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("missing target {id:?}"))?;
    let direct = graph.direct_deps(id);

    // Box the recursive future - async recursion needs an indirection.
    let mut dep_outputs: BTreeMap<TargetId, ContentHash> = BTreeMap::new();
    for dep in direct {
        let (_, oh) = Box::pin(walk_target(graph, cache, workspace_root, &dep, memo)).await?;
        if let Some(h) = oh {
            dep_outputs.insert(dep, h);
        }
    }

    // Compute this target's cache key from its (possibly empty) dep
    // output hashes. If a dep has no AC entry, we omit it - which is
    // *not* what an actual build would do (it would have a real hash
    // after the dep ran). For explain on never-built targets that's
    // OK; the key shown matches what the next build will compute given
    // the current cache state.
    let (key, _) =
        compute_cache_key_with_breakdown(spec, workspace_root, cache, dep_outputs).await?;

    // Look up the AC entry to get the real output hash if cached.
    let output_hash = match cache.get_ac(&key).await? {
        Some(ac) => ContentHash::from_hex(&ac.outputs_content_hash),
        None => None,
    };

    memo.insert(id.clone(), (key, output_hash));
    Ok((key, output_hash))
}
