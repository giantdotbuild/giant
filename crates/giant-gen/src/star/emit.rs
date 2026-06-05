//! Grouping and deterministic emit (TDD-0024 §E/§F): a flat list of targets is
//! grouped into one `giant.<infix>.yaml` per package, written under the output
//! root, and files the generator no longer produces are pruned.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use giant_schema::{Document, WireTarget};

use super::Emitted;

/// Group targets by owning package, sorted by package then by target name.
/// A duplicate name within one package is a hard error (it would be a label
/// collision once written into that package's file).
fn group(targets: Vec<Emitted>) -> Result<BTreeMap<String, Vec<WireTarget>>> {
    let mut groups: BTreeMap<String, Vec<WireTarget>> = BTreeMap::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for e in targets {
        if !seen.insert((e.package.clone(), e.wire.name.clone())) {
            bail!(
                "duplicate target '{}' in package '{}'",
                e.wire.name,
                e.package
            );
        }
        groups.entry(e.package).or_default().push(e.wire);
    }
    for targets in groups.values_mut() {
        targets.sort_by(|a, b| a.name.cmp(&b.name));
    }
    Ok(groups)
}

/// The file a package's targets are written to: `<out_root>/<package>/giant.<infix>.yaml`.
fn file_for(out_root: &Path, package: &str, infix: &str) -> PathBuf {
    let name = format!("giant.{infix}.yaml");
    if package.is_empty() {
        out_root.join(name)
    } else {
        out_root.join(package).join(name)
    }
}

/// Write each group's `giant.<infix>.yaml` under `out_root`, then remove any
/// `giant.<infix>.yaml` the generator no longer owns. Returns the written paths.
pub(crate) fn write(targets: Vec<Emitted>, infix: &str, out_root: &Path) -> Result<Vec<PathBuf>> {
    let groups = group(targets)?;

    let mut written = Vec::with_capacity(groups.len());
    for (package, targets) in groups {
        let path = file_for(out_root, &package, infix);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let doc = Document { targets };
        let yaml = serde_yaml_ng::to_string(&doc).context("serializing targets")?;
        std::fs::write(&path, yaml).with_context(|| format!("writing {}", path.display()))?;
        written.push(path);
    }

    prune(&written, infix, out_root)?;
    written.sort();
    Ok(written)
}

/// Delete `giant.<infix>.yaml` files under `out_root` that are not in `keep`,
/// so the on-disk set always matches a fresh generation (the `--check` gate).
fn prune(keep: &[PathBuf], infix: &str, out_root: &Path) -> Result<()> {
    let owned = format!("giant.{infix}.yaml");
    let keep: BTreeSet<&Path> = keep.iter().map(PathBuf::as_path).collect();
    for entry in ignore::Walk::new(out_root).flatten() {
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) == Some(&owned) && !keep.contains(path) {
            std::fs::remove_file(path).with_context(|| format!("pruning {}", path.display()))?;
        }
    }
    Ok(())
}
