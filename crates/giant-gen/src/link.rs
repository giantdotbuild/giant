//! The dependency link pass.
//!
//! After generators emit, this resolves each target's input globs to the
//! targets that produce them and writes the resulting `deps:` into the
//! generated `giant.<infix>.yaml` files. Dependency inference is no longer an
//! engine concern; the engine reads explicit
//! deps only. The matching is identical to the engine's old pass -- pure
//! glob-vs-output-string, no filesystem access -- run once at generation over
//! the whole workspace.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use giant::model::{Input, TargetSpec};
use giant::{Config, TargetId};
use giant_schema::Document;

/// Run the link pass over the workspace rooted at `root`: scan every config,
/// infer output-to-producer edges, and fill `deps:` into the generated files.
/// Returns the number of generated files rewritten.
pub fn run(root: &Path) -> Result<usize> {
    let cfg = Config::scan(root).context("scanning workspace for dep resolution")?;
    let inferred = infer(&cfg.targets)?;

    let mut rewritten = 0;
    for path in generated_files(root) {
        let package = package_of(root, &path);
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut doc: Document =
            serde_yaml_ng::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        if !fill(&mut doc, &package, &inferred) {
            continue;
        }
        let yaml = serde_yaml_ng::to_string(&doc)
            .with_context(|| format!("serializing {}", path.display()))?;
        if yaml != raw {
            std::fs::write(&path, yaml).with_context(|| format!("writing {}", path.display()))?;
            rewritten += 1;
        }
    }
    Ok(rewritten)
}

/// output-path to producer, then for each target's input globs, every producer
/// whose output the glob matches (excluding self). A duplicate output is a
/// config error. Pure string/glob matching: the edge set is a function of
/// declared inputs and outputs only.
fn infer(targets: &[TargetSpec]) -> Result<HashMap<TargetId, BTreeSet<TargetId>>> {
    let mut producer: HashMap<String, TargetId> = HashMap::new();
    for t in targets {
        for o in &t.outputs {
            let path = o.as_path().to_string_lossy().into_owned();
            if let Some(prev) = producer.insert(path.clone(), t.id.clone()) {
                bail!(
                    "two targets produce the same output {path}: {prev} and {}",
                    t.id
                );
            }
        }
    }

    let mut deps: HashMap<TargetId, BTreeSet<TargetId>> = HashMap::new();
    for t in targets {
        for input in &t.inputs {
            let Input::File { glob } = input;
            let Ok(pattern) = glob::Pattern::new(glob.as_str()) else {
                continue;
            };
            for (output_path, prod) in &producer {
                if *prod != t.id && pattern.matches(output_path) {
                    deps.entry(t.id.clone()).or_default().insert(prod.clone());
                }
            }
        }
    }
    Ok(deps)
}

/// Merge inferred producers into each target's `deps` (sorted, deduped). Returns
/// whether anything changed. Targets with no inferred deps are left untouched,
/// so explicit-only files do not churn.
fn fill(
    doc: &mut Document,
    package: &str,
    inferred: &HashMap<TargetId, BTreeSet<TargetId>>,
) -> bool {
    let mut changed = false;
    for t in &mut doc.targets {
        let label = TargetId::label(package, &t.name);
        let Some(producers) = inferred.get(&label) else {
            continue;
        };
        let mut merged: BTreeSet<String> = t.deps.iter().cloned().collect();
        for p in producers {
            merged.insert(p.as_str().to_string());
        }
        let merged: Vec<String> = merged.into_iter().collect();
        if merged != t.deps {
            t.deps = merged;
            changed = true;
        }
    }
    changed
}

/// Generated config files (`giant.<infix>.yaml`, non-empty infix) under `root`,
/// respecting gitignore like the engine's scan. The hand-written `giant.yaml`
/// (empty infix) is excluded -- the pass never rewrites user files.
fn generated_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .hidden(false)
        .build()
        .flatten()
    {
        if entry.file_type().is_some_and(|t| t.is_file())
            && entry.file_name().to_str().is_some_and(is_generated)
        {
            out.push(entry.into_path());
        }
    }
    out
}

/// The package a config file belongs to: its parent dir relative to `root`,
/// slash-separated, `""` at the root.
fn package_of(root: &Path, path: &Path) -> String {
    path.parent()
        .and_then(|d| d.strip_prefix(root).ok())
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

/// `giant.<infix>.{yaml,yml,json}` with a non-empty, filename-safe infix.
pub(crate) fn is_generated(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("giant.") else {
        return false;
    };
    let Some((infix, ext)) = rest.rsplit_once('.') else {
        return false;
    };
    !infix.is_empty()
        && matches!(ext, "yaml" | "yml" | "json")
        && infix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests;
