//! `giant explain <target>` - show what feeds a target's cache key.
//!
//! The first thing you reach for when "why did this rebuild?" comes up.
//! Walks the dep graph from the target back, recomputing each ancestor's
//! cache key (consulting the cache to find prior AC entries → real
//! output_content_hash → real input to the next key) so the displayed
//! key matches what an actual build would compute.

use crate::cache::{AcEntry, LocalCache};
use crate::executor::{CacheKeyBreakdown, compute_cache_key_with_breakdown};
use crate::graph::BuildGraph;
use crate::model::{CacheKey, ContentHash, TargetId};
use crate::paths::AbsPath;
use clap::Args;
use std::collections::BTreeMap;
use std::io::Write;

use super::prep;

#[derive(Args, Debug)]
pub struct ExplainArgs {
    /// Target ID to explain.
    pub target: String,
}

pub async fn execute(args: ExplainArgs, global: &super::GlobalFlags) -> anyhow::Result<()> {
    let (tx, sink) = prep::null_event_sink();
    let cancel = tokio_util::sync::CancellationToken::new();
    let parallelism = prep::num_cpus_estimate();

    let prepared = match prep::prepare(
        global.config.as_deref(),
        parallelism,
        global.fresh,
        tx,
        cancel,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            sink.abort();
            return Err(e);
        }
    };
    let _ = sink.await;

    let target_id = TargetId::new(&args.target);
    if prepared.graph.get(&target_id).is_none() {
        anyhow::bail!("target {:?} not found in graph", args.target);
    }

    // Recursively compute cache keys for the target's dep closure so
    // dep_outputs reflects real prior-build state where available.
    let mut memo: BTreeMap<TargetId, (CacheKey, Option<ContentHash>)> = BTreeMap::new();
    let (key, output_hash) = walk_target(
        &prepared.graph,
        &prepared.cache,
        &prepared.workspace_root,
        &target_id,
        &mut memo,
    )
    .await?;

    let dep_outputs: BTreeMap<TargetId, ContentHash> = prepared
        .graph
        .direct_deps(&target_id)
        .into_iter()
        .filter_map(|d| memo.get(&d).and_then(|(_, oh)| oh.map(|h| (d, h))))
        .collect();
    let spec = prepared.graph.get(&target_id).expect("checked above");
    let (verify_key, breakdown) = compute_cache_key_with_breakdown(
        spec,
        &prepared.workspace_root,
        &prepared.cache,
        dep_outputs,
    )
    .await?;
    debug_assert_eq!(
        key, verify_key,
        "two paths for the same key disagreed - bug in explain"
    );

    let ac = prepared.cache.get_ac(&key).await.ok().flatten();

    print_breakdown(&target_id, key, &breakdown, ac.as_ref(), output_hash);
    Ok(())
}

/// Compute (cache_key, output_content_hash) for a target by walking
/// its dep closure. Each dep contributes its real output hash (read
/// from its AC entry if present, sentinel-empty if not). Memoised so
/// shared ancestors get computed once.
async fn walk_target(
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
    let (key, _) = compute_cache_key_with_breakdown(spec, workspace_root, cache, dep_outputs)
        .await?;

    // Look up the AC entry to get the real output hash if cached.
    let output_hash = match cache.get_ac(&key).await? {
        Some(ac) => const_hex::decode(&ac.outputs_content_hash)
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
            .map(ContentHash::from_raw),
        None => None,
    };

    memo.insert(id.clone(), (key, output_hash));
    Ok((key, output_hash))
}

fn print_breakdown(
    target_id: &TargetId,
    key: CacheKey,
    bd: &CacheKeyBreakdown,
    ac: Option<&AcEntry>,
    own_output_hash: Option<ContentHash>,
) {
    let stdout = std::io::stdout();
    let mut w = stdout.lock();

    let _ = writeln!(w, "target:      {target_id}");
    let _ = writeln!(w, "cache key:   {}", key.to_hex());
    match ac {
        Some(entry) => {
            let _ = writeln!(
                w,
                "cache state: HIT (built {}, {}ms, exit {})",
                entry.built_at, entry.duration_ms, entry.exit_code
            );
        }
        None => {
            let _ = writeln!(w, "cache state: miss (next build will populate)");
        }
    }
    let _ = writeln!(w);

    let _ = writeln!(w, "command:");
    let _ = writeln!(w, "  {}", bd.command);
    let _ = writeln!(w);

    let cwd_display = if bd.cwd.is_empty() {
        "<workspace root>".to_string()
    } else {
        bd.cwd.clone()
    };
    let _ = writeln!(w, "cwd:         {cwd_display}");
    let _ = writeln!(w, "sandbox:     {}", bd.sandbox);
    let _ = writeln!(w);

    let total_env = bd.user_env.len() + bd.built_in_env.len();
    let _ = writeln!(w, "env ({total_env}):");
    for (k, v) in &bd.user_env {
        let _ = writeln!(w, "  {k}={v}");
    }
    for (k, v) in &bd.built_in_env {
        let _ = writeln!(w, "  {k}={v}  (built-in)");
    }
    let _ = writeln!(w);

    let _ = writeln!(w, "file inputs ({}):", bd.file_inputs.len());
    for f in &bd.file_inputs {
        let _ = writeln!(
            w,
            "  {:<60} {}  {}",
            f.rel_path,
            &f.content_hash.to_hex()[..16],
            human_bytes(f.size)
        );
    }
    if bd.file_inputs.is_empty() {
        let _ = writeln!(w, "  (none)");
    }
    let _ = writeln!(w);

    let _ = writeln!(w, "structural inputs ({}):", bd.structural_inputs.len());
    for (i, s) in bd.structural_inputs.iter().enumerate() {
        let _ = writeln!(w, "  [{}] files: {}", i + 1, s.files.join(", "));
        let _ = writeln!(w, "      lines: {:?}", s.lines);
        if !s.scope.is_empty() {
            let _ = writeln!(w, "      scope: {}", s.scope.join(", "));
        }
        let _ = writeln!(w, "      fingerprint: {}", s.fingerprint.to_hex());
    }
    if bd.structural_inputs.is_empty() {
        let _ = writeln!(w, "  (none)");
    }
    let _ = writeln!(w);

    let _ = writeln!(w, "deps ({}):", bd.dep_outputs.len());
    for (d, oh) in &bd.dep_outputs {
        let _ = writeln!(w, "  {:<60} {}", d, &oh.to_hex()[..16]);
    }
    if bd.dep_outputs.is_empty() {
        let _ = writeln!(w, "  (none)");
    }
    let _ = writeln!(w);

    if let Some(ac) = ac {
        let _ = writeln!(w, "outputs (from cache, {}):", ac.outputs.len());
        for o in &ac.outputs {
            let _ = writeln!(
                w,
                "  {:<60} {}  {} {}",
                o.path,
                &o.content_hash[..16],
                human_bytes(o.size),
                o.mode
            );
        }
        if let Some(oh) = own_output_hash {
            let _ = writeln!(w);
            let _ = writeln!(w, "outputs_content_hash: {}", oh.to_hex());
            let _ = writeln!(
                w,
                "  (downstream targets feed this into their cache keys - early cutoff)"
            );
        }
    }

    let _ = w.flush();
}

/// Human-readable byte sizes; short and stable. Plenty good for explain
/// output; if we ever need precise sizes elsewhere we add a richer
/// formatter.
fn human_bytes(n: u64) -> String {
    const KB: u64 = 1_024;
    const MB: u64 = KB * 1_024;
    const GB: u64 = MB * 1_024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}
