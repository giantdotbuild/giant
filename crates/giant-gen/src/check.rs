//! `giant gen --check` - the staleness gate.
//!
//! A fresh `giant gen` is two phases: every generator emits, then a global link
//! pass fills inferred `deps:` into the generated files. So `--check`
//! must reproduce *both* before diffing. It mirrors the committed config tree
//! into a merged scratch root, overlays each selected generator's freshly
//! produced output, runs the link pass over the whole mirror, then diffs each
//! generator's owned files (post-link) against the committed tree. Per
//! generator, added / removed / content-changed files are drift, and any
//! non-owned file it wrote is an ownership violation.

use crate::config::Generator;
use crate::link;
use crate::run;
use anyhow::{Context, Result};
use ignore::WalkBuilder;
use std::collections::BTreeMap;
use std::path::Path;

/// Run `--check` over the selected generators, printing a per-generator
/// report. Returns the process exit code (0 = all clean).
pub async fn run(generators: &[Generator], root: &Path, state_dir: &str) -> Result<i32> {
    // A mirror of the committed config tree; the link pass needs the whole
    // workspace (cross-generator and hand-written producers) to resolve deps.
    let merged = root.join(state_dir).join("gen-check").join("_merged");
    reset_dir(&merged)?;
    copy_committed_configs(root, &merged, state_dir)?;

    // Produce each generator into its own scratch (for ownership + the produced
    // file set), then overlay its fresh output into the mirror.
    let mut produced: Vec<(String, Outcome)> = Vec::new();
    for g in generators {
        let scratch = root.join(state_dir).join("gen-check").join(g.name());
        reset_dir(&scratch)?;
        match run::produce_quiet(g, root, &scratch).await? {
            run::Produced::Failed(msg) => {
                produced.push((g.name().to_string(), Outcome::Failed(msg)))
            }
            run::Produced::Ran => {
                let Scratch { owned, violations } = enumerate_scratch(&scratch, g.name())?;
                overlay(&merged, g.name(), &owned)?;
                produced.push((g.name().to_string(), Outcome::Ran { owned, violations }));
            }
        }
    }

    // Link the mirror, so the diff compares the fully resolved output.
    let to_link = merged.clone();
    if let Err(e) = tokio::task::spawn_blocking(move || link::run(&to_link)).await? {
        eprintln!("error: dep link failed during check: {e:#}");
        return Ok(1);
    }

    let mut clean = true;
    for (name, outcome) in produced {
        let report = match outcome {
            Outcome::Failed(msg) => failed(&name, msg),
            Outcome::Ran { owned, violations } => {
                diff(&name, &owned, violations, &merged, root, state_dir)?
            }
        };
        report.print();
        clean &= report.is_clean();
    }
    if !clean {
        eprintln!("error: a generator is stale; run `giant gen <name>` and commit the result");
    }
    Ok(if clean { 0 } else { 1 })
}

/// A generator's produce result: it failed, or it ran and these are its owned
/// files (pre-link) and any ownership violations.
enum Outcome {
    Failed(String),
    Ran {
        owned: BTreeMap<String, Vec<u8>>,
        violations: Vec<String>,
    },
}

enum Drift {
    Added(String),   // generated, missing from the tree (needs commit)
    Removed(String), // in the tree, no longer generated (stale)
    Changed(String), // present in both, content differs
}

struct Report {
    name: String,
    failed: Option<String>, // the generator did not run cleanly
    drift: Vec<Drift>,
    violations: Vec<String>, // non-owned files written under scratch
    empty_output: bool,      // produced nothing but the tree has owned files
}

impl Report {
    fn is_clean(&self) -> bool {
        self.failed.is_none() && self.drift.is_empty() && self.violations.is_empty()
    }

    fn print(&self) {
        if self.is_clean() {
            println!("{}\tok", self.name);
            return;
        }
        if let Some(msg) = &self.failed {
            println!("{}\tFAILED", self.name);
            for line in msg.lines() {
                println!("  {line}");
            }
            return;
        }
        println!("{}\tDRIFT", self.name);
        if self.empty_output {
            println!("  (generator wrote nothing under GIANT_GEN_OUT - does it honor it?)");
        }
        for d in &self.drift {
            match d {
                Drift::Added(p) => println!("  + {p}   (missing from tree)"),
                Drift::Removed(p) => println!("  - {p}   (stale; not regenerated)"),
                Drift::Changed(p) => println!("  ~ {p}   (content differs)"),
            }
        }
        for v in &self.violations {
            println!("  ! {v}   (generator wrote a file it does not own)");
        }
    }
}

/// Diff a generator's produced files (read post-link from the mirror) against
/// the committed tree.
fn diff(
    name: &str,
    owned: &BTreeMap<String, Vec<u8>>,
    violations: Vec<String>,
    merged: &Path,
    root: &Path,
    state_dir: &str,
) -> Result<Report> {
    let committed = enumerate(root, name, Some(state_dir))?;

    let mut drift = Vec::new();
    for rel in owned.keys() {
        let linked =
            std::fs::read(merged.join(rel)).with_context(|| format!("reading linked {rel}"))?;
        match committed.get(rel) {
            None => drift.push(Drift::Added(rel.clone())),
            Some(c) if *c != linked => drift.push(Drift::Changed(rel.clone())),
            Some(_) => {}
        }
    }
    for rel in committed.keys() {
        if !owned.contains_key(rel) {
            drift.push(Drift::Removed(rel.clone()));
        }
    }
    drift.sort_by(|a, b| drift_path(a).cmp(drift_path(b)));

    Ok(Report {
        name: name.to_string(),
        failed: None,
        empty_output: owned.is_empty() && !committed.is_empty(),
        drift,
        violations,
    })
}

fn failed(name: &str, message: String) -> Report {
    Report {
        name: name.to_string(),
        failed: Some(message),
        drift: Vec::new(),
        violations: Vec::new(),
        empty_output: false,
    }
}

fn drift_path(d: &Drift) -> &str {
    match d {
        Drift::Added(p) | Drift::Removed(p) | Drift::Changed(p) => p,
    }
}

/// Reset a scratch directory to empty.
fn reset_dir(p: &Path) -> Result<()> {
    if p.exists() {
        std::fs::remove_dir_all(p).with_context(|| format!("clearing {}", p.display()))?;
    }
    std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?;
    Ok(())
}

/// Copy every committed config file (`giant.yaml` and `giant.<infix>.yaml`)
/// into the mirror, preserving structure. Not gitignore-aware, matching how
/// `enumerate` reads the committed set; the state dir (holding the scratch) and
/// `.git` are skipped.
fn copy_committed_configs(root: &Path, merged: &Path, state_dir: &str) -> Result<()> {
    for entry in WalkBuilder::new(root)
        .standard_filters(false)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
    {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Some(fname) = entry.file_name().to_str() else {
            continue;
        };
        if !is_config(fname) {
            continue;
        }
        let rel = rel_slash(root, entry.path());
        if rel == state_dir || rel.starts_with(&format!("{state_dir}/")) {
            continue;
        }
        let dest = merged.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(entry.path(), &dest)?;
    }
    Ok(())
}

/// Replace generator `name`'s files in the mirror with its freshly produced
/// output: drop its prior (committed-copy) files so stale outputs do not
/// pollute inference, then write the produced ones.
fn overlay(merged: &Path, name: &str, owned: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    for entry in WalkBuilder::new(merged)
        .standard_filters(false)
        .hidden(false)
        .build()
        .flatten()
    {
        if entry.file_type().is_some_and(|t| t.is_file())
            && entry
                .file_name()
                .to_str()
                .is_some_and(|f| is_owned(f, name))
        {
            std::fs::remove_file(entry.path())?;
        }
    }
    for (rel, bytes) in owned {
        let dest = merged.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, bytes)?;
    }
    Ok(())
}

/// Owned `giant.<name>.yaml` files under `dir`, keyed by path relative to
/// `dir`. The walk is **not** gitignore-aware: a generator writes its files
/// regardless of ignore rules, so the committed set must be compared the same
/// way. `.git` and `skip` (the state dir, which holds the scratch tree) are
/// pruned.
fn enumerate(dir: &Path, name: &str, skip: Option<&str>) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    for entry in WalkBuilder::new(dir)
        .standard_filters(false)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
    {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Some(fname) = entry.file_name().to_str() else {
            continue;
        };
        if !is_owned(fname, name) {
            continue;
        }
        let rel = rel_slash(dir, entry.path());
        if let Some(s) = skip
            && (rel == s || rel.starts_with(&format!("{s}/")))
        {
            continue;
        }
        out.insert(rel, std::fs::read(entry.path())?);
    }
    Ok(out)
}

/// What a scratch walk found: the generator's owned files (keyed by relative
/// path) and any non-owned files it wrote (ownership violations).
struct Scratch {
    owned: BTreeMap<String, Vec<u8>>,
    violations: Vec<String>,
}

/// Files under the scratch root. Walk everything - scratch has no .gitignore
/// to respect.
fn enumerate_scratch(scratch: &Path, name: &str) -> Result<Scratch> {
    let mut owned = BTreeMap::new();
    let mut violations = Vec::new();
    for entry in WalkBuilder::new(scratch)
        .standard_filters(false)
        .hidden(false)
        .build()
        .flatten()
    {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = rel_slash(scratch, entry.path());
        let fname = entry.file_name().to_str().unwrap_or_default();
        if is_owned(fname, name) {
            owned.insert(rel, std::fs::read(entry.path())?);
        } else {
            violations.push(rel);
        }
    }
    violations.sort();
    Ok(Scratch { owned, violations })
}

/// `giant.<name>.{yaml,yml,json}` - the files generator `<name>` owns.
fn is_owned(fname: &str, name: &str) -> bool {
    fname
        .strip_prefix("giant.")
        .and_then(|rest| rest.strip_prefix(name))
        .is_some_and(|ext| matches!(ext, ".yaml" | ".yml" | ".json"))
}

/// Any giant config filename: the primary `giant.yaml` or a generated
/// `giant.<infix>.yaml` variant.
fn is_config(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("giant.") else {
        return false;
    };
    let (infix, ext) = rest.rsplit_once('.').unwrap_or(("", rest));
    matches!(ext, "yaml" | "yml" | "json")
        && (infix.is_empty()
            || infix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
}

fn rel_slash(base: &Path, p: &Path) -> String {
    p.strip_prefix(base)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::{is_config, is_owned};

    #[test]
    fn is_owned_matches_infix_and_exts() {
        assert!(is_owned("giant.go.yaml", "go"));
        assert!(is_owned("giant.go.yml", "go"));
        assert!(is_owned("giant.go.json", "go"));
        assert!(!is_owned("giant.yaml", "go")); // hand-written namespace
        assert!(!is_owned("giant.docker.yaml", "go")); // another generator
        assert!(!is_owned("giant.gopher.yaml", "go")); // not the same infix
        assert!(!is_owned("giant.go.txt", "go"));
    }

    #[test]
    fn is_config_matches_primary_and_infix() {
        assert!(is_config("giant.yaml"));
        assert!(is_config("giant.json"));
        assert!(is_config("giant.go.yaml"));
        assert!(!is_config("giant.go.bar.yaml"));
        assert!(!is_config("giants.yaml"));
    }
}
