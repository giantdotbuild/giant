use std::fs;

use giant::{BuildGraph, Config, TargetId};
use tempfile::TempDir;

use super::{Direction, Palette, compact, dot, json, list, mermaid, tree};

/// Color-off palette so assertions match plain text.
fn plain() -> Palette {
    Palette::new(false)
}

/// Build the static graph from a workspace `giant.yaml` (the real loader path).
fn graph(yaml: &str) -> BuildGraph {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("giant.yaml");
    fs::write(&path, yaml).unwrap();
    let (cfg, _) = Config::scan_workspace(Some(&path)).unwrap();
    let mut g = BuildGraph::new();
    for t in cfg.targets {
        g.add_target(t).unwrap();
    }
    g.build_edges_and_validate().unwrap();
    g
}

/// Diamond: app -> {libA, libB} -> base.
const DIAMOND: &str = "\
workspace:
  name: w
targets:
  - name: app
    command: \"true\"
    outputs: [\"app\"]
    deps: [\"//:libA\", \"//:libB\"]
  - name: libA
    command: \"true\"
    outputs: [\"a\"]
    deps: [\"//:base\"]
  - name: libB
    command: \"true\"
    outputs: [\"b\"]
    deps: [\"//:base\"]
  - name: base
    command: \"true\"
    outputs: [\"base\"]
";

fn id(s: &str) -> TargetId {
    TargetId::new(s)
}

#[test]
fn list_shows_all_targets_with_deps_and_count() {
    let g = graph(DIAMOND);
    let out = list(&g, &plain());
    assert!(out.contains("//:app"));
    assert!(out.contains("→ //:libA, //:libB"), "got:\n{out}");
    assert!(out.contains("//:base\n"), "leaf has no arrow; got:\n{out}");
    assert!(out.contains("4 target(s)"));
}

#[test]
fn tree_expands_and_marks_repeats_seen() {
    let g = graph(DIAMOND);
    let out = tree(&g, &id("//:app"), Direction::Deps, None, &plain());
    assert!(out.starts_with("//:app\n"));
    // Box-drawing connectors for children, deeper nesting carries the │ rail.
    assert!(
        out.contains("├─ //:libA") || out.contains("└─ //:libA"),
        "got:\n{out}"
    );
    assert!(out.contains("//:base"), "got:\n{out}");
    // base is shared by libA and libB; the second occurrence is not re-expanded.
    assert!(
        out.contains("(seen)"),
        "shared node should be marked; got:\n{out}"
    );
}

#[test]
fn tree_colors_when_palette_on() {
    let g = graph(DIAMOND);
    let out = tree(
        &g,
        &id("//:app"),
        Direction::Deps,
        None,
        &Palette::new(true),
    );
    assert!(
        out.contains("\x1b["),
        "colored output carries SGR codes; got:\n{out}"
    );
}

#[test]
fn tree_depth_limits_levels() {
    let g = graph(DIAMOND);
    let out = tree(&g, &id("//:app"), Direction::Deps, Some(1), &plain());
    assert!(out.contains("//:libA") && out.contains("//:libB"));
    assert!(
        !out.contains("//:base"),
        "depth 1 must not reach base; got:\n{out}"
    );
}

#[test]
fn compact_lists_closure_once() {
    let g = graph(DIAMOND);
    let out = compact(&g, &id("//:app"), Direction::Deps, &plain());
    // base gets exactly one node line (no repeated subtree); it still appears
    // in libA's and libB's dep lists.
    let base_node_lines = out.lines().filter(|l| l.trim() == "//:base").count();
    assert_eq!(base_node_lines, 1, "got:\n{out}");
    assert!(out.contains("4 target(s) in closure"));
}

#[test]
fn reverse_walks_downstream() {
    let g = graph(DIAMOND);
    let out = tree(&g, &id("//:base"), Direction::Rdeps, None, &plain());
    // base's consumers: libA, libB, and transitively app.
    assert!(
        out.contains("//:libA") && out.contains("//:app"),
        "got:\n{out}"
    );
}

#[test]
fn dot_emits_nodes_and_real_edges() {
    let g = graph(DIAMOND);
    let out = dot(&g, None, Direction::Deps);
    assert!(out.starts_with("digraph giant {"));
    assert!(out.contains("\"//:app\" -> \"//:libA\""), "got:\n{out}");
    assert!(out.trim_end().ends_with('}'));
}

#[test]
fn mermaid_emits_graph_and_edges() {
    let g = graph(DIAMOND);
    let out = mermaid(&g, None, Direction::Deps);
    assert!(out.starts_with("graph LR"));
    assert!(out.contains("-->"), "got:\n{out}");
    assert!(out.contains("[\"//:app\"]"));
}

#[test]
fn json_round_trips_with_deps() {
    let g = graph(DIAMOND);
    let out = json(&g, None, Direction::Deps);
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    let targets = doc["targets"].as_array().unwrap();
    let app = targets
        .iter()
        .find(|t| t["id"] == "//:app")
        .expect("app present");
    let deps: Vec<&str> = app["deps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap())
        .collect();
    assert_eq!(deps, vec!["//:libA", "//:libB"]);
}

#[test]
fn focused_export_restricts_to_closure() {
    let g = graph(DIAMOND);
    // libA's deps-closure is {libA, base}; app and libB are out of scope.
    let out = json(&g, Some(&id("//:libA")), Direction::Deps);
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    let ids: Vec<&str> = doc["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"//:libA") && ids.contains(&"//:base"));
    assert!(
        !ids.contains(&"//:app") && !ids.contains(&"//:libB"),
        "got: {ids:?}"
    );
}
