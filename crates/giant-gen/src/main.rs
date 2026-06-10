//! giant-gen - the generator runner porcelain.
//!
//! Reached as `giant gen` via core's PATH dispatch, like
//! `giant-task`. It reads the workspace root `giant.yaml`'s `generate:` list
//! and runs each entry - the built-in Starlark host on a
//! `giant.star`, or an external generator command - writing in
//! place, or with `--check` into a scratch dir to diff against the committed
//! tree. It never inspects output beyond enforcing `giant.<name>.yaml`
//! filename ownership. The engine has no part in any of this.

mod check;
mod config;
mod link;
mod run;
mod star;

use anyhow::{Context, Result};
use clap::Parser;
use config::Generator;
use std::path::Path;

#[derive(Parser, Debug)]
#[command(name = "giant-gen", about = "Run a workspace's configured generators")]
struct Cli {
    /// Generators to run (default: every entry in giant.yaml's generate:).
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
    // `vendor` is a sub-verb, not a generator name; dispatch before clap so the
    // positional generator list keeps its plain shape (`giant gen [names...]`).
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("vendor") {
        // Off the runtime thread: vendoring a pinned module does blocking I/O.
        let names = args[1..].to_vec();
        return tokio::task::spawn_blocking(move || vendor(&names)).await?;
    }

    let cli = Cli::parse();
    let (cfg, root) = giant::Config::load_root(cli.config.as_deref())?;
    let declared = config::load(&root)?;
    let selected = select(&declared, &cli.names)?;

    if selected.is_empty() {
        eprintln!(
            "giant gen: nothing to generate (add a `generate:` list to giant.yaml or a giant.star at the workspace root)"
        );
        return Ok(0);
    }

    let std = std_source(&cfg, &root)?;
    if cli.check {
        check::run(&selected, &root, &cfg.state.dir, &std).await
    } else {
        run_all(&selected, &root, &std).await
    }
}

/// The workspace's `@std//` resolver, from the root config's `std:` block:
/// a local directory, or a pin caching fetched modules under the cache dir.
/// `GIANT_STD` and the install-relative collection still override.
fn std_source(cfg: &giant::Config, root: &Path) -> Result<star::StdSource> {
    let (path, pin) = match config::load_std(root)? {
        Some(config::StdDecl::Local { path }) => {
            // Tilde-expand (the same rules as cache.dir), then anchor a
            // relative path at the workspace root.
            let p = giant::resolve_cache_dir(&path)?;
            let dir = if p.is_absolute() { p } else { root.join(p) };
            if !dir.is_dir() {
                anyhow::bail!("std.path {} is not a directory", dir.display());
            }
            (Some(dir), None)
        }
        Some(config::StdDecl::Pinned { repo, rev }) => {
            let cache = giant::resolve_cache_dir(&cfg.cache.dir)?.join("std");
            (None, Some(star::StdPin::new(repo, rev, cache)))
        }
        None => (None, None),
    };
    Ok(star::StdSource::new(star::detect_dir(path), pin))
}

/// Copy stdlib modules from giant's std collection into the workspace's `star/`
/// dir so they can be edited and pinned in-repo, then loaded with
/// `load("star/<name>")`.
fn vendor(names: &[String]) -> Result<i32> {
    if names.is_empty() {
        anyhow::bail!("giant gen vendor: name a module, e.g. `giant gen vendor go.star`");
    }
    let (cfg, root) = giant::Config::load_root(None)?;
    let std = std_source(&cfg, &root)?;
    let dest = root.join("star");
    std::fs::create_dir_all(&dest)?;
    for name in names {
        let src = std.source(name)?;
        std::fs::write(dest.join(name), src).with_context(|| format!("vendoring {name}"))?;
        eprintln!("vendored {name} -> star/{name}");
    }
    Ok(0)
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
            all.iter()
                .find(|g| g.name() == n)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no generator named '{n}' is declared in generate:"))
        })
        .collect()
}

/// Run every selected generator concurrently, each writing in place.
async fn run_all(selected: &[Generator], root: &Path, std: &star::StdSource) -> Result<i32> {
    let mut handles = Vec::with_capacity(selected.len());
    for g in selected {
        let g = g.clone();
        let root = root.to_path_buf();
        let std = std.clone();
        handles.push(tokio::spawn(async move {
            run::run_live(&g, &root, &root, &std).await
        }));
    }
    let mut failures = 0;
    for h in handles {
        if !h.await?? {
            failures += 1;
        }
    }
    if failures > 0 {
        return Ok(1);
    }

    // Phase 2: resolve deps over the whole emitted tree and write them into the
    // generated files. Skipped above on any generator failure, since
    // the tree would be partial.
    let root = root.to_path_buf();
    match tokio::task::spawn_blocking(move || link::run(&root)).await? {
        Ok(n) => {
            if n > 0 {
                println!("link\tresolved deps in {n} file(s)");
            }
            Ok(0)
        }
        Err(e) => {
            eprintln!("giant gen: link: {e:#}");
            Ok(1)
        }
    }
}
