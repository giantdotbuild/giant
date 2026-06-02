//! `giant logs <target>` - replay the captured stdout/stderr from the
//! last cached invocation of a target.
//!
//! Reads the AC entry by the target's current cache key, pulls the
//! `stdout_blob`/`stderr_blob` CAS hashes out, streams the blob
//! contents back to stdout/stderr (or stdout merged, with `--merged`).
//!
//! Useful answer to "I cache-hit, what did the build SAY that one
//! time?" without needing to bust the cache. Honors the same
//! `cache.capture_logs` / `cache.replay_logs` policy: logs only exist
//! if capture is on.

use clap::Args;
use std::io::Write;

use crate::model::{ContentHash, TargetId};

use super::prep;

#[derive(Args, Debug)]
pub struct LogsArgs {
    /// Target ID to show logs for.
    #[arg(add = clap_complete::ArgValueCompleter::new(super::dynamic::complete_target_ids))]
    pub target: String,

    /// Inspect a specific AC entry by its cache-key hex. Defaults to
    /// the target's current cache key (what a build right now would
    /// look up).
    #[arg(long, value_name = "HEX")]
    pub key: Option<String>,

    /// Print stdout only.
    #[arg(long, conflicts_with_all = ["stderr_only", "merged"])]
    pub stdout_only: bool,

    /// Print stderr only.
    #[arg(long, conflicts_with_all = ["stdout_only", "merged"])]
    pub stderr_only: bool,

    /// Merge stdout + stderr to the current stdout, in stdout-then-stderr
    /// order. Loses the distinction between streams but useful for
    /// piping into a single consumer.
    #[arg(long)]
    pub merged: bool,
}

pub async fn execute(args: LogsArgs, global: &super::GlobalFlags) -> anyhow::Result<()> {
    let prepared = prep::prepare(global.config.as_deref()).await?;

    let target_id = TargetId::new(args.target.clone());
    let spec = prepared
        .graph
        .get(&target_id)
        .ok_or_else(|| anyhow::anyhow!("unknown target: {}", target_id.as_str()))?
        .clone();

    let key = match args.key {
        Some(hex) => parse_cache_key(&hex)?,
        None => {
            let dep_outputs =
                collect_dep_output_hashes(&prepared.graph, &target_id, &prepared.cache).await?;
            let (key, _) = crate::executor::compute_cache_key_with_breakdown(
                &spec,
                &prepared.workspace_root,
                &prepared.cache,
                dep_outputs,
            )
            .await?;
            key
        }
    };

    let entry = prepared.cache.get_ac(&key).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "no cached AC entry for {} at key {}\n\
                 (run the target first, or check capture_logs is on)",
            target_id.as_str(),
            key.to_hex(),
        )
    })?;

    let want_stdout = !args.stderr_only;
    let want_stderr = !args.stdout_only;

    if want_stdout && let Some(hex) = entry.stdout_blob.as_deref() {
        write_blob(&prepared.cache, hex, args.merged, false).await?;
    }
    if want_stderr && let Some(hex) = entry.stderr_blob.as_deref() {
        write_blob(&prepared.cache, hex, args.merged, true).await?;
    }

    if entry.stdout_blob.is_none() && entry.stderr_blob.is_none() {
        eprintln!(
            "no captured logs for {} (cache_key={}, exit_code={}). \
             cache.capture_logs may have been off when this entry was written.",
            target_id.as_str(),
            key.to_hex(),
            entry.exit_code,
        );
    }

    Ok(())
}

async fn write_blob(
    cache: &crate::cache::LocalCache,
    hex: &str,
    merged: bool,
    is_stderr: bool,
) -> anyhow::Result<()> {
    let hash = ContentHash::from_hex(hex)
        .ok_or_else(|| anyhow::anyhow!("malformed cache-key hex in AC entry: {hex}"))?;
    let blob = cache
        .get_cas(&hash)
        .await?
        .ok_or_else(|| anyhow::anyhow!("log blob {hex} missing from CAS (evicted?)"))?;

    if merged || !is_stderr {
        std::io::stdout().write_all(&blob)?;
    } else {
        std::io::stderr().write_all(&blob)?;
    }
    Ok(())
}

fn parse_cache_key(hex: &str) -> anyhow::Result<crate::model::CacheKey> {
    let hash = ContentHash::from_hex(hex)
        .ok_or_else(|| anyhow::anyhow!("--key must be 64 hex chars (32 bytes), got {hex:?}"))?;
    Ok(crate::model::CacheKey::new(hash))
}

/// Compute `dep_outputs` for `target_id` by reading each direct dep's
/// AC entry (early-cutoff hash). Mirrors what the executor does at
/// dispatch time, just for one target.
async fn collect_dep_output_hashes(
    graph: &crate::graph::BuildGraph,
    target_id: &TargetId,
    cache: &crate::cache::LocalCache,
) -> anyhow::Result<std::collections::BTreeMap<TargetId, ContentHash>> {
    let spec = graph
        .get(target_id)
        .ok_or_else(|| anyhow::anyhow!("missing target: {}", target_id.as_str()))?;

    let mut out = std::collections::BTreeMap::new();
    for dep_id in &spec.deps {
        let dep_spec = graph
            .get(dep_id)
            .ok_or_else(|| anyhow::anyhow!("missing dep: {}", dep_id.as_str()))?
            .clone();
        // Recurse to get the dep's own dep outputs first.
        let inner = Box::pin(collect_dep_output_hashes(graph, dep_id, cache)).await?;
        let (dep_key, _) = crate::executor::compute_cache_key_with_breakdown(
            &dep_spec,
            &cache.root().clone(),
            cache,
            inner,
        )
        .await?;
        if let Some(entry) = cache.get_ac(&dep_key).await? {
            let hash = ContentHash::from_hex(&entry.outputs_content_hash)
                .ok_or_else(|| anyhow::anyhow!("bad outputs_content_hash in AC"))?;
            out.insert(dep_id.clone(), hash);
        }
    }
    Ok(out)
}
