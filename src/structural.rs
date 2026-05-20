//! Structural inputs: line-pattern-filtered file fingerprinting.
//!
//! Stage 1 implementation per TDD-0002: cold computation via filesystem
//! walk (honors `.gitignore` via the `ignore` crate). Per-file fingerprint
//! is sha256 of the concatenated matching lines; global fingerprint is
//! sha256 of `(rel_path, per_file_hash)` pairs in sorted order.
//!
//! Not yet shipped:
//! - Sidecar persistence between runs (TDD-0002 §Sidecar storage)
//! - Git fast-path via `gix` (TDD-0002 §Warm validation)
//! - mtime + size mtime-skips
//!
//! Those land in a follow-up slice. For correctness they're not required;
//! for speed at 10k+ files they are.

use crate::model::ContentHash;
use crate::paths::AbsPath;
use ignore::WalkBuilder;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum StructuralError {
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),

    #[error("walk error in {path}: {error}")]
    Walk { path: String, error: String },
}

/// Compute the global fingerprint for one structural input.
///
/// - `files_globs`: glob patterns relative to workspace root; matches
///   are unioned.
/// - `lines_patterns`: a line contributes if it `starts_with` any of these.
/// - `scope`: limits the walk to these workspace-relative subtrees.
///   Empty = walk the workspace root.
///
/// Files that don't match the globs, files with no matching lines, and
/// unreadable files all contribute nothing. The empty-input case
/// hashes to the sha256-of-empty sentinel.
pub fn compute_fingerprint(
    workspace_root: &AbsPath,
    files_globs: &[String],
    lines_patterns: &[String],
    scope: &[String],
) -> Result<ContentHash, StructuralError> {
    let patterns: Vec<glob::Pattern> = files_globs
        .iter()
        .filter_map(|s| glob::Pattern::new(s).ok())
        .collect();
    if patterns.is_empty() || lines_patterns.is_empty() {
        // Nothing to hash; stable sentinel.
        return Ok(ContentHash::of_bytes(b""));
    }

    let walk_roots: Vec<std::path::PathBuf> = if scope.is_empty() {
        vec![workspace_root.as_path().to_path_buf()]
    } else {
        scope
            .iter()
            .map(|s| workspace_root.as_path().join(s))
            .collect()
    };

    // Per-file hashes, keyed by workspace-relative path. BTreeMap so the
    // global hash incorporates them in canonical (sorted) order.
    let mut per_file: BTreeMap<String, ContentHash> = BTreeMap::new();
    for root in &walk_roots {
        if !root.exists() {
            continue;
        }
        let walker = WalkBuilder::new(root).standard_filters(true).build();
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            let path = entry.path();
            let rel = path
                .strip_prefix(workspace_root.as_path())
                .unwrap_or(path);
            let rel_str = rel.to_string_lossy().into_owned();

            if !patterns.iter().any(|p| p.matches(&rel_str)) {
                continue;
            }

            let Some(fp) = fingerprint_one_file(path, lines_patterns) else {
                continue;
            };
            per_file.insert(rel_str, fp);
        }
    }

    Ok(combine_per_file(&per_file))
}

/// Read a single file and compute the sha256 of its matching lines
/// concatenated with `\0` separators. Returns `None` if the file has no
/// matching lines (so it doesn't appear in the per-file map at all) or
/// if the file is unreadable.
fn fingerprint_one_file(path: &Path, lines_patterns: &[String]) -> Option<ContentHash> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut hasher = ContentHash::hasher();
    let mut any_match = false;
    for line in content.lines() {
        if lines_patterns.iter().any(|p| line.starts_with(p.as_str())) {
            hasher.update(line.as_bytes());
            hasher.update(b"\0");
            any_match = true;
        }
    }
    any_match.then(|| hasher.finalize())
}

fn combine_per_file(per_file: &BTreeMap<String, ContentHash>) -> ContentHash {
    let mut hasher = ContentHash::hasher();
    for (path, hash) in per_file {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_bytes());
        hasher.update(b"\0");
    }
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn ws(tempdir: &tempfile::TempDir) -> AbsPath {
        AbsPath::new(tempdir.path().to_path_buf())
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn empty_inputs_return_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let h = compute_fingerprint(&ws(&dir), &[], &["^import ".into()], &[]).unwrap();
        assert_eq!(h, ContentHash::of_bytes(b""));
    }

    #[test]
    fn empty_lines_return_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.go"), "import x\n");
        let h = compute_fingerprint(&ws(&dir), &["*.go".into()], &[], &[]).unwrap();
        assert_eq!(h, ContentHash::of_bytes(b""));
    }

    #[test]
    fn matching_lines_change_hash() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.go"), "package foo\nimport \"x\"\nfunc f() {}\n");
        let h1 = compute_fingerprint(
            &ws(&dir),
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        // Modify the function body - no matching-line change.
        write(&dir.path().join("a.go"), "package foo\nimport \"x\"\nfunc f() { return }\n");
        let h2 = compute_fingerprint(
            &ws(&dir),
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        assert_eq!(h1, h2, "function body edit must not change structural hash");
        // Modify an import - must change.
        write(&dir.path().join("a.go"), "package foo\nimport \"y\"\nfunc f() { return }\n");
        let h3 = compute_fingerprint(
            &ws(&dir),
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        assert_ne!(h1, h3, "import edit must change structural hash");
    }

    #[test]
    fn deterministic_across_runs() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.go"), "package a\nimport \"x\"\n");
        write(&dir.path().join("b.go"), "package b\nimport \"y\"\n");
        let a = compute_fingerprint(
            &ws(&dir),
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        let b = compute_fingerprint(
            &ws(&dir),
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn adding_new_file_with_matching_lines_changes_hash() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.go"), "package a\nimport \"x\"\n");
        let h1 = compute_fingerprint(
            &ws(&dir),
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        write(&dir.path().join("b.go"), "package b\nimport \"y\"\n");
        let h2 = compute_fingerprint(
            &ws(&dir),
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn adding_file_with_no_matching_lines_doesnt_change_hash() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.go"), "package a\nimport \"x\"\n");
        let h1 = compute_fingerprint(
            &ws(&dir),
            &["*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        // README has no `package ` or `import ` line at start.
        write(&dir.path().join("README.md"), "hello\n");
        let h2 = compute_fingerprint(
            &ws(&dir),
            &["*.go".into(), "*.md".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn scope_limits_walk() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("internal/a.go"), "package a\nimport \"x\"\n");
        write(&dir.path().join("vendor/b.go"), "package b\nimport \"y\"\n");
        let with_scope = compute_fingerprint(
            &ws(&dir),
            &["**/*.go".into()],
            &["package ".into(), "import ".into()],
            &["internal".into()],
        )
        .unwrap();
        let no_scope = compute_fingerprint(
            &ws(&dir),
            &["**/*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        // No-scope walk sees both files; scoped walk sees only internal.
        assert_ne!(with_scope, no_scope);
    }

    #[test]
    fn gitignored_files_excluded() {
        let dir = tempfile::tempdir().unwrap();
        // Initialize git so `ignore` crate recognizes the gitignore.
        write(&dir.path().join(".gitignore"), "gen/\n");
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        write(&dir.path().join("a.go"), "package a\nimport \"x\"\n");
        write(&dir.path().join("gen/auto.go"), "package gen\nimport \"z\"\n");
        let h = compute_fingerprint(
            &ws(&dir),
            &["**/*.go".into()],
            &["package ".into(), "import ".into()],
            &[],
        )
        .unwrap();
        // Should differ if gen/auto.go were included.
        let h_with_only_a = {
            let dir2 = tempfile::tempdir().unwrap();
            write(&dir2.path().join("a.go"), "package a\nimport \"x\"\n");
            compute_fingerprint(
                &ws(&dir2),
                &["**/*.go".into()],
                &["package ".into(), "import ".into()],
                &[],
            )
            .unwrap()
        };
        assert_eq!(h, h_with_only_a, "gitignored files must not contribute");
    }
}
