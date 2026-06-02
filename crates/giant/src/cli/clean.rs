//! `giant clean` - clear the local cache.
//!
//! Two modes:
//!
//! - **All** (`giant clean`): wipe the whole cache directory. Prints
//!   size + entry counts first, asks for confirmation (`-y` skips).
//! - **Selective**: filter by target-ID pattern (`giant clean go:test:*`),
//!   by age (`giant clean --older-than 7d`), or both. Both filters
//!   compose - pattern AND age - and only AC entries matching both
//!   are deleted. Orphaned CAS blobs (referenced only by deleted AC
//!   entries) get GC'd via the existing eviction pass.
//!
//! Doesn't run discovery - just reads cache.dir from the config and
//! scans `ac/`.

use clap::Args;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::prep;

#[derive(Args, Debug)]
pub struct CleanArgs {
    /// Restrict to AC entries whose `target_id` matches this glob.
    /// Repeatable; matches are unioned. Empty = all entries.
    #[arg(value_name = "PATTERN")]
    pub patterns: Vec<String>,

    /// Restrict to AC entries older than this duration. Accepts
    /// `30s` / `5m` / `2h` / `7d` (default unit: seconds if bare integer).
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    pub older_than: Option<Duration>,

    /// Skip the confirmation prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// Print what would be deleted, then exit without touching anything.
    #[arg(long)]
    pub dry_run: bool,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let (num, unit) = s
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse::<u64>()
        .map_err(|e| format!("not a number in {s:?}: {e}"))
        .map(|n| (n, s.trim_start_matches(|c: char| c.is_ascii_digit())))?;
    let secs = match unit {
        "" | "s" => num,
        "m" => num * 60,
        "h" => num * 3600,
        "d" => num * 86_400,
        other => return Err(format!("unknown duration unit '{other}'; use s/m/h/d")),
    };
    Ok(Duration::from_secs(secs))
}

pub async fn execute(args: CleanArgs, global: &super::GlobalFlags) -> anyhow::Result<()> {
    let (config, _workspace_root) = prep::load_config(global.config.as_deref())?;
    let cache_root = prep::resolve_cache_dir(&config.cache.dir)?;

    // Selective mode if either filter is set; otherwise full wipe.
    let selective = !args.patterns.is_empty() || args.older_than.is_some();
    if selective {
        return execute_selective(&cache_root, &args).await;
    }

    let stats = collect_stats(&cache_root);
    print_summary(&cache_root, &stats);

    if args.dry_run {
        println!("\nDry run - nothing deleted.");
        return Ok(());
    }
    if stats.total_entries == 0 {
        println!("\nNothing to clean.");
        return Ok(());
    }

    if !args.yes && !confirm("Delete?")? {
        return Ok(());
    }

    match std::fs::remove_dir_all(&cache_root) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => anyhow::bail!("failed to clean {}: {e}", cache_root.display()),
    }
    std::fs::create_dir_all(&cache_root)?;

    println!(
        "Cleared {} ({}).",
        cache_root.display(),
        human_bytes(stats.total_bytes)
    );

    Ok(())
}

async fn execute_selective(cache_root: &Path, args: &CleanArgs) -> anyhow::Result<()> {
    let patterns: Vec<glob::Pattern> = args
        .patterns
        .iter()
        .map(|p| glob::Pattern::new(p))
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("bad pattern: {e}"))?;

    let cutoff = args.older_than.map(|d| SystemTime::now() - d);
    let ac_dir = cache_root.join("ac");
    if !ac_dir.is_dir() {
        println!("Nothing to clean (no AC entries at {}).", ac_dir.display());
        return Ok(());
    }

    let mut matches: Vec<(std::path::PathBuf, String, u64)> = Vec::new();
    let mut bytes_total: u64 = 0;
    for entry in walkdir::WalkDir::new(&ac_dir).min_depth(2).max_depth(2) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path().to_path_buf();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        if let Some(cutoff) = cutoff
            && meta.modified().map(|t| t > cutoff).unwrap_or(true)
        {
            continue;
        }

        let target_id = match read_target_id(&path) {
            Some(s) => s,
            None => continue,
        };
        if !patterns.is_empty() && !patterns.iter().any(|p| p.matches(&target_id)) {
            continue;
        }

        let size = meta.len();
        bytes_total += size;
        matches.push((path, target_id, size));
    }

    if matches.is_empty() {
        println!("Nothing matched.");
        return Ok(());
    }

    matches.sort_by(|a, b| a.1.cmp(&b.1));
    println!(
        "Will delete {} AC entr{} ({}):",
        matches.len(),
        if matches.len() == 1 { "y" } else { "ies" },
        human_bytes(bytes_total),
    );
    for (_, tid, size) in matches.iter().take(20) {
        println!("  {} ({})", tid, human_bytes(*size));
    }
    if matches.len() > 20 {
        println!("  … and {} more", matches.len() - 20);
    }
    println!("\nReferenced CAS blobs become eligible for eviction; run a build to GC them.");

    if args.dry_run {
        println!("\nDry run - nothing deleted.");
        return Ok(());
    }
    if !args.yes && !confirm("Delete?")? {
        return Ok(());
    }

    let mut errs = 0;
    for (path, _, _) in &matches {
        if let Err(e) = std::fs::remove_file(path) {
            tracing::warn!(error = %e, path = %path.display(), "failed to delete AC entry");
            errs += 1;
        }
    }
    println!(
        "Deleted {}/{} ({}).",
        matches.len() - errs,
        matches.len(),
        human_bytes(bytes_total),
    );
    Ok(())
}

fn confirm(prompt: &str) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("stdin is not a terminal; pass -y to skip the confirmation prompt");
    }
    print!("\n{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let yes = matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if !yes {
        println!("Cancelled.");
    }
    Ok(yes)
}

/// Read just the `target_id` field from an AC entry JSON without
/// deserialising the whole thing. Returns None on parse error so the
/// scan keeps going.
fn read_target_id(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("target_id")?.as_str().map(str::to_string)
}

#[derive(Default)]
struct Stats {
    ac: u64,
    cas: u64,
    log: u64,
    total_bytes: u64,
    total_entries: u64,
}

fn collect_stats(root: &Path) -> Stats {
    let mut s = Stats::default();
    if !root.is_dir() {
        return s;
    }
    // Walk each of the known subdirs, summing entry counts and bytes.
    // Anything else under the root (version file, tmp/) gets bytes-only
    // accounting.
    for entry in walkdir::WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        s.total_bytes += size;
        s.total_entries += 1;
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy();
        if rel.starts_with("ac/") {
            s.ac += 1;
        } else if rel.starts_with("cas/") {
            s.cas += 1;
        } else if rel.starts_with("log/") {
            s.log += 1;
        }
    }
    s
}

fn print_summary(root: &Path, s: &Stats) {
    println!("Cache: {}", root.display());
    if s.total_entries == 0 {
        println!("  empty.");
        return;
    }
    println!("  size:    {}", human_bytes(s.total_bytes));
    println!("  entries: {} total", s.total_entries);
    if s.ac > 0 || s.cas > 0 || s.log > 0 {
        println!("    {} AC, {} CAS, {} log", s.ac, s.cas, s.log);
    }
}

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
