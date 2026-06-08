//! giant-logs - replay the captured stdout/stderr from the last cached
//! invocation of a target. Answer "what did the build say?" without busting
//! the cache.
//!
//! Porcelain (ADR-0034), dispatched as `giant logs`. Reads the AC entry by the
//! target's current cache key, pulls the `stdout_blob` / `stderr_blob` CAS
//! hashes, and streams the blob contents back. Honors the same
//! `cache.capture_logs` policy: logs only exist if capture was on.

use std::io::Write;

use anyhow::Result;
use clap::Parser;
use giant::executor::compute_cache_key_with_breakdown;
use giant::{BuildGraph, CacheKey, ContentHash, LocalCache, TargetId, prepare};

#[derive(Parser, Debug)]
#[command(
    name = "giant-logs",
    about = "Replay a target's captured logs from the last cached build"
)]
struct Cli {
    /// Target ID to show logs for.
    target: String,

    /// Inspect a specific AC entry by its cache-key hex. Defaults to the
    /// target's current cache key.
    #[arg(long, value_name = "HEX")]
    key: Option<String>,

    /// Print stdout only.
    #[arg(long, conflicts_with_all = ["stderr_only", "merged"])]
    stdout_only: bool,

    /// Print stderr only.
    #[arg(long, conflicts_with_all = ["stdout_only", "merged"])]
    stderr_only: bool,

    /// Merge stdout + stderr to the current stdout, in stdout-then-stderr order.
    #[arg(long)]
    merged: bool,

    /// Path to giant.yaml (defaults to walking up from the current directory).
    #[arg(long)]
    config: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("giant logs: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let prepared = prepare(cli.config.as_deref()).await?;

    let target_id = TargetId::new(cli.target.clone());
    let spec = prepared
        .graph
        .get(&target_id)
        .ok_or_else(|| anyhow::anyhow!("unknown target: {}", target_id.as_str()))?
        .clone();

    let key = match cli.key {
        Some(hex) => parse_cache_key(&hex)?,
        None => {
            let dep_outputs =
                collect_dep_output_hashes(&prepared.graph, &target_id, &prepared.cache).await?;
            let (key, _) = compute_cache_key_with_breakdown(
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

    let want_stdout = !cli.stderr_only;
    let want_stderr = !cli.stdout_only;
    if want_stdout && let Some(hex) = entry.stdout_blob.as_deref() {
        write_blob(&prepared.cache, hex, cli.merged, false).await?;
    }
    if want_stderr && let Some(hex) = entry.stderr_blob.as_deref() {
        write_blob(&prepared.cache, hex, cli.merged, true).await?;
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

async fn write_blob(cache: &LocalCache, hex: &str, merged: bool, is_stderr: bool) -> Result<()> {
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

fn parse_cache_key(hex: &str) -> Result<CacheKey> {
    let hash = ContentHash::from_hex(hex)
        .ok_or_else(|| anyhow::anyhow!("--key must be 64 hex chars (32 bytes), got {hex:?}"))?;
    Ok(CacheKey::new(hash))
}

/// Compute `dep_outputs` for `target_id` by reading each direct dep's AC entry
/// (early-cutoff hash). Mirrors what the executor does at dispatch time.
async fn collect_dep_output_hashes(
    graph: &BuildGraph,
    target_id: &TargetId,
    cache: &LocalCache,
) -> Result<std::collections::BTreeMap<TargetId, ContentHash>> {
    let spec = graph
        .get(target_id)
        .ok_or_else(|| anyhow::anyhow!("missing target: {}", target_id.as_str()))?;

    let mut out = std::collections::BTreeMap::new();
    for dep_id in &spec.deps {
        let dep_spec = graph
            .get(dep_id)
            .ok_or_else(|| anyhow::anyhow!("missing dep: {}", dep_id.as_str()))?
            .clone();
        let inner = Box::pin(collect_dep_output_hashes(graph, dep_id, cache)).await?;
        let (dep_key, _) =
            compute_cache_key_with_breakdown(&dep_spec, &cache.root().clone(), cache, inner)
                .await?;
        if let Some(entry) = cache.get_ac(&dep_key).await? {
            let hash = ContentHash::from_hex(&entry.outputs_content_hash)
                .ok_or_else(|| anyhow::anyhow!("bad outputs_content_hash in AC"))?;
            out.insert(dep_id.clone(), hash);
        }
    }
    Ok(out)
}
