//! `giant clean` - clear the local cache.
//!
//! Prints the cache size and entry counts first, then asks for
//! confirmation. `-y` skips the prompt (for scripts). Non-interactive
//! (stdin not a tty) always requires `-y`.
//!
//! Doesn't need to run discovery - just reads cache.dir from the
//! config and removes the directory contents. The engine recreates
//! the layout on the next build.

use clap::Args;
use std::io::{IsTerminal, Write};
use std::path::Path;

use super::prep;

#[derive(Args, Debug)]
pub struct CleanArgs {
    /// Skip the confirmation prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// Print what would be deleted, then exit without touching anything.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn execute(args: CleanArgs, global: &super::GlobalFlags) -> anyhow::Result<()> {
    let (config, _workspace_root) = prep::load_config(global.config.as_deref())?;
    let cache_root = prep::resolve_cache_dir(&config.cache.dir)?;

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

    if !args.yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("stdin is not a terminal; pass -y to skip the confirmation prompt");
        }
        print!("\nDelete? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    // remove_dir_all + recreate gives us a clean known-empty layout
    // for the next build's LocalCache::open. NotFound on remove is fine
    // (e.g. someone deleted the dir between our scan and the delete).
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

    // Workspace .giant/ state (build logs, discovery outputs) lives
    // *outside* the cache dir; we don't touch it. If users want to
    // wipe state too, they can `rm -rf .giant/` themselves.

    Ok(())
}

#[derive(Default)]
struct Stats {
    ac: u64,
    cas: u64,
    structural: u64,
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
        } else if rel.starts_with("structural/") {
            s.structural += 1;
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
    if s.ac > 0 || s.cas > 0 || s.structural > 0 || s.log > 0 {
        println!(
            "    {} AC, {} CAS, {} structural, {} log",
            s.ac, s.cas, s.structural, s.log
        );
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
