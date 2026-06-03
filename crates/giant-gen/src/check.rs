//! `giant gen --check` - the staleness gate (TDD-0022 §E).
//!
//! Per generator: regenerate into a scratch root, enumerate the
//! `giant.<name>.yaml` files it owns in both the committed tree and the
//! scratch output, and diff them. Added / removed / content-changed files are
//! drift; any non-owned file the generator wrote under scratch is an ownership
//! violation. Exit nonzero if anything is stale or misbehaved.

use crate::config::Generator;
use crate::run;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;

/// Run `--check` over the selected generators, printing a per-generator
/// report. Returns the process exit code (0 = all clean).
pub async fn run(generators: &[Generator], root: &Path, state_dir: &str) -> Result<i32> {
    let mut clean = true;
    for g in generators {
        let report = check_one(g, root, state_dir).await?;
        report.print();
        clean &= report.is_clean();
    }
    if !clean {
        eprintln!("error: a generator is stale; run `giant gen <name>` and commit the result");
    }
    Ok(if clean { 0 } else { 1 })
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

async fn check_one(g: &Generator, root: &Path, state_dir: &str) -> Result<Report> {
    let scratch = root.join(state_dir).join("gen-check").join(&g.name);
    if scratch.exists() {
        std::fs::remove_dir_all(&scratch)
            .with_context(|| format!("clearing scratch dir {}", scratch.display()))?;
    }
    std::fs::create_dir_all(&scratch)
        .with_context(|| format!("creating scratch dir {}", scratch.display()))?;

    // Run the generator into the scratch root, capturing output for the report.
    let out = run::command(g, root, &scratch)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await;
    match out {
        Ok(o) if !o.status.success() => {
            return Ok(failed(
                g,
                String::from_utf8_lossy(&o.stderr).trim().to_string(),
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(failed(
                g,
                format!("command '{}' not found on PATH", g.command),
            ));
        }
        Err(e) => return Err(e.into()),
    }

    let committed = enumerate(root, &g.name, Some(state_dir))?;
    let Scratch {
        owned: generated,
        violations,
    } = enumerate_scratch(&scratch, &g.name)?;

    let mut drift = Vec::new();
    for (rel, bytes) in &generated {
        match committed.get(rel) {
            None => drift.push(Drift::Added(rel.clone())),
            Some(c) if c != bytes => drift.push(Drift::Changed(rel.clone())),
            Some(_) => {}
        }
    }
    for rel in committed.keys() {
        if !generated.contains_key(rel) {
            drift.push(Drift::Removed(rel.clone()));
        }
    }
    drift.sort_by_key(drift_path);

    Ok(Report {
        name: g.name.clone(),
        failed: None,
        empty_output: generated.is_empty() && !committed.is_empty(),
        drift,
        violations,
    })
}

fn failed(g: &Generator, message: String) -> Report {
    Report {
        name: g.name.clone(),
        failed: Some(message),
        drift: Vec::new(),
        violations: Vec::new(),
        empty_output: false,
    }
}

fn drift_path(d: &Drift) -> String {
    match d {
        Drift::Added(p) | Drift::Removed(p) | Drift::Changed(p) => p.clone(),
    }
}

/// Owned `giant.<name>.yaml` files under `dir`, keyed by path relative to
/// `dir`. The walk is **not** gitignore-aware: a generator writes its files
/// regardless of ignore rules, so the committed set must be compared the same
/// way (a generated file that happens to sit in a gitignored directory still
/// has to match a fresh run). `.git` and `skip` (the state dir, which holds
/// the scratch tree) are pruned.
fn enumerate(dir: &Path, name: &str, skip: Option<&str>) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    for entry in ignore::WalkBuilder::new(dir)
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
    for entry in ignore::WalkBuilder::new(scratch)
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

fn rel_slash(base: &Path, p: &Path) -> String {
    p.strip_prefix(base)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::is_owned;

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
}
