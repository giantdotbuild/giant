//! Target selection - turn CLI args into a concrete set of target IDs.
//!
//! Today this module hosts affected-target computation. Glob patterns,
//! tags, and the larger pipeline from TDD-0011 live in cli/build.rs
//! inline until they grow enough to deserve their own home.

use crate::graph::BuildGraph;
use crate::model::{Input, TargetId};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum SelectionError {
    #[error("no affected targets matched")]
    NoneAffected,

    #[error("invalid glob {pattern:?}: {error}")]
    BadGlob {
        pattern: String,
        error: glob::PatternError,
    },
}

/// Set of targets whose inputs match any of the given changed files,
/// plus everything transitively downstream of them.
///
/// "Matches" means: any input glob on the target (file or structural)
/// `Pattern::matches` the workspace-relative path. We don't try to
/// resolve "is this file ACTUALLY going to change the cache key?" -
/// that's the job of the cache-key compute. Affected detection just
/// has to be sound (over-include is fine; under-include is a bug).
pub fn affected_targets(graph: &BuildGraph, changed_files: &[&Path]) -> HashSet<TargetId> {
    let changed_strs: Vec<String> = changed_files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    let mut direct: HashSet<TargetId> = HashSet::new();
    for (id, spec) in graph.iter() {
        if target_inputs_match_any(spec, &changed_strs) {
            direct.insert(id.clone());
        }
    }

    // Transitively close downstream: anything that consumes an affected
    // target is also affected.
    let mut all = direct.clone();
    let mut stack: Vec<TargetId> = direct.into_iter().collect();
    while let Some(id) = stack.pop() {
        for downstream in graph.direct_downstream(&id) {
            if all.insert(downstream.clone()) {
                stack.push(downstream);
            }
        }
    }
    all
}

fn target_inputs_match_any(spec: &crate::model::TargetSpec, files: &[String]) -> bool {
    for input in &spec.inputs {
        let globs: Vec<&str> = match input {
            Input::File { glob } => vec![glob.as_str()],
            Input::Structural { files: gs, .. } => gs.iter().map(|g| g.as_str()).collect(),
        };
        for raw in globs {
            let Ok(pattern) = glob::Pattern::new(raw) else {
                continue;
            };
            if files.iter().any(|f| pattern.matches(f)) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TargetSpec;
    use crate::paths::{OutputPath, WsRelPath};
    use crate::types::GlobPattern;

    fn spec(id: &str, deps: &[&str], outputs: &[&str], inputs: &[&str]) -> TargetSpec {
        TargetSpec {
            id: TargetId::new(id),
            inputs: inputs
                .iter()
                .map(|g| Input::File {
                    glob: GlobPattern::new(*g).unwrap(),
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
            sandbox: false,
            inferred_deps: Default::default(),
        }
    }

    fn graph_with(specs: Vec<TargetSpec>) -> BuildGraph {
        let mut g = BuildGraph::new();
        for s in specs {
            g.add_target(s).unwrap();
        }
        g.build_edges_and_validate().unwrap();
        g
    }

    #[test]
    fn no_changed_files_means_no_affected() {
        let g = graph_with(vec![spec("a", &[], &["a"], &["**/*.go"])]);
        let aff = affected_targets(&g, &[]);
        assert!(aff.is_empty());
    }

    #[test]
    fn direct_match_via_input_glob() {
        let g = graph_with(vec![spec("a", &[], &["a"], &["src/**/*.go"])]);
        let aff = affected_targets(&g, &[Path::new("src/main.go")]);
        assert_eq!(aff, [TargetId::new("a")].into());
    }

    #[test]
    fn no_match_when_file_outside_glob() {
        let g = graph_with(vec![spec("a", &[], &["a"], &["src/**/*.go"])]);
        let aff = affected_targets(&g, &[Path::new("README.md")]);
        assert!(aff.is_empty());
    }

    #[test]
    fn transitive_downstream_included() {
        // a (input: src/*.go) → produces bin/a
        // b (input: bin/a) → depends on a via inference
        let g = graph_with(vec![
            spec("a", &[], &["bin/a"], &["src/**/*.go"]),
            spec("b", &[], &["bin/b"], &["bin/a"]),
        ]);
        let aff = affected_targets(&g, &[Path::new("src/main.go")]);
        assert!(aff.contains(&TargetId::new("a")));
        assert!(aff.contains(&TargetId::new("b")));
    }

    #[test]
    fn unrelated_targets_not_affected() {
        let g = graph_with(vec![
            spec("a", &[], &["bin/a"], &["src/a/**/*.go"]),
            spec("b", &[], &["bin/b"], &["src/b/**/*.go"]),
        ]);
        let aff = affected_targets(&g, &[Path::new("src/a/main.go")]);
        assert_eq!(aff, [TargetId::new("a")].into());
    }

    #[test]
    fn structural_input_glob_matches_too() {
        let g = graph_with(vec![TargetSpec {
            id: TargetId::new("discover:go"),
            inputs: vec![Input::Structural {
                files: vec![GlobPattern::new("**/*.go").unwrap()],
                lines: vec!["package ".into()],
                scope: vec![],
            }],
            outputs: vec![OutputPath::new("d.json").unwrap()],
            deps: vec![],
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
        }]);
        let aff = affected_targets(&g, &[Path::new("internal/util.go")]);
        assert_eq!(aff, [TargetId::new("discover:go")].into());
    }
}
