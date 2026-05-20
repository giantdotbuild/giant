//! Build graph backed by `petgraph`.
//!
//! Holds the merged set of targets and dep edges (explicit + output-inferred).
//! See TDD-0003 for inference and merge semantics, TDD-0001 for the schema.

use crate::model::{TargetId, TargetSpec};
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

#[derive(Debug, Default)]
pub struct BuildGraph {
    targets: HashMap<TargetId, TargetSpec>,
    g: DiGraph<TargetId, ()>,
    idx: HashMap<TargetId, NodeIndex>,
}

impl BuildGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a target. Edges are inserted by `resolve_explicit_deps`
    /// after all targets are added.
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

    /// Wire up the explicit `deps:` edges. Call after all targets added.
    pub fn resolve_explicit_deps(&mut self) -> Result<(), GraphError> {
        // We mutate `g` (edges) based on a read of `targets`; clone the
        // tuples to avoid borrow conflicts.
        let edges: Vec<(TargetId, Vec<TargetId>)> = self
            .targets
            .iter()
            .map(|(id, spec)| (id.clone(), spec.deps.clone()))
            .collect();
        for (parent, deps) in edges {
            let parent_idx = *self.idx.get(&parent).expect("just-added target missing");
            for dep in deps {
                let Some(&dep_idx) = self.idx.get(&dep) else {
                    return Err(GraphError::UnknownDep {
                        parent: parent.clone(),
                        missing: dep,
                    });
                };
                self.g.add_edge(parent_idx, dep_idx, ());
            }
        }
        Ok(())
    }

    /// Run output-based dep inference (TDD-0003). Stub for now - see
    /// also `tests/` for a real fixture exercising it.
    pub fn apply_output_based_inference(&mut self) -> Result<(), GraphError> {
        // TODO(impl): inverted-output index + glob match against inputs.
        // Defer until the next slice; static-config-only builds work fine
        // without it.
        Ok(())
    }

    /// Detect cycles. Returns Ok(()) on DAG; Err(Cycle) with the offending
    /// chain stringified.
    pub fn validate_acyclic(&self) -> Result<(), GraphError> {
        match toposort(&self.g, None) {
            Ok(_) => Ok(()),
            Err(cycle) => {
                let node = cycle.node_id();
                let id = &self.g[node];
                Err(GraphError::Cycle(format!(
                    "cycle includes target {id}"
                )))
            }
        }
    }

    /// Topological order, deps first. A target appears after all its deps.
    pub fn topo_order(&self) -> Result<Vec<TargetId>, GraphError> {
        // petgraph's toposort gives "edge source before edge target". Our
        // edges go parent → dep (target → its dep), so toposort gives us
        // parents before deps. We want deps first, so reverse.
        let order = toposort(&self.g, None).map_err(|c| {
            GraphError::Cycle(format!("cycle includes target {}", &self.g[c.node_id()]))
        })?;
        Ok(order.into_iter().rev().map(|n| self.g[n].clone()).collect())
    }

    /// Direct dependencies of a target.
    pub fn direct_deps(&self, id: &TargetId) -> Vec<TargetId> {
        let Some(&node) = self.idx.get(id) else {
            return Vec::new();
        };
        self.g
            .neighbors_directed(node, Direction::Outgoing)
            .map(|n| self.g[n].clone())
            .collect()
    }

    /// Direct downstream consumers (targets that depend on `id`).
    pub fn direct_downstream(&self, id: &TargetId) -> Vec<TargetId> {
        let Some(&node) = self.idx.get(id) else {
            return Vec::new();
        };
        self.g
            .neighbors_directed(node, Direction::Incoming)
            .map(|n| self.g[n].clone())
            .collect()
    }

    /// Closure: this target plus everything it transitively depends on.
    /// Used to compute the build subgraph for a selection.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{OutputPath, WsRelPath};

    fn spec(id: &str, deps: &[&str]) -> TargetSpec {
        TargetSpec {
            id: TargetId::new(id),
            inputs: Vec::new(),
            outputs: vec![OutputPath::new(format!("out/{id}")).unwrap()],
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
            sandbox: false,
            inferred_deps: Default::default(),
        }
    }

    fn build(specs: Vec<TargetSpec>) -> Result<BuildGraph, GraphError> {
        let mut g = BuildGraph::new();
        for s in specs {
            g.add_target(s)?;
        }
        g.resolve_explicit_deps()?;
        Ok(g)
    }

    #[test]
    fn duplicate_id_rejected() {
        let mut g = BuildGraph::new();
        g.add_target(spec("a", &[])).unwrap();
        let err = g.add_target(spec("a", &[])).unwrap_err();
        assert!(matches!(err, GraphError::DuplicateId(_)));
    }

    #[test]
    fn unknown_dep_rejected() {
        let err = build(vec![spec("a", &["missing"])]).unwrap_err();
        assert!(matches!(err, GraphError::UnknownDep { .. }));
    }

    #[test]
    fn topo_order_deps_first() {
        // a depends on b; b depends on c.
        let g = build(vec![spec("a", &["b"]), spec("b", &["c"]), spec("c", &[])]).unwrap();
        let order = g.topo_order().unwrap();
        let pos = |id: &str| {
            order
                .iter()
                .position(|t| t.as_str() == id)
                .expect("present")
        };
        // deps must come BEFORE their consumers.
        assert!(pos("c") < pos("b"));
        assert!(pos("b") < pos("a"));
    }

    #[test]
    fn cycle_detected() {
        let g = build(vec![spec("a", &["b"]), spec("b", &["a"])]).unwrap();
        let err = g.validate_acyclic().unwrap_err();
        assert!(matches!(err, GraphError::Cycle(_)));
    }

    #[test]
    fn direct_deps_and_downstream() {
        let g = build(vec![spec("a", &["b"]), spec("b", &[])]).unwrap();
        assert_eq!(g.direct_deps(&TargetId::new("a")), vec![TargetId::new("b")]);
        assert_eq!(
            g.direct_downstream(&TargetId::new("b")),
            vec![TargetId::new("a")]
        );
    }

    #[test]
    fn closure_walks_transitively() {
        let g = build(vec![
            spec("a", &["b"]),
            spec("b", &["c"]),
            spec("c", &[]),
            spec("unrelated", &[]),
        ])
        .unwrap();
        let closure = g.closure_over_deps([&TargetId::new("a")]);
        assert_eq!(closure.len(), 3);
        assert!(closure.contains(&TargetId::new("a")));
        assert!(closure.contains(&TargetId::new("b")));
        assert!(closure.contains(&TargetId::new("c")));
        assert!(!closure.contains(&TargetId::new("unrelated")));
    }
}
