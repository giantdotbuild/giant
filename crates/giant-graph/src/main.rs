//! giant-graph - the dependency-graph inspection porcelain.
//!
//! Reached as `giant graph` via core's PATH dispatch (ADR-0021), like
//! `giant-gen` / `giant-task`. The build graph is fully static on disk since
//! ADR-0032 (explicit `deps:`, no engine inference), so this tool needs nothing
//! from the engine at runtime: it scans the workspace config, builds the same
//! `BuildGraph` the engine would, and renders it. Text modes (list / tree /
//! compact) plus `--format dot|mermaid|json` for external renderers.

mod render;

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use giant::{BuildGraph, Config, TargetId};

use render::Direction;

#[derive(Parser, Debug)]
#[command(name = "giant-graph", about = "Inspect the target dependency graph")]
struct Cli {
    /// Target to focus on. Omitted: list every target in the merged graph.
    target: Option<String>,

    /// Walk downstream consumers (rdeps) instead of upstream deps. Answers
    /// "what breaks if I change this?".
    #[arg(short = 'r', long)]
    reverse: bool,

    /// Text mode: render a focused target as a flat closure (each target once)
    /// instead of an expanded tree.
    #[arg(long)]
    compact: bool,

    /// Text tree mode: limit the depth shown.
    #[arg(long)]
    depth: Option<usize>,

    /// Output format. `text` is human-facing; the rest emit the graph as data
    /// for external renderers.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,

    /// Path to giant.yaml (defaults to walking up from the current directory).
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Format {
    Text,
    Dot,
    Mermaid,
    Json,
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("giant graph: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let graph = load_graph(cli.config.as_deref())?;

    let root = match cli.target {
        Some(s) => {
            let id = TargetId::new(&s);
            if graph.get(&id).is_none() {
                bail!("target {s:?} not found in graph");
            }
            Some(id)
        }
        None => None,
    };
    let dir = if cli.reverse {
        Direction::Rdeps
    } else {
        Direction::Deps
    };

    let out = match cli.format {
        Format::Text => match (&root, cli.compact) {
            (None, _) => render::list(&graph),
            (Some(r), false) => render::tree(&graph, r, dir, cli.depth),
            (Some(r), true) => render::compact(&graph, r, dir),
        },
        Format::Dot => render::dot(&graph, root.as_ref(), dir),
        Format::Mermaid => render::mermaid(&graph, root.as_ref(), dir),
        Format::Json => render::json(&graph, root.as_ref(), dir),
    };
    print!("{out}");
    Ok(())
}

/// Scan the workspace config and build the (fully static) graph.
fn load_graph(config: Option<&std::path::Path>) -> Result<BuildGraph> {
    let (cfg, _root) = Config::scan_workspace(config)?;
    let mut graph = BuildGraph::new();
    for target in cfg.targets {
        graph.add_target(target)?;
    }
    graph.build_edges_and_validate()?;
    Ok(graph)
}
