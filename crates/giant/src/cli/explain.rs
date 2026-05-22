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
    #[arg(add = clap_complete::ArgValueCompleter::new(super::dynamic::complete_target_ids))]
    pub target: String,

    /// Compare this target's cache-key breakdown against another
    /// target's. Useful for "why does target X have a different key
    /// than target Y?" The output is a unified diff of command, cwd,
    /// env, file inputs, structural inputs, and dep outputs.
    #[arg(
        long,
        value_name = "OTHER_TARGET",
        add = clap_complete::ArgValueCompleter::new(super::dynamic::complete_target_ids)
    )]
    pub diff: Option<String>,
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

    let mut memo: BTreeMap<TargetId, (CacheKey, Option<ContentHash>)> = BTreeMap::new();
    let (key, breakdown, output_hash) = breakdown_for_target(&prepared, &target_id, &mut memo).await?;

    if let Some(other) = &args.diff {
        let other_id = TargetId::new(other);
        if prepared.graph.get(&other_id).is_none() {
            anyhow::bail!("target {:?} not found in graph", other);
        }
        let (other_key, other_bd, _) = breakdown_for_target(&prepared, &other_id, &mut memo).await?;
        print_diff(
            (&target_id, key, &breakdown),
            (&other_id, other_key, &other_bd),
        );
        return Ok(());
    }

    let ac = prepared.cache.get_ac(&key).await.ok().flatten();
    print_breakdown(&target_id, key, &breakdown, ac.as_ref(), output_hash);
    Ok(())
}

async fn breakdown_for_target(
    prepared: &prep::Prepared,
    target_id: &TargetId,
    memo: &mut BTreeMap<TargetId, (CacheKey, Option<ContentHash>)>,
) -> anyhow::Result<(CacheKey, CacheKeyBreakdown, Option<ContentHash>)> {
    let (key, output_hash) = walk_target(
        &prepared.graph,
        &prepared.cache,
        &prepared.workspace_root,
        target_id,
        memo,
    )
    .await?;

    let dep_outputs: BTreeMap<TargetId, ContentHash> = prepared
        .graph
        .direct_deps(target_id)
        .into_iter()
        .filter_map(|d| memo.get(&d).and_then(|(_, oh)| oh.map(|h| (d, h))))
        .collect();
    let spec = prepared
        .graph
        .get(target_id)
        .ok_or_else(|| anyhow::anyhow!("target {target_id:?} missing"))?;
    let (verify_key, breakdown) = compute_cache_key_with_breakdown(
        spec,
        &prepared.workspace_root,
        &prepared.cache,
        dep_outputs,
    )
    .await?;
    debug_assert_eq!(key, verify_key, "two paths for the same key disagreed");
    Ok((key, breakdown, output_hash))
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
    let (key, _) =
        compute_cache_key_with_breakdown(spec, workspace_root, cache, dep_outputs).await?;

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

/// Render a side-by-side diff of two breakdowns: any field that
/// differs gets a section. Identical fields are summarised so the
/// output stays focused on what's actually different.
fn print_diff(
    left: (&TargetId, CacheKey, &CacheKeyBreakdown),
    right: (&TargetId, CacheKey, &CacheKeyBreakdown),
) {
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    let (lid, lkey, lbd) = left;
    let (rid, rkey, rbd) = right;

    let _ = writeln!(w, "comparing:");
    let _ = writeln!(w, "  -  {lid}  ({})", lkey.to_hex());
    let _ = writeln!(w, "  +  {rid}  ({})", rkey.to_hex());
    if lkey == rkey {
        let _ = writeln!(w);
        let _ = writeln!(w, "(cache keys are identical - no diff to show)");
        return;
    }
    let _ = writeln!(w);

    diff_scalar(&mut w, "command", &lbd.command, &rbd.command);
    diff_scalar(&mut w, "cwd", &lbd.cwd, &rbd.cwd);

    diff_env_map(&mut w, "env (user)", &lbd.user_env, &rbd.user_env);
    diff_env_map(&mut w, "env (built-in)", &lbd.built_in_env, &rbd.built_in_env);

    diff_file_inputs(&mut w, &lbd.file_inputs, &rbd.file_inputs);

    diff_dep_outputs(&mut w, &lbd.dep_outputs, &rbd.dep_outputs);

    // Structural inputs: compare by index, ordered the same as in the
    // key. We treat them as opaque blobs keyed on fingerprint - that's
    // what feeds the cache key. A mismatch points the user at one of
    // the discovery-time aggregations.
    diff_structural(&mut w, &lbd.structural_inputs, &rbd.structural_inputs);

    let _ = w.flush();
}

fn diff_scalar<W: Write>(w: &mut W, label: &str, left: &str, right: &str) {
    if left == right {
        return;
    }
    let _ = writeln!(w, "── {label} ──");
    let _ = writeln!(w, "  - {left}");
    let _ = writeln!(w, "  + {right}");
    let _ = writeln!(w);
}

fn diff_env_map<W: Write>(
    w: &mut W,
    label: &str,
    left: &BTreeMap<String, String>,
    right: &BTreeMap<String, String>,
) {
    let mut keys: std::collections::BTreeSet<&String> = left.keys().collect();
    keys.extend(right.keys());
    let mut wrote_header = false;
    for k in keys {
        match (left.get(k), right.get(k)) {
            (Some(l), Some(r)) if l == r => {}
            (l, r) => {
                if !wrote_header {
                    let _ = writeln!(w, "── {label} ──");
                    wrote_header = true;
                }
                let _ = writeln!(
                    w,
                    "  - {k}={}",
                    l.map(String::as_str).unwrap_or("<unset>")
                );
                let _ = writeln!(
                    w,
                    "  + {k}={}",
                    r.map(String::as_str).unwrap_or("<unset>")
                );
            }
        }
    }
    if wrote_header {
        let _ = writeln!(w);
    }
}

fn diff_file_inputs<W: Write>(
    w: &mut W,
    left: &[crate::executor::FileInputContribution],
    right: &[crate::executor::FileInputContribution],
) {
    let left_map: BTreeMap<&str, &crate::executor::FileInputContribution> =
        left.iter().map(|f| (f.rel_path.as_str(), f)).collect();
    let right_map: BTreeMap<&str, &crate::executor::FileInputContribution> =
        right.iter().map(|f| (f.rel_path.as_str(), f)).collect();
    let mut paths: std::collections::BTreeSet<&str> = left_map.keys().copied().collect();
    paths.extend(right_map.keys().copied());
    let mut wrote_header = false;
    for p in paths {
        match (left_map.get(p), right_map.get(p)) {
            (Some(l), Some(r)) if l.content_hash == r.content_hash && l.size == r.size => {}
            (l, r) => {
                if !wrote_header {
                    let _ = writeln!(w, "── file inputs ──");
                    wrote_header = true;
                }
                let _ = writeln!(
                    w,
                    "  - {:<60} {}",
                    p,
                    l.map(|f| f.content_hash.to_hex()[..16].to_string())
                        .unwrap_or_else(|| "<absent>".into())
                );
                let _ = writeln!(
                    w,
                    "  + {:<60} {}",
                    p,
                    r.map(|f| f.content_hash.to_hex()[..16].to_string())
                        .unwrap_or_else(|| "<absent>".into())
                );
            }
        }
    }
    if wrote_header {
        let _ = writeln!(w);
    }
}

fn diff_dep_outputs<W: Write>(
    w: &mut W,
    left: &BTreeMap<TargetId, ContentHash>,
    right: &BTreeMap<TargetId, ContentHash>,
) {
    let mut deps: std::collections::BTreeSet<&TargetId> = left.keys().collect();
    deps.extend(right.keys());
    let mut wrote_header = false;
    for d in deps {
        match (left.get(d), right.get(d)) {
            (Some(l), Some(r)) if l == r => {}
            (l, r) => {
                if !wrote_header {
                    let _ = writeln!(w, "── dep outputs ──");
                    wrote_header = true;
                }
                let _ = writeln!(
                    w,
                    "  - {:<60} {}",
                    d,
                    l.map(|h| h.to_hex()[..16].to_string())
                        .unwrap_or_else(|| "<absent>".into())
                );
                let _ = writeln!(
                    w,
                    "  + {:<60} {}",
                    d,
                    r.map(|h| h.to_hex()[..16].to_string())
                        .unwrap_or_else(|| "<absent>".into())
                );
            }
        }
    }
    if wrote_header {
        let _ = writeln!(w);
    }
}

fn diff_structural<W: Write>(
    w: &mut W,
    left: &[crate::executor::StructuralContribution],
    right: &[crate::executor::StructuralContribution],
) {
    let n = left.len().max(right.len());
    let mut wrote_header = false;
    for i in 0..n {
        let l = left.get(i);
        let r = right.get(i);
        let same = match (l, r) {
            (Some(a), Some(b)) => a.fingerprint == b.fingerprint,
            _ => false,
        };
        if same {
            continue;
        }
        if !wrote_header {
            let _ = writeln!(w, "── structural inputs ──");
            wrote_header = true;
        }
        match l {
            Some(s) => {
                let _ = writeln!(w, "  - [{}] {}", i + 1, s.fingerprint.to_hex());
                let _ = writeln!(w, "      files: {}", s.files.join(", "));
            }
            None => {
                let _ = writeln!(w, "  - [{}] <absent>", i + 1);
            }
        }
        match r {
            Some(s) => {
                let _ = writeln!(w, "  + [{}] {}", i + 1, s.fingerprint.to_hex());
                let _ = writeln!(w, "      files: {}", s.files.join(", "));
            }
            None => {
                let _ = writeln!(w, "  + [{}] <absent>", i + 1);
            }
        }
    }
    if wrote_header {
        let _ = writeln!(w);
    }
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
