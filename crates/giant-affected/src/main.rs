//! giant-affected - list targets that would rebuild given a set of changed
//! files. Doesn't run anything; one target id per line on stdout, for piping
//! into xargs / jq in CI.
//!
//! Porcelain (ADR-0034), dispatched as `giant affected`. Links the giant
//! library for the workspace load + selection + git change detection - the
//! same code the in-core command used, just out of the binary.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser;
use giant::git;
use giant::selection::{self, SelectionOpts, TestMode};
use giant::{TargetId, prepare};

#[derive(Parser, Debug)]
#[command(
    name = "giant-affected",
    about = "List targets that would rebuild given changed files"
)]
struct Cli {
    /// Git ref used as the diff baseline (worktree vs this ref, plus
    /// untracked-not-gitignored files).
    #[arg(long, value_name = "REF")]
    base: Option<String>,

    /// Explicit changed-file list. Repeatable; overrides --base.
    #[arg(long, value_name = "PATH")]
    file: Vec<PathBuf>,

    /// Restrict to targets matching these patterns (the build selection
    /// language: exact ids, globs, `!`-exclusions).
    patterns: Vec<String>,

    /// Include only targets carrying this tag. Repeatable.
    #[arg(long = "tag", value_name = "TAG")]
    tags: Vec<String>,

    /// Exclude targets carrying this tag. Repeatable.
    #[arg(long = "no-tag", value_name = "TAG")]
    no_tags: Vec<String>,

    /// Restrict to test targets.
    #[arg(long, conflicts_with = "with_tests")]
    tests_only: bool,

    /// Include test targets alongside non-test ones.
    #[arg(long)]
    with_tests: bool,

    /// Path to giant.yaml (defaults to walking up from the current directory).
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("giant affected: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let prepared = prepare(cli.config.as_deref()).await?;

    let changed = resolve_changed_files(&cli, prepared.workspace_root.as_path())?;
    let changed_refs: Vec<&Path> = changed.iter().map(|p| p.as_path()).collect();
    let affected = selection::affected_targets(&prepared.graph, &changed_refs);

    // Pattern selection over the full graph, then intersect with the affected
    // set: a glob matching nothing is a silent empty, a typo'd exact id still
    // bails (resolve_patterns handles that).
    let opts = SelectionOpts {
        tags: cli.tags.clone(),
        no_tags: cli.no_tags.clone(),
    };
    let test_mode = match (cli.tests_only, cli.with_tests) {
        (true, _) => TestMode::Only,
        (_, true) => TestMode::Include,
        _ => TestMode::Exclude,
    };
    let pattern_set =
        selection::resolve_patterns(&prepared.graph, &cli.patterns, test_mode, &opts)?;
    let mut out: Vec<TargetId> = pattern_set
        .into_iter()
        .filter(|id| affected.contains(id))
        .collect();
    out.sort();

    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    for id in &out {
        let _ = writeln!(w, "{id}");
    }
    let _ = w.flush();
    Ok(())
}

fn resolve_changed_files(cli: &Cli, workspace_root: &Path) -> Result<Vec<PathBuf>> {
    if !cli.file.is_empty() {
        return Ok(cli
            .file
            .iter()
            .map(|p| {
                p.strip_prefix(workspace_root)
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|_| p.clone())
            })
            .collect());
    }
    let base = cli.base.as_deref().ok_or_else(|| {
        anyhow::anyhow!("`giant affected` requires --base <ref> or one or more --file <path>")
    })?;
    git::affected_files_since(workspace_root, base)
        .map_err(|e| anyhow::anyhow!("affected detection: {e}"))
}
