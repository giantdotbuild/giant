//! `giant graph [target]` - list targets, or show a target's dep tree.
//!
//! Two modes:
//! - No positional → flat list of every target, sorted, with a single
//!   line per target showing its direct deps. Discoverability mode.
//! - One positional → tree under that target. `--reverse` flips to
//!   downstream consumers (what depends on this).

use crate::graph::BuildGraph;
use crate::model::TargetId;
use clap::Args;
use std::collections::HashSet;
use std::io::Write;

use super::prep;

#[derive(Args, Debug)]
pub struct GraphArgs {
    /// Target to show. If omitted, lists every target in the merged graph.
    pub target: Option<String>,

    /// In tree mode, show downstream consumers instead of upstream deps.
    /// Answers "what breaks if I remove this target?"
    #[arg(short = 'r', long)]
    pub reverse: bool,
}

pub async fn execute(args: GraphArgs, global: &super::GlobalFlags) -> anyhow::Result<()> {
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

    match args.target {
        None => print_list(&prepared.graph),
        Some(id_str) => {
            let id = TargetId::new(&id_str);
            if prepared.graph.get(&id).is_none() {
                anyhow::bail!("target {id_str:?} not found in graph");
            }
            print_tree(&prepared.graph, &id, args.reverse);
        }
    }
    Ok(())
}

/// One line per target, sorted, with a `→ dep1, dep2, …` tail when the
/// target has deps. Leaves get just their ID - easy to spot.
fn print_list(graph: &BuildGraph) {
    let stdout = std::io::stdout();
    let mut w = stdout.lock();

    let mut ids: Vec<&TargetId> = graph.iter().map(|(id, _)| id).collect();
    ids.sort();

    // Column-align the target column so the arrows line up. Width = the
    // longest ID, capped so absurdly long IDs don't bury the rest.
    const MAX_COL: usize = 48;
    let col = ids
        .iter()
        .map(|id| id.as_str().len())
        .max()
        .unwrap_or(0)
        .min(MAX_COL);

    for id in &ids {
        let deps = graph.direct_deps(id);
        if deps.is_empty() {
            let _ = writeln!(w, "{}", id);
        } else {
            let dep_list = deps
                .iter()
                .map(|d| d.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(w, "{:<width$}  → {}", id, dep_list, width = col);
        }
    }

    let _ = writeln!(w);
    let _ = writeln!(w, "{} target(s)", ids.len());
    let _ = w.flush();
}

/// Indented tree printout. Inferred-vs-explicit is marked inline so
/// users see why an edge exists.
fn print_tree(graph: &BuildGraph, root: &TargetId, reverse: bool) {
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    let _ = writeln!(w, "{root}");
    let mut visited: HashSet<TargetId> = HashSet::new();
    visited.insert(root.clone());
    walk(&mut w, graph, root, 1, reverse, &mut visited);
    let _ = w.flush();
}

fn walk(
    w: &mut impl Write,
    graph: &BuildGraph,
    id: &TargetId,
    depth: usize,
    reverse: bool,
    visited: &mut HashSet<TargetId>,
) {
    let neighbors = if reverse {
        graph.direct_downstream(id)
    } else {
        graph.direct_deps(id)
    };
    let spec = graph.get(id);
    let inferred: &HashSet<TargetId> = spec
        .map(|s| &s.inferred_deps)
        .unwrap_or(EMPTY_INFERRED.get_or_init(HashSet::new));

    for dep in neighbors {
        let mark = if reverse {
            ""
        } else if inferred.contains(&dep) {
            "  (inferred)"
        } else {
            ""
        };
        let prefix = "  ".repeat(depth);
        if !visited.insert(dep.clone()) {
            let _ = writeln!(w, "{prefix}{dep}{mark}  (cycle/visited)");
            continue;
        }
        let _ = writeln!(w, "{prefix}{dep}{mark}");
        walk(w, graph, &dep, depth + 1, reverse, visited);
    }
}

use std::sync::OnceLock;
/// Shared empty set so we can return a `&HashSet` without allocating
/// when a target spec is missing (only happens on graph-state bugs).
static EMPTY_INFERRED: OnceLock<HashSet<TargetId>> = OnceLock::new();
