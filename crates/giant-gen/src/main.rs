//! giant-gen - the generator runner porcelain (TDD-0022).
//!
//! Reached as `giant gen` via core's PATH dispatch (ADR-0021), like
//! `giant-task`. It reads the workspace's `giant-gen.yaml` `generators:` list,
//! resolves each to a command, and runs them - writing in place, or with
//! `--check` into a scratch dir to diff against the committed tree. It never
//! inspects a generator's output beyond enforcing `giant.<name>.yaml`
//! filename ownership. The engine has no part in any of this (ADR-0024).

mod check;
mod config;
mod run;

use anyhow::Result;
use clap::Parser;
use config::Generator;
use std::path::Path;

#[derive(Parser, Debug)]
#[command(name = "giant-gen", about = "Run a workspace's configured generators")]
struct Cli {
    /// Generators to run (default: every generator declared in giant-gen.yaml).
    names: Vec<String>,

    /// Check for staleness instead of writing: regenerate into a scratch dir
    /// and diff each generator's giant.<name>.yaml files against the tree.
    #[arg(long)]
    check: bool,

    /// Path to giant.yaml (defaults to walking up from the current directory).
    #[arg(long)]
    config: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() {
    match real_main().await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("giant gen: {e:#}");
            std::process::exit(1);
        }
    }
}

async fn real_main() -> Result<i32> {
    let cli = Cli::parse();
    let (cfg, root) = giant::Config::load_root(cli.config.as_deref())?;
    let declared = config::load(&root)?;
    let selected = select(&declared, &cli.names)?;

    if selected.is_empty() {
        eprintln!("giant gen: no generators declared in giant-gen.yaml");
        return Ok(0);
    }

    if cli.check {
        check::run(&selected, &root, &cfg.state.dir).await
    } else {
        run_all(&selected, &root).await
    }
}

/// Resolve the requested names to declared generators. Empty selection means
/// "all declared"; an unknown name is a clear error.
fn select(all: &[Generator], names: &[String]) -> Result<Vec<Generator>> {
    if names.is_empty() {
        return Ok(all.to_vec());
    }
    names
        .iter()
        .map(|n| {
            all.iter().find(|g| &g.name == n).cloned().ok_or_else(|| {
                anyhow::anyhow!("no generator named '{n}' is declared in giant-gen.yaml")
            })
        })
        .collect()
}

/// Run every selected generator concurrently, each writing in place.
async fn run_all(selected: &[Generator], root: &Path) -> Result<i32> {
    let mut handles = Vec::with_capacity(selected.len());
    for g in selected {
        let g = g.clone();
        let root = root.to_path_buf();
        handles.push(tokio::spawn(async move {
            run::run_live(&g, &root, &root).await
        }));
    }
    let mut failures = 0;
    for h in handles {
        if !h.await?? {
            failures += 1;
        }
    }
    Ok(if failures > 0 { 1 } else { 0 })
}
