//! Target selection - turn CLI args into a concrete set of target IDs.
//!
//! Two surfaces live here:
//! 1. **Pattern resolution** ([`resolve_patterns`]) - the user-facing
//!    selection language from TDD-0011: glob/exclusion patterns, tag
//!    filters via [`SelectionOpts`], and test/non-test filtering via
//!    [`TestMode`]. Used by `giant build`, `giant test` (incl. their
//!    `--watch` loop), `giant affected`, and the NDJSON command channel.
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
    /// silently empty; only literals get this error. `suggestion` is the
    /// closest existing label, when one is near enough to be a plausible
    /// fix.
    #[error("no target matches {pattern:?}{}", .suggestion.as_deref().map(|s| format!(" - did you mean {s}?")).unwrap_or_default())]
    NoMatch {
        pattern: String,
        suggestion: Option<String>,
    },
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

/// Resolve a list of user-supplied label patterns against the graph into
/// a concrete, sorted set of target labels.
///
/// Syntax (TDD-0011), matched against `//package:name` labels:
/// - Empty patterns → all targets (subject to test mode + tag filter).
/// - Exact label (`//pkg:name`, or `//pkg/name` shorthand) → one target.
///   A literal miss is an error.
/// - Package `//pkg:*` → every target in that package; recursive
///   `//pkg/...` (and `//...`) crosses into subpackages. `*` stops at a
///   `/` boundary; `...` is the only construct that crosses packages.
/// - `!pattern` excludes whatever the pattern matches.
/// - Multiple positionals are unioned; exclusions apply after.
/// - If only excludes are given, the implicit include is `//...`.
///
/// `test_mode` controls whether `test: true` targets are eligible.
/// `opts` carries tag filters. Both apply *before* the pattern match,
/// so the user sees consistent results regardless of how broad the
/// pattern is.
///
/// Output is sorted lexicographically by label, so the order is stable
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
        includes.push("//...");
    }

    let mut selected: Vec<TargetId> = Vec::new();
    let mut seen: HashSet<TargetId> = HashSet::new();
    for raw in &includes {
        let pat = LabelPattern::parse(raw)?;
        let mut matched_any = false;
        for (id, spec) in graph.iter() {
            if !eligible(spec) {
                continue;
            }
            if pat.matches(id) {
                matched_any = true;
                if seen.insert(id.clone()) {
                    selected.push(id.clone());
                }
            }
        }
        if !matched_any && !has_glob_chars(raw) {
            return Err(SelectionError::NoMatch {
                pattern: (*raw).to_string(),
                suggestion: closest_label(graph, raw),
            });
        }
    }

    if !excludes.is_empty() {
        let ex_pats: Vec<LabelPattern> = excludes
            .iter()
            .map(|p| LabelPattern::parse(p))
            .collect::<Result<_, _>>()?;
        selected.retain(|id| !ex_pats.iter().any(|p| p.matches(id)));
    }

    selected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(selected)
}

/// `require_literal_separator = true` makes `*` stop at `/`, so a package
/// glob like `//src/*:bin` matches one path segment, while `...` is the
/// only construct that crosses package boundaries (TDD-0011).
const MATCH_OPTS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// A parsed label pattern (TDD-0011 §Pattern syntax). Matches against
/// `//<package>:<name>` labels.
#[derive(Debug, Clone)]
enum LabelPattern {
    /// `//prefix/...` (or `//...`): the package equals `prefix` or is a
    /// subpackage of it. `prefix` empty = the whole workspace.
    Recursive { prefix: String },
    /// `//pkgpat:namepat`: glob the package and name segments. A bare
    /// `//a/b/c` is shorthand for `//a/b/c:c` (name = last segment).
    Exact {
        pkg: glob::Pattern,
        name: glob::Pattern,
    },
}

impl LabelPattern {
    fn parse(raw: &str) -> Result<Self, SelectionError> {
        let body = raw.strip_prefix("//").unwrap_or(raw);
        if let Some(prefix) = body.strip_suffix("...") {
            let prefix = prefix.strip_suffix('/').unwrap_or(prefix);
            return Ok(LabelPattern::Recursive {
                prefix: prefix.to_string(),
            });
        }
        let (pkg, name) = match body.rsplit_once(':') {
            Some((p, n)) => (p, n),
            // `//a/b/c` shorthand → package `a/b/c`, name = last segment.
            None => (body, body.rsplit_once('/').map_or(body, |(_, n)| n)),
        };
        Ok(LabelPattern::Exact {
            pkg: glob_pat(pkg)?,
            name: glob_pat(name)?,
        })
    }

    fn matches(&self, id: &TargetId) -> bool {
        let (lpkg, lname) = id.split();
        match self {
            // `prefix` empty = whole workspace; otherwise the package is
            // `prefix` itself or a subpackage (`prefix` then `/`).
            LabelPattern::Recursive { prefix } => {
                prefix.is_empty()
                    || lpkg
                        .strip_prefix(prefix.as_str())
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            }
            LabelPattern::Exact { pkg, name } => {
                pkg.matches_with(lpkg, MATCH_OPTS) && name.matches_with(lname, MATCH_OPTS)
            }
        }
    }
}

fn glob_pat(s: &str) -> Result<glob::Pattern, SelectionError> {
    glob::Pattern::new(s).map_err(|e| SelectionError::BadGlob {
        pattern: s.to_string(),
        error: e,
    })
}

/// Whether a pattern is a glob (a miss contributes zero) rather than a
/// literal label (a miss is a typo → error). Recognises `*`/`?`/`[` and
/// the recursive `...`.
pub fn has_glob_chars(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[') || s.contains("...")
}

/// The graph label closest to `pattern` by edit distance, when one is near
/// enough to be a plausible typo (so a wild miss doesn't get a nonsense
/// suggestion). Used to turn a literal miss into a "did you mean …?".
fn closest_label(graph: &BuildGraph, pattern: &str) -> Option<String> {
    let (dist, label) = graph
        .iter()
        .map(|(id, _)| (levenshtein(pattern, id.as_str()), id.as_str()))
        .min_by_key(|(d, _)| *d)?;
    // Within a third of the pattern length (or 2 edits) reads as a typo.
    (dist <= 2 || dist * 3 <= pattern.len()).then(|| label.to_string())
}

/// Levenshtein edit distance, two-row DP.
fn levenshtein(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut curr = vec![0usize; b_chars.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_chars.len()]
}

/// A compiled single-pattern matcher that porcelains can use to apply
/// the same label-selection rules as `giant build` (TDD-0011).
#[derive(Debug, Clone)]
pub struct PatternMatcher {
    inner: LabelPattern,
}

impl PatternMatcher {
    pub fn compile(raw: &str) -> Result<Self, SelectionError> {
        Ok(Self {
            inner: LabelPattern::parse(raw)?,
        })
    }

    pub fn matches(&self, id: &TargetId) -> bool {
        self.inner.matches(id)
    }

    pub fn matches_str(&self, id: &str) -> bool {
        self.inner.matches(&TargetId::new(id))
    }
}

/// Set of targets whose inputs match any of the given changed files,
/// plus everything transitively downstream of them.
///
/// "Matches" means: any input glob on the target `Pattern::matches` the
/// workspace-relative path. We don't try to resolve "is this file
/// ACTUALLY going to change the cache key?" -
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
        let Input::File { glob } = input;
        let Ok(pattern) = glob::Pattern::new(glob.as_str()) else {
            continue;
        };
        if files.iter().any(|f| pattern.matches(f)) {
            return true;
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
        // Accept a full `//pkg:name` label (take the name) or a bare id
        // (use it as the name) - affected tests use bare ids, pattern
        // tests use labels.
        let name = id.rsplit_once(':').map_or(id, |(_, n)| n);
        TargetSpec {
            name: name.to_string(),
            id: TargetId::new(id),
            inputs: inputs
                .iter()
                .map(|g| Input::File {
                    glob: GlobPattern::new(*g).unwrap(),
                })
                .collect(),
            outputs_raw: Vec::new(),
            outputs: outputs
                .iter()
                .map(|o| OutputPath::new(*o).unwrap())
                .collect(),
            deps: deps.iter().map(|d| TargetId::new(*d)).collect(),
            command: "true".into(),
            cwd_raw: None,
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
            prune_dirs: Vec::new(),
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

    // -------- pattern resolution (TDD-0011) --------

    fn sample_graph() -> BuildGraph {
        graph_with(vec![
            spec("//go/bin:server", &[], &["s"], &[]),
            spec("//go/bin:client", &[], &["c"], &[]),
            spec("//go/lib:util", &[], &["u"], &[]),
            spec("//go/test:auth", &[], &["t"], &[]),
            spec("//docker:api", &[], &["d"], &[]),
        ])
    }

    fn ids(out: &[TargetId]) -> Vec<&str> {
        out.iter().map(|i| i.as_str()).collect()
    }

    #[test]
    fn empty_patterns_returns_all_non_test_targets_sorted() {
        let g = sample_graph();
        let out = resolve_patterns(&g, &[], TestMode::Exclude, &SelectionOpts::default()).unwrap();
        // None are test targets in the fixture, so all 5 show.
        assert_eq!(
            ids(&out),
            vec![
                "//docker:api",
                "//go/bin:client",
                "//go/bin:server",
                "//go/lib:util",
                "//go/test:auth",
            ]
        );
    }

    #[test]
    fn empty_patterns_excludes_test_targets_when_requested() {
        let g = graph_with(vec![
            spec("//go/bin:server", &[], &["s"], &[]),
            TargetSpec {
                test: true,
                ..spec("//go/test:auth", &[], &["t"], &[])
            },
        ]);
        let out = resolve_patterns(&g, &[], TestMode::Exclude, &SelectionOpts::default()).unwrap();
        assert_eq!(ids(&out), vec!["//go/bin:server"]);
    }

    #[test]
    fn exact_label_matches() {
        let g = sample_graph();
        let out = resolve_patterns(
            &g,
            &["//go/bin:server".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert_eq!(ids(&out), vec!["//go/bin:server"]);
    }

    #[test]
    fn exact_label_typo_errors_and_suggests_closest() {
        let g = sample_graph();
        let err = resolve_patterns(
            &g,
            &["//go/bin:srvr".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap_err();
        match err {
            SelectionError::NoMatch { suggestion, .. } => {
                assert_eq!(suggestion.as_deref(), Some("//go/bin:server"));
            }
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }

    #[test]
    fn wild_miss_gets_no_suggestion() {
        let g = sample_graph();
        let err = resolve_patterns(
            &g,
            &["//completely/different:thing".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap_err();
        match err {
            SelectionError::NoMatch { suggestion, .. } => assert_eq!(suggestion, None),
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }

    #[test]
    fn package_pattern_does_not_cross_into_subpackages() {
        let g = sample_graph();
        // `//go:*` is the `go` package exactly - but every target lives in
        // a subpackage (go/bin, go/lib, go/test), so nothing matches.
        let out = resolve_patterns(
            &g,
            &["//go:*".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert!(out.is_empty(), "got {:?}", ids(&out));
    }

    #[test]
    fn package_star_matches_one_package() {
        let g = sample_graph();
        // `//go/bin:*` is every target in package go/bin.
        let out = resolve_patterns(
            &g,
            &["//go/bin:*".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert_eq!(ids(&out), vec!["//go/bin:client", "//go/bin:server"]);
    }

    #[test]
    fn recursive_crosses_package_boundaries() {
        let g = sample_graph();
        let out = resolve_patterns(
            &g,
            &["//go/...".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert_eq!(
            ids(&out),
            vec![
                "//go/bin:client",
                "//go/bin:server",
                "//go/lib:util",
                "//go/test:auth"
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
            &["//rust/...".into()],
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
            &["//go/bin:server".into(), "//docker/...".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert_eq!(ids(&out), vec!["//docker:api", "//go/bin:server"]);
    }

    #[test]
    fn dedupes_overlapping_patterns() {
        let g = sample_graph();
        let out = resolve_patterns(
            &g,
            &["//go/...".into(), "//go/bin:server".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        // //go/bin:server appears once even though both patterns match it.
        assert_eq!(
            ids(&out),
            vec![
                "//go/bin:client",
                "//go/bin:server",
                "//go/lib:util",
                "//go/test:auth"
            ]
        );
    }

    #[test]
    fn exclusion_removes_matches() {
        let g = sample_graph();
        let out = resolve_patterns(
            &g,
            &["//go/...".into(), "!//go/test/...".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert_eq!(
            ids(&out),
            vec!["//go/bin:client", "//go/bin:server", "//go/lib:util"]
        );
    }

    #[test]
    fn exclude_only_implies_match_all() {
        let g = sample_graph();
        let out = resolve_patterns(
            &g,
            &["!//go/...".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert_eq!(ids(&out), vec!["//docker:api"]);
    }

    #[test]
    fn exclude_doesnt_complain_when_nothing_excluded() {
        let g = sample_graph();
        let out = resolve_patterns(
            &g,
            &["//docker/...".into(), "!//go/...".into()],
            TestMode::Exclude,
            &SelectionOpts::default(),
        )
        .unwrap();
        assert_eq!(ids(&out), vec!["//docker:api"]);
    }

    // -------- test_mode --------

    fn graph_with_test_targets() -> BuildGraph {
        graph_with(vec![
            spec("//go/bin:server", &[], &["s"], &[]),
            TargetSpec {
                test: true,
                ..spec("//go/test:auth", &[], &["a"], &[])
            },
            TargetSpec {
                test: true,
                ..spec("//go/test:store", &[], &["s2"], &[])
            },
        ])
    }

    #[test]
    fn test_mode_only_filters_to_tests() {
        let g = graph_with_test_targets();
        let out = resolve_patterns(&g, &[], TestMode::Only, &SelectionOpts::default()).unwrap();
        assert_eq!(ids(&out), vec!["//go/test:auth", "//go/test:store"]);
    }

    #[test]
    fn test_mode_exclude_drops_tests() {
        let g = graph_with_test_targets();
        let out = resolve_patterns(&g, &[], TestMode::Exclude, &SelectionOpts::default()).unwrap();
        assert_eq!(ids(&out), vec!["//go/bin:server"]);
    }

    #[test]
    fn test_mode_include_returns_everything() {
        let g = graph_with_test_targets();
        let out = resolve_patterns(&g, &[], TestMode::Include, &SelectionOpts::default()).unwrap();
        assert_eq!(
            ids(&out),
            vec!["//go/bin:server", "//go/test:auth", "//go/test:store"]
        );
    }

    #[test]
    fn test_mode_only_rejects_non_test_exact_label() {
        // `giant test //go/bin:server` - server isn't a test, so a
        // literal-label miss errors. Catches "did you mean build?".
        let g = graph_with_test_targets();
        let err = resolve_patterns(
            &g,
            &["//go/bin:server".into()],
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
                ..spec("//go/bin:server", &[], &["s"], &[])
            },
            TargetSpec {
                tags: mk(&["release", "macos"]),
                ..spec("//go/bin:client", &[], &["c"], &[])
            },
            TargetSpec {
                tags: mk(&["dev"]),
                ..spec("//go/bin:devtools", &[], &["d"], &[])
            },
            TargetSpec {
                tags: mk(&["release", "flaky"]),
                ..spec("//docker:api", &[], &["a"], &[])
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
            vec!["//docker:api", "//go/bin:client", "//go/bin:server"]
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
        assert_eq!(ids(&out), vec!["//go/bin:client", "//go/bin:devtools"]);
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
            vec!["//go/bin:client", "//go/bin:devtools", "//go/bin:server"]
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
        assert_eq!(ids(&out), vec!["//go/bin:client", "//go/bin:server"]);
    }

    #[test]
    fn tag_filter_applies_to_pattern_match_too() {
        let g = graph_with_tags();
        let opts = SelectionOpts {
            tags: vec!["release".into()],
            ..Default::default()
        };
        let out = resolve_patterns(&g, &["//go/...".into()], TestMode::Exclude, &opts).unwrap();
        assert_eq!(ids(&out), vec!["//go/bin:client", "//go/bin:server"]);
    }

    #[test]
    fn tag_filter_makes_literal_pattern_miss_an_error() {
        let g = graph_with_tags();
        let opts = SelectionOpts {
            tags: vec!["release".into()],
            ..Default::default()
        };
        let err = resolve_patterns(&g, &["//go/bin:devtools".into()], TestMode::Exclude, &opts)
            .unwrap_err();
        assert!(matches!(err, SelectionError::NoMatch { .. }));
    }
}
