//! Graph renderers. Text modes (list / tree / compact) for humans, and
//! dot / mermaid / json for external layout tools. Every renderer takes the
//! static `BuildGraph` and returns a `String`; `main` prints it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write;

use giant::{BuildGraph, TargetId};

/// Which way to walk: a target's dependencies, or its downstream consumers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Deps,
    Rdeps,
}

fn neighbors(graph: &BuildGraph, id: &TargetId, dir: Direction) -> Vec<TargetId> {
    match dir {
        Direction::Deps => graph.direct_deps(id),
        Direction::Rdeps => graph.direct_downstream(id),
    }
}

/// Targets reachable from `root` along `dir`, including `root`.
fn closure(graph: &BuildGraph, root: &TargetId, dir: Direction) -> BTreeSet<TargetId> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![root.clone()];
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        stack.extend(neighbors(graph, &id, dir));
    }
    seen
}

/// The node set an export covers: a focused target's closure, else everything.
fn scope(graph: &BuildGraph, root: Option<&TargetId>, dir: Direction) -> BTreeSet<TargetId> {
    match root {
        Some(r) => closure(graph, r, dir),
        None => graph.iter().map(|(id, _)| id.clone()).collect(),
    }
}

/// Column width for aligning the `id → deps` columns, capped so a runaway id
/// does not bury the rest.
fn col_width<'a>(ids: impl Iterator<Item = &'a TargetId>) -> usize {
    ids.map(|id| id.as_str().len()).max().unwrap_or(0).min(48)
}

/// One line per target, sorted: `id → dep1, dep2` (leaves get just the id).
pub(crate) fn list(graph: &BuildGraph) -> String {
    let mut ids: Vec<&TargetId> = graph.iter().map(|(id, _)| id).collect();
    ids.sort();
    let col = col_width(ids.iter().copied());

    let mut s = String::new();
    for id in &ids {
        adjacency_line(&mut s, id, &graph.direct_deps(id), None, col);
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "{} target(s)", ids.len());
    s
}

/// A focused target's closure as flat adjacency (each target once), sorted.
/// The DAG-aware answer to the tree's repeated subtrees.
pub(crate) fn compact(graph: &BuildGraph, root: &TargetId, dir: Direction) -> String {
    let nodes = closure(graph, root, dir);
    let col = col_width(nodes.iter());

    let mut s = String::new();
    for id in &nodes {
        let ns: Vec<TargetId> = neighbors(graph, id, dir)
            .into_iter()
            .filter(|n| nodes.contains(n))
            .collect();
        adjacency_line(&mut s, id, &ns, Some(&nodes), col);
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "{} target(s) in closure", nodes.len());
    s
}

/// Write `id` alone, or `id  → a, b, c` aligned to `col`. `keep`, when set,
/// filters the neighbor list to in-scope ids.
fn adjacency_line(
    s: &mut String,
    id: &TargetId,
    ns: &[TargetId],
    keep: Option<&BTreeSet<TargetId>>,
    col: usize,
) {
    let shown: Vec<&str> = ns
        .iter()
        .filter(|n| keep.is_none_or(|k| k.contains(*n)))
        .map(|n| n.as_str())
        .collect();
    if shown.is_empty() {
        let _ = writeln!(s, "{id}");
    } else {
        let _ = writeln!(s, "{:<col$}  → {}", id.as_str(), shown.join(", "));
    }
}

/// Indented dependency tree from `root` (or downstream tree in rdeps mode).
/// A target reached more than once is shown but not re-expanded, marked
/// `(seen)`; `depth` caps the levels.
pub(crate) fn tree(
    graph: &BuildGraph,
    root: &TargetId,
    dir: Direction,
    depth: Option<usize>,
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{root}");
    let mut visited = HashSet::new();
    visited.insert(root.clone());
    walk(graph, root, 1, dir, depth, &mut visited, &mut s);
    s
}

fn walk(
    graph: &BuildGraph,
    id: &TargetId,
    level: usize,
    dir: Direction,
    depth: Option<usize>,
    visited: &mut HashSet<TargetId>,
    s: &mut String,
) {
    if depth.is_some_and(|max| level > max) {
        return;
    }
    for n in neighbors(graph, id, dir) {
        let indent = "  ".repeat(level);
        if !visited.insert(n.clone()) {
            let _ = writeln!(s, "{indent}{n}  (seen)");
            continue;
        }
        let _ = writeln!(s, "{indent}{n}");
        walk(graph, &n, level + 1, dir, depth, visited, s);
    }
}

/// Graphviz DOT over the scope. Edges are the real dep edges (parent -> dep)
/// between in-scope nodes; `dir` only selects the scope around a focused target.
pub(crate) fn dot(graph: &BuildGraph, root: Option<&TargetId>, dir: Direction) -> String {
    let nodes = scope(graph, root, dir);
    let mut s = String::from("digraph giant {\n  rankdir=LR;\n  node [shape=box];\n");
    for id in &nodes {
        let _ = writeln!(s, "  {:?};", id.as_str());
        for dep in graph.direct_deps(id) {
            if nodes.contains(&dep) {
                let _ = writeln!(s, "  {:?} -> {:?};", id.as_str(), dep.as_str());
            }
        }
    }
    s.push_str("}\n");
    s
}

/// Mermaid `graph LR` over the scope, with stable `nN` node ids and the label
/// in brackets (target ids carry `/` and `:` which mermaid ids cannot).
pub(crate) fn mermaid(graph: &BuildGraph, root: Option<&TargetId>, dir: Direction) -> String {
    let nodes = scope(graph, root, dir);
    let alias: HashMap<&TargetId, String> = nodes
        .iter()
        .enumerate()
        .map(|(i, id)| (id, format!("n{i}")))
        .collect();

    let mut s = String::from("graph LR\n");
    for id in &nodes {
        let nid = &alias[id];
        let _ = writeln!(s, "  {nid}[\"{}\"]", id.as_str());
        for dep in graph.direct_deps(id) {
            if let Some(did) = alias.get(&dep) {
                let _ = writeln!(s, "  {nid} --> {did}");
            }
        }
    }
    s
}

/// `{ "targets": [ { id, deps, tags } ] }` over the scope, sorted.
pub(crate) fn json(graph: &BuildGraph, root: Option<&TargetId>, dir: Direction) -> String {
    let nodes = scope(graph, root, dir);
    let targets: Vec<serde_json::Value> = nodes
        .iter()
        .map(|id| {
            let deps: Vec<String> = graph
                .direct_deps(id)
                .iter()
                .filter(|d| nodes.contains(*d))
                .map(|d| d.as_str().to_string())
                .collect();
            let mut tags: Vec<String> = graph
                .get(id)
                .map(|s| s.tags.iter().cloned().collect())
                .unwrap_or_default();
            tags.sort();
            serde_json::json!({ "id": id.as_str(), "deps": deps, "tags": tags })
        })
        .collect();
    let doc = serde_json::json!({ "targets": targets });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&doc).expect("serializable")
    )
}

#[cfg(test)]
mod tests;
