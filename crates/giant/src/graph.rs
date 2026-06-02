//! Build graph backed by `petgraph`.
//!
//! Holds the merged set of targets and dep edges (explicit + output-inferred).
//! See TDD-0003 for inference and merge semantics, TDD-0001 for the schema.

use crate::model::{Input, TargetId, TargetSpec};
use petgraph::Direction;
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::{HashMap, HashSet};

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("duplicate target id: {0}")]
    DuplicateId(TargetId),

    #[error("target {parent:?} has dep on unknown target {missing:?}")]
    UnknownDep { parent: TargetId, missing: TargetId },

    #[error("dependency cycle: {0}")]
    Cycle(String),

    #[error("two targets produce the same output {path}: {a} and {b}")]
    OutputCollision {
        path: String,
        a: TargetId,
        b: TargetId,
    },
}

#[derive(Debug, Default, Clone)]
pub struct BuildGraph {
    targets: HashMap<TargetId, TargetSpec>,
    g: DiGraph<TargetId, ()>,
    idx: HashMap<TargetId, NodeIndex>,
}

impl BuildGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a target. Only inserts the node; edges are (re)built by
    /// `build_edges_and_validate`.
    pub fn add_target(&mut self, spec: TargetSpec) -> Result<(), GraphError> {
        if self.targets.contains_key(&spec.id) {
            return Err(GraphError::DuplicateId(spec.id));
        }
        let id = spec.id.clone();
        let n = self.g.add_node(id.clone());
        self.idx.insert(id.clone(), n);
        self.targets.insert(id, spec);
        Ok(())
    }

    pub fn get(&self, id: &TargetId) -> Option<&TargetSpec> {
        self.targets.get(id)
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&TargetId, &TargetSpec)> {
        self.targets.iter()
    }

    /// Ids of every target carrying `tag`. The renderer uses this to fold
    /// `toolchain`-tagged targets out of the default view (TDD-0017).
    pub fn ids_with_tag(&self, tag: &str) -> HashSet<TargetId> {
        self.iter()
            .filter(|(_, spec)| spec.tags.contains(tag))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Re-derive all dep edges from current targets. Safe to call multiple
    /// times (clears existing edges first). Does three things:
    ///
    /// 1. Wire explicit `deps:` edges, checking they reference known IDs.
    /// 2. Run output-based dep inference (TDD-0003), adding edges where a
    ///    target's input glob matches another target's output paths.
    /// 3. Validate acyclic.
    ///
    /// After this, `direct_deps()` returns the union of explicit and
    /// inferred deps, deduplicated.
    pub fn build_edges_and_validate(&mut self) -> Result<(), GraphError> {
        self.g.clear_edges();

        // Reset inferred_deps so re-runs after adding more targets don't
        // accumulate stale entries.
        for spec in self.targets.values_mut() {
            spec.inferred_deps.clear();
        }

        let mut seen_edges: HashSet<(TargetId, TargetId)> = HashSet::new();

        // 1. Explicit deps.
        let explicit_edges: Vec<(TargetId, Vec<TargetId>)> = self
            .targets
            .iter()
            .map(|(id, spec)| (id.clone(), spec.deps.clone()))
            .collect();
        for (parent, deps) in explicit_edges {
            let parent_idx = *self.idx.get(&parent).expect("target node missing");
            for dep in deps {
                let Some(&dep_idx) = self.idx.get(&dep) else {
                    return Err(GraphError::UnknownDep {
                        parent: parent.clone(),
                        missing: dep,
                    });
                };
                if seen_edges.insert((parent.clone(), dep.clone())) {
                    self.g.add_edge(parent_idx, dep_idx, ());
                }
            }
        }

        // 2. Output-based inference (TDD-0003).
        let inferred = compute_inferred_edges(&self.targets)?;
        for (parent, dep) in inferred {
            if !seen_edges.insert((parent.clone(), dep.clone())) {
                continue;
            }
            let parent_idx = *self.idx.get(&parent).expect("target node missing");
            let dep_idx = *self.idx.get(&dep).expect("target node missing");
            self.g.add_edge(parent_idx, dep_idx, ());
            if let Some(spec) = self.targets.get_mut(&parent) {
                spec.inferred_deps.insert(dep);
            }
        }

        // 3. Acyclic check.
        self.validate_acyclic()?;

        Ok(())
    }

    /// Detect cycles. Returns Ok(()) on DAG.
    pub fn validate_acyclic(&self) -> Result<(), GraphError> {
        match toposort(&self.g, None) {
            Ok(_) => Ok(()),
            Err(cycle) => {
                let node = cycle.node_id();
                let id = &self.g[node];
                Err(GraphError::Cycle(format!("cycle includes target {id}")))
            }
        }
    }

    /// Topological order, deps first. A target appears after all its deps.
    pub fn topo_order(&self) -> Result<Vec<TargetId>, GraphError> {
        let order = toposort(&self.g, None).map_err(|c| {
            GraphError::Cycle(format!("cycle includes target {}", &self.g[c.node_id()]))
        })?;
        // Edges point parent → dep, so toposort emits parents first; we want
        // deps first.
        Ok(order.into_iter().rev().map(|n| self.g[n].clone()).collect())
    }

    /// Direct dependencies (explicit ∪ inferred), sorted, deduplicated.
    pub fn direct_deps(&self, id: &TargetId) -> Vec<TargetId> {
        let Some(&node) = self.idx.get(id) else {
            return Vec::new();
        };
        let mut deps: Vec<TargetId> = self
            .g
            .neighbors_directed(node, Direction::Outgoing)
            .map(|n| self.g[n].clone())
            .collect();
        deps.sort();
        deps.dedup();
        deps
    }

    /// Direct downstream consumers (sorted, deduplicated).
    pub fn direct_downstream(&self, id: &TargetId) -> Vec<TargetId> {
        let Some(&node) = self.idx.get(id) else {
            return Vec::new();
        };
        let mut ds: Vec<TargetId> = self
            .g
            .neighbors_directed(node, Direction::Incoming)
            .map(|n| self.g[n].clone())
            .collect();
        ds.sort();
        ds.dedup();
        ds
    }

    /// Closure: this target plus everything it transitively depends on.
    pub fn closure_over_deps<'a>(
        &'a self,
        seeds: impl IntoIterator<Item = &'a TargetId>,
    ) -> HashSet<TargetId> {
        let mut visited: HashSet<TargetId> = HashSet::new();
        let mut stack: Vec<TargetId> = seeds.into_iter().cloned().collect();
        while let Some(id) = stack.pop() {
            if !visited.insert(id.clone()) {
                continue;
            }
            for dep in self.direct_deps(&id) {
                stack.push(dep);
            }
        }
        visited
    }
}

/// Output-based dependency inference (TDD-0003).
///
/// For each target T and each input glob G, find producers whose output
/// paths match G. Returns (parent, dep) tuples. Hard-errors when two
/// targets declare the same output path - that's a config bug, not an
/// ambiguity we can fix automatically.
fn compute_inferred_edges(
    targets: &HashMap<TargetId, TargetSpec>,
) -> Result<Vec<(TargetId, TargetId)>, GraphError> {
    // Inverted index: output path → producer.
    let mut producer: HashMap<String, TargetId> = HashMap::new();
    for (id, spec) in targets {
        for output in &spec.outputs {
            let path = output.as_path().to_string_lossy().into_owned();
            if let Some(prev) = producer.insert(path.clone(), id.clone()) {
                return Err(GraphError::OutputCollision {
                    path,
                    a: prev,
                    b: id.clone(),
                });
            }
        }
    }

    // For each (target, input-glob), find every producer whose output
    // satisfies the glob. Dedupe at the end.
    let mut edges: Vec<(TargetId, TargetId)> = Vec::new();
    for (id, spec) in targets {
        for input in &spec.inputs {
            let Input::File { glob } = input;
            let Ok(pattern) = glob::Pattern::new(glob.as_str()) else {
                continue;
            };
            for (output_path, prod) in &producer {
                if prod == id {
                    continue; // a target doesn't depend on itself
                }
                if pattern.matches(output_path) {
                    edges.push((id.clone(), prod.clone()));
                }
            }
        }
    }
    edges.sort();
    edges.dedup();
    Ok(edges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Input;
    use crate::paths::{OutputPath, WsRelPath};
    use crate::types::GlobPattern;

    fn spec(id: &str, deps: &[&str], outputs: &[&str], inputs: &[&str]) -> TargetSpec {
        TargetSpec {
            id: TargetId::new(id),
            inputs: inputs
                .iter()
                .map(|i| Input::File {
                    glob: GlobPattern::new(*i).unwrap(),
                })
                .collect(),
            outputs: outputs
                .iter()
                .map(|o| OutputPath::new(*o).unwrap())
                .collect(),
            deps: deps.iter().map(|d| TargetId::new(*d)).collect(),
            command: "true".into(),
            cwd: WsRelPath::default(),
            env: Default::default(),
            cache: Some(true),
            remote_cache: true,
            exists: None,
            timeout_secs: None,
            test: false,
            tags: Default::default(),
            label: None,
            inferred_deps: Default::default(),
        }
    }

    fn build(specs: Vec<TargetSpec>) -> Result<BuildGraph, GraphError> {
        let mut g = BuildGraph::new();
        for s in specs {
            g.add_target(s)?;
        }
        g.build_edges_and_validate()?;
        Ok(g)
    }

    #[test]
    fn duplicate_id_rejected() {
        let mut g = BuildGraph::new();
        g.add_target(spec("a", &[], &["a"], &[])).unwrap();
        let err = g.add_target(spec("a", &[], &["a2"], &[])).unwrap_err();
        assert!(matches!(err, GraphError::DuplicateId(_)));
    }

    #[test]
    fn unknown_dep_rejected() {
        let err = build(vec![spec("a", &["missing"], &["a"], &[])]).unwrap_err();
        assert!(matches!(err, GraphError::UnknownDep { .. }));
    }

    #[test]
    fn topo_order_deps_first() {
        let g = build(vec![
            spec("a", &["b"], &["a"], &[]),
            spec("b", &["c"], &["b"], &[]),
            spec("c", &[], &["c"], &[]),
        ])
        .unwrap();
        let order = g.topo_order().unwrap();
        let pos = |id: &str| order.iter().position(|t| t.as_str() == id).unwrap();
        assert!(pos("c") < pos("b"));
        assert!(pos("b") < pos("a"));
    }

    #[test]
    fn cycle_detected() {
        let err = build(vec![
            spec("a", &["b"], &["a"], &[]),
            spec("b", &["a"], &["b"], &[]),
        ])
        .unwrap_err();
        assert!(matches!(err, GraphError::Cycle(_)));
    }

    #[test]
    fn output_collision_rejected() {
        let err = build(vec![
            spec("a", &[], &["bin/server"], &[]),
            spec("b", &[], &["bin/server"], &[]),
        ])
        .unwrap_err();
        assert!(matches!(err, GraphError::OutputCollision { .. }));
    }

    #[test]
    fn output_based_inference_links_matching_input() {
        // a produces gen/api.pb.go
        // b inputs include gen/**/*.go - should infer b depends on a
        let g = build(vec![
            spec("a", &[], &["gen/api.pb.go"], &[]),
            spec("b", &[], &["bin/server"], &["gen/**/*.go"]),
        ])
        .unwrap();
        let deps_of_b = g.direct_deps(&TargetId::new("b"));
        assert_eq!(deps_of_b, vec![TargetId::new("a")]);
        // explicit deps remain empty; inferred_deps populated.
        let b = g.get(&TargetId::new("b")).unwrap();
        assert!(b.deps.is_empty());
        assert!(b.inferred_deps.contains(&TargetId::new("a")));
    }

    #[test]
    fn inference_doesnt_duplicate_existing_explicit_dep() {
        // a → b is both declared and inferred. Should appear once.
        let g = build(vec![
            spec("a", &[], &["bin/foo"], &[]),
            spec("b", &["a"], &["bin/bar"], &["bin/foo"]),
        ])
        .unwrap();
        let deps_of_b = g.direct_deps(&TargetId::new("b"));
        assert_eq!(deps_of_b, vec![TargetId::new("a")]);
    }

    #[test]
    fn inference_skips_self_dependency() {
        // a's input glob matches its own output. Should not depend on itself.
        let g = build(vec![spec("a", &[], &["a.txt"], &["*.txt"])]).unwrap();
        let deps_of_a = g.direct_deps(&TargetId::new("a"));
        assert!(deps_of_a.is_empty());
    }

    #[test]
    fn rebuild_edges_idempotent() {
        // Calling build_edges_and_validate twice doesn't accumulate edges.
        let mut g = BuildGraph::new();
        g.add_target(spec("a", &[], &["gen/x.go"], &[])).unwrap();
        g.add_target(spec("b", &[], &["bin/b"], &["gen/**/*.go"]))
            .unwrap();
        g.build_edges_and_validate().unwrap();
        g.build_edges_and_validate().unwrap();
        let deps = g.direct_deps(&TargetId::new("b"));
        assert_eq!(deps.len(), 1);
    }

    #[test]
    fn closure_walks_transitively() {
        let g = build(vec![
            spec("a", &["b"], &["a"], &[]),
            spec("b", &["c"], &["b"], &[]),
            spec("c", &[], &["c"], &[]),
            spec("unrelated", &[], &["x"], &[]),
        ])
        .unwrap();
        let closure = g.closure_over_deps([&TargetId::new("a")]);
        assert_eq!(closure.len(), 3);
        assert!(closure.contains(&TargetId::new("c")));
        assert!(!closure.contains(&TargetId::new("unrelated")));
    }
}
