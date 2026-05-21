//! Target selection - turn CLI args into a concrete set of target IDs.
//!
//! Two surfaces live here:
//! 1. **Pattern resolution** ([`resolve_patterns`]) - the user-facing
//!    selection language from TDD-0011: glob/exclusion patterns, tag
//!    filters via [`SelectionOpts`], and test/non-test filtering via
//!    [`TestMode`]. Used by `giant build`, `giant test`, `giant watch`,
//!    `giant affected`, and (planned) the NDJSON command channel.
//! 2. **Affected detection** ([`affected_targets`]) - given a list of
//!    changed files, which targets need to rebuild?

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

    /// Raised when a literal (non-glob) pattern matches no target -
    /// almost always a typo. Glob patterns that match nothing are
    /// silently empty; only literals get this error.
    #[error("no target matches {pattern:?}")]
    NoMatch { pattern: String },
}

/// What to do with `test: true` targets when resolving a selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestMode {
    /// `giant build` default - tests are not part of the selection
    /// unless named explicitly.
    Exclude,
    /// `giant test` - only test targets are selectable.
    Only,
    /// Used by tooling that wants every target (e.g. `giant graph`).
    Include,
}

/// Options that ride alongside the positional patterns.
#[derive(Debug, Clone, Default)]
pub struct SelectionOpts {
    /// Repeatable `--tag` - include only targets that carry at least
    /// one of these tags. Empty = no tag include filter.
    pub tags: Vec<String>,
    /// Repeatable `--no-tag` - drop targets that carry any of these
    /// tags. Composes with `tags` - `--tag release --no-tag flaky`
    /// means "release-tagged AND not flaky-tagged".
    pub no_tags: Vec<String>,
}

impl SelectionOpts {
    fn passes_tags(&self, spec: &crate::model::TargetSpec) -> bool {
        if !self.tags.is_empty() && !self.tags.iter().any(|t| spec.tags.contains(t)) {
            return false;
        }
        if self.no_tags.iter().any(|t| spec.tags.contains(t)) {
            return false;
        }
        true
    }
}

/// Resolve a list of user-supplied patterns against the graph into a
/// concrete, sorted set of target ids.
///
/// Syntax (TDD-0011):
/// - Empty patterns → all targets (subject to test mode + tag filter).
/// - Exact id (no glob chars) → exact match. Missing literal id is
///   an error.
/// - Glob: `*` matches any chars except `:`; `**` matches any chars
///   including `:`. (Implemented by swapping `:`↔`/` and delegating to
///   the `glob` crate.)
/// - `!pattern` excludes whatever the pattern matches.
/// - Multiple positionals are unioned; exclusions apply after.
/// - If only excludes are given, the implicit include is `**`.
///
/// `test_mode` controls whether `test: true` targets are eligible.
/// `opts` carries tag filters. Both apply *before* the pattern match,
/// so the user sees consistent results regardless of how broad the
/// pattern is.
///
/// Output is sorted lexicographically by id, so the order is stable
/// across runs and between CLI invocations and porcelain consumers.
pub fn resolve_patterns(
    graph: &BuildGraph,
    patterns: &[String],
    test_mode: TestMode,
    opts: &SelectionOpts,
) -> Result<Vec<TargetId>, SelectionError> {
    let eligible = |spec: &crate::model::TargetSpec| {
        match test_mode {
            TestMode::Exclude if spec.test => return false,
            TestMode::Only if !spec.test => return false,
            _ => {}
        }
        opts.passes_tags(spec)
    };

    if patterns.is_empty() {
        let mut out: Vec<TargetId> = graph
            .iter()
            .filter(|(_, s)| eligible(s))
            .map(|(id, _)| id.clone())
            .collect();
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        return Ok(out);
    }

    // Split into include / exclude lists, preserving order.
    let mut includes: Vec<&str> = Vec::new();
    let mut excludes: Vec<&str> = Vec::new();
    for p in patterns {
        match p.strip_prefix('!') {
            Some(neg) => excludes.push(neg),
            None => includes.push(p),
        }
    }
    // Excludes-only → start from everything.
    if includes.is_empty() {
        includes.push("**");
    }

    let mut selected: Vec<TargetId> = Vec::new();
    let mut seen: HashSet<TargetId> = HashSet::new();
    for raw in &includes {
        let pat = compile(raw)?;
        let mut matched_any = false;
        for (id, spec) in graph.iter() {
            if !eligible(spec) {
                continue;
            }
            if pat.matches_with(&id_match_str(id), MATCH_OPTS) {
                matched_any = true;
                if seen.insert(id.clone()) {
                    selected.push(id.clone());
                }
            }
        }
        if !matched_any && !has_glob_chars(raw) {
            return Err(SelectionError::NoMatch {
                pattern: (*raw).to_string(),
            });
        }
    }

    if !excludes.is_empty() {
        let ex_pats: Vec<glob::Pattern> = excludes
            .iter()
            .map(|p| compile(p))
            .collect::<Result<_, _>>()?;
        selected.retain(|id| {
            let s = id_match_str(id);
            !ex_pats.iter().any(|p| p.matches_with(&s, MATCH_OPTS))
        });
    }

    selected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(selected)
}

/// `require_literal_separator = true` makes `*` stop at `/` (which is
/// our stand-in for `:` after substitution) - that's how `go:*` ends
/// up matching only one segment while `go:**` crosses boundaries.
const MATCH_OPTS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// Translate `:`-separated giant patterns into `/`-separated globs so
/// `glob::Pattern` treats `:` as a separator. `*` then naturally
/// doesn't cross `:` boundaries; `**` does.
fn compile(raw: &str) -> Result<glob::Pattern, SelectionError> {
    let translated = raw.replace(':', "/");
    glob::Pattern::new(&translated).map_err(|e| SelectionError::BadGlob {
        pattern: raw.to_string(),
        error: e,
    })
}

fn id_match_str(id: &TargetId) -> String {
    id.as_str().replace(':', "/")
}

/// Whether a string contains any of the glob metacharacters Giant's
/// selection language recognises (`*`, `?`, `[`). Porcelains that want
/// to switch between literal and pattern matching can call this.
pub fn has_glob_chars(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// A compiled single-pattern matcher that porcelains can use to apply
/// the same selection rules as `giant build`. Same `:` segmentation
/// (`*` stops at `:`, `**` crosses).
#[derive(Debug, Clone)]
pub struct PatternMatcher {
    inner: glob::Pattern,
}

impl PatternMatcher {
    pub fn compile(raw: &str) -> Result<Self, SelectionError> {
        Ok(Self {
            inner: compile(raw)?,
        })
    }

    pub fn matches(&self, id: &TargetId) -> bool {
        self.inner
            .matches_with(&id_match_str(id), MATCH_OPTS)
    }

    pub fn matches_str(&self, id: &str) -> bool {
        let s = id.replace(':', "/");
        self.inner.matches_with(&s, MATCH_OPTS)
    }
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
            inferred_deps: Default::default(),
        }]);
        let aff = affected_targets(&g, &[Path::new("internal/util.go")]);
        assert_eq!(aff, [TargetId::new("discover:go")].into());
    }

    // -------- pattern resolution (TDD-0011) --------

    fn sample_graph() -> BuildGraph {
        graph_with(vec![
            spec("go:bin:server", &[], &["s"], &[]),
            spec("go:bin:client", &[], &["c"], &[]),
            spec("go:lib:util", &[], &["u"], &[]),
            spec("go:test:auth", &[], &["t"], &[]),
            spec("docker:api", &[], &["d"], &[]),
        ])
    }

    fn ids(out: &[TargetId]) -> Vec<&str> {
        out.iter().map(|i| i.as_str()).collect()
    }

    #[test]
    fn empty_patterns_returns_all_non_test_targets_sorted() {
        let g = sample_graph();
        let out = resolve_patterns(&g, &[], TestMode::Exclude, &SelectionOpts::default()).unwrap();
        // go:test:auth has test=false in the fixture, so all 5 show.
        assert_eq!(
            ids(&out),
            vec![
                "docker:api",
                "go:bin:client",
                "go:bin:server",
                "go:lib:util",
                "go:test:auth",
            ]
        );
    }

    #[test]
    fn empty_patterns_excludes_test_targets_when_requested() {
        // Mark go:test:auth as a test target.
        let g = graph_with(vec![
            spec("go:bin:server", &[], &["s"], &[]),
            TargetSpec {
                test: true,
                ..spec("go:test:auth", &[], &["t"], &[])
            },
        ]);
        let out = resolve_patterns(&g, &[], TestMode::Exclude, &SelectionOpts::default()).unwrap();
        assert_eq!(ids(&out), vec!["go:bin:server"]);
    }

    #[test]
    fn exact_id_matches() {
        let g = sample_graph();
        let out = resolve_patterns(
            &g,
            &["go:bin:server".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert_eq!(ids(&out), vec!["go:bin:server"]);
    }

    #[test]
    fn exact_id_typo_errors() {
        let g = sample_graph();
        let err = resolve_patterns(
            &g,
            &["go:bin:srvr".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap_err();
        assert!(matches!(err, SelectionError::NoMatch { .. }));
    }

    #[test]
    fn single_star_does_not_cross_colon() {
        let g = sample_graph();
        // `go:*` should match go:bin / go:lib / go:test segments only,
        // not the deeper `go:bin:server`. Result: nothing in this fixture,
        // because we don't have a target literally named `go:bin`.
        let out = resolve_patterns(
            &g,
            &["go:*".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert!(out.is_empty(), "got {:?}", ids(&out));
    }

    #[test]
    fn single_star_matches_one_segment() {
        let g = sample_graph();
        // `go:bin:*` matches go:bin:server and go:bin:client.
        let out = resolve_patterns(
            &g,
            &["go:bin:*".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert_eq!(ids(&out), vec!["go:bin:client", "go:bin:server"]);
    }

    #[test]
    fn double_star_crosses_colons() {
        let g = sample_graph();
        let out = resolve_patterns(
            &g,
            &["go:**".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert_eq!(
            ids(&out),
            vec![
                "go:bin:client",
                "go:bin:server",
                "go:lib:util",
                "go:test:auth"
            ]
        );
    }

    #[test]
    fn glob_missing_everything_is_silent_empty() {
        // Glob with no matches - not an error (could happen legitimately
        // when no target with that prefix exists yet).
        let g = sample_graph();
        let out = resolve_patterns(
            &g,
            &["rust:**".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn multiple_patterns_union() {
        let g = sample_graph();
        let out = resolve_patterns(
            &g,
            &["go:bin:server".into(), "docker:**".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert_eq!(ids(&out), vec!["docker:api", "go:bin:server"]);
    }

    #[test]
    fn dedupes_overlapping_patterns() {
        let g = sample_graph();
        let out = resolve_patterns(
            &g,
            &["go:**".into(), "go:bin:server".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        // go:bin:server appears once even though both patterns match it.
        assert_eq!(
            ids(&out),
            vec![
                "go:bin:client",
                "go:bin:server",
                "go:lib:util",
                "go:test:auth"
            ]
        );
    }

    #[test]
    fn exclusion_removes_matches() {
        let g = sample_graph();
        let out = resolve_patterns(
            &g,
            &["go:**".into(), "!go:test:*".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert_eq!(
            ids(&out),
            vec!["go:bin:client", "go:bin:server", "go:lib:util"]
        );
    }

    #[test]
    fn exclude_only_implies_match_all() {
        let g = sample_graph();
        let out = resolve_patterns(
            &g,
            &["!go:**".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert_eq!(ids(&out), vec!["docker:api"]);
    }

    #[test]
    fn exclude_doesnt_complain_when_nothing_excluded() {
        let g = sample_graph();
        let out = resolve_patterns(
            &g,
            &["docker:**".into(), "!go:**".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert_eq!(ids(&out), vec!["docker:api"]);
    }

    // -------- test_mode --------

    fn graph_with_test_targets() -> BuildGraph {
        graph_with(vec![
            spec("go:bin:server", &[], &["s"], &[]),
            TargetSpec {
                test: true,
                ..spec("go:test:auth", &[], &["a"], &[])
            },
            TargetSpec {
                test: true,
                ..spec("go:test:store", &[], &["s2"], &[])
            },
        ])
    }

    #[test]
    fn test_mode_only_filters_to_tests() {
        let g = graph_with_test_targets();
        let out = resolve_patterns(&g, &[], TestMode::Only, &SelectionOpts::default()).unwrap();
        assert_eq!(ids(&out), vec!["go:test:auth", "go:test:store"]);
    }

    #[test]
    fn test_mode_exclude_drops_tests() {
        let g = graph_with_test_targets();
        let out = resolve_patterns(&g, &[], TestMode::Exclude, &SelectionOpts::default()).unwrap();
        assert_eq!(ids(&out), vec!["go:bin:server"]);
    }

    #[test]
    fn test_mode_include_returns_everything() {
        let g = graph_with_test_targets();
        let out = resolve_patterns(&g, &[], TestMode::Include, &SelectionOpts::default()).unwrap();
        assert_eq!(
            ids(&out),
            vec!["go:bin:server", "go:test:auth", "go:test:store"]
        );
    }

    #[test]
    fn test_mode_only_rejects_non_test_exact_id() {
        // `giant test go:bin:server` - server isn't a test, so a
        // literal-id miss errors. Catches "did you mean build?".
        let g = graph_with_test_targets();
        let err = resolve_patterns(
            &g,
            &["go:bin:server".into()],
            TestMode::Only,
            &SelectionOpts::default(),
        )
        .unwrap_err();
        assert!(matches!(err, SelectionError::NoMatch { .. }));
    }

    // -------- tag filtering --------

    fn graph_with_tags() -> BuildGraph {
        let mk = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        graph_with(vec![
            TargetSpec {
                tags: mk(&["release", "linux"]),
                ..spec("go:bin:server", &[], &["s"], &[])
            },
            TargetSpec {
                tags: mk(&["release", "macos"]),
                ..spec("go:bin:client", &[], &["c"], &[])
            },
            TargetSpec {
                tags: mk(&["dev"]),
                ..spec("go:bin:devtools", &[], &["d"], &[])
            },
            TargetSpec {
                tags: mk(&["release", "flaky"]),
                ..spec("docker:api", &[], &["a"], &[])
            },
        ])
    }

    #[test]
    fn tag_filter_includes_only_matching() {
        let g = graph_with_tags();
        let opts = SelectionOpts {
            tags: vec!["release".into()],
            ..Default::default()
        };
        let out = resolve_patterns(&g, &[], TestMode::Exclude, &opts).unwrap();
        assert_eq!(
            ids(&out),
            vec!["docker:api", "go:bin:client", "go:bin:server"]
        );
    }

    #[test]
    fn tag_filter_multiple_includes_are_union() {
        let g = graph_with_tags();
        let opts = SelectionOpts {
            tags: vec!["dev".into(), "macos".into()],
            ..Default::default()
        };
        let out = resolve_patterns(&g, &[], TestMode::Exclude, &opts).unwrap();
        // dev → devtools; macos → client.
        assert_eq!(ids(&out), vec!["go:bin:client", "go:bin:devtools"]);
    }

    #[test]
    fn no_tag_filter_excludes_matching() {
        let g = graph_with_tags();
        let opts = SelectionOpts {
            no_tags: vec!["flaky".into()],
            ..Default::default()
        };
        let out = resolve_patterns(&g, &[], TestMode::Exclude, &opts).unwrap();
        assert_eq!(
            ids(&out),
            vec!["go:bin:client", "go:bin:devtools", "go:bin:server"]
        );
    }

    #[test]
    fn tag_and_no_tag_compose_as_and() {
        let g = graph_with_tags();
        let opts = SelectionOpts {
            tags: vec!["release".into()],
            no_tags: vec!["flaky".into()],
        };
        let out = resolve_patterns(&g, &[], TestMode::Exclude, &opts).unwrap();
        assert_eq!(ids(&out), vec!["go:bin:client", "go:bin:server"]);
    }

    #[test]
    fn tag_filter_applies_to_pattern_match_too() {
        let g = graph_with_tags();
        let opts = SelectionOpts {
            tags: vec!["release".into()],
            ..Default::default()
        };
        let out = resolve_patterns(&g, &["go:**".into()], TestMode::Exclude, &opts).unwrap();
        assert_eq!(ids(&out), vec!["go:bin:client", "go:bin:server"]);
    }

    #[test]
    fn tag_filter_makes_literal_pattern_miss_an_error() {
        let g = graph_with_tags();
        let opts = SelectionOpts {
            tags: vec!["release".into()],
            ..Default::default()
        };
        let err = resolve_patterns(&g, &["go:bin:devtools".into()], TestMode::Exclude, &opts)
            .unwrap_err();
        assert!(matches!(err, SelectionError::NoMatch { .. }));
    }
}
