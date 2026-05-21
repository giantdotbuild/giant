//! Dynamic completion providers - invoked by the shell at TAB time
//! via `clap_complete::engine::ArgValueCompleter`.
//!
//! These have to be fast: shells block on them. We read `giant.yaml`
//! statically + replay any cached discovery output JSON sitting on
//! disk. We do NOT run discovery (that would push completion latency
//! into hundreds of ms). Result: targets are completed for everything
//! that existed as of the last `giant build` - which is what the
//! user's muscle memory expects anyway.
//!
//! Typical cost on a 200-target monorepo: ~5–20 ms.

use crate::config::Config;
use crate::discovery::DiscoveryFragment;
use clap_complete::CompletionCandidate;
use std::ffi::OsStr;
use std::path::Path;

/// All target ids known to the workspace: static `targets:` +
/// `include:` (the discovery targets themselves) + any discovered
/// targets sitting in cached discovery-output JSON files. The latter
/// covers everything emitted by previous `giant build` runs without
/// having to re-run discovery.
pub fn complete_target_ids(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some((cfg, workspace_root)) = load_nearby_config() else {
        return Vec::new();
    };
    let prefix = current.to_string_lossy();

    let mut ids: Vec<String> = cfg
        .targets
        .iter()
        .chain(cfg.include.iter())
        .map(|t| t.id.as_str().to_string())
        .collect();

    // Replay each include target's declared output JSON. These are
    // produced fresh on each `giant build`, so they reflect the
    // workspace's actual discovered-target set without us having to
    // re-run anything.
    for inc in &cfg.include {
        for output in &inc.outputs {
            let path = workspace_root.join(output.as_path());
            if let Some(frag) = read_discovery_fragment(&path) {
                for t in frag.targets {
                    ids.push(t.id.as_str().to_string());
                }
            }
        }
    }

    ids.sort();
    ids.dedup();
    ids.into_iter()
        .filter(|id| id.starts_with(prefix.as_ref()))
        .map(CompletionCandidate::new)
        .collect()
}

/// Walk up from cwd looking for giant.yaml / giant.json. Returns
/// `None` (and thus no candidates) on any error - completion failing
/// quietly is better than completion erroring at TAB time.
fn load_nearby_config() -> Option<(Config, std::path::PathBuf)> {
    let cwd = std::env::current_dir().ok()?;
    let mut here = Some(cwd.as_path());
    while let Some(dir) = here {
        for name in ["giant.yaml", "giant.yml", "giant.json"] {
            let candidate = dir.join(name);
            if candidate.is_file()
                && let Ok(cfg) = Config::load(&candidate)
            {
                return Some((cfg, dir.to_path_buf()));
            }
        }
        here = dir.parent();
    }
    None
}

/// Read + parse a discovery output file. Returns `None` on any error
/// (missing file, bad JSON, wrong schema) - completion gracefully
/// degrades.
fn read_discovery_fragment(path: &Path) -> Option<DiscoveryFragment> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_discovery_fragment_returns_targets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.json");
        std::fs::write(
            &path,
            r#"{
                "schema_version": 1,
                "targets": [
                    {
                        "id": "go:pkg:internal/auth",
                        "inputs": ["internal/auth/**/*.go"],
                        "outputs": ["bin/auth"],
                        "command": "go build -o bin/auth ./internal/auth"
                    },
                    {
                        "id": "go:pkg:internal/store",
                        "inputs": ["internal/store/**/*.go"],
                        "outputs": ["bin/store"],
                        "command": "go build -o bin/store ./internal/store"
                    }
                ]
            }"#,
        )
        .unwrap();
        let frag = read_discovery_fragment(&path).expect("fragment parses");
        let ids: Vec<&str> = frag.targets.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["go:pkg:internal/auth", "go:pkg:internal/store"]);
    }

    #[test]
    fn read_discovery_fragment_returns_none_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_discovery_fragment(&dir.path().join("missing.json")).is_none());
    }

    #[test]
    fn read_discovery_fragment_returns_none_on_bad_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(read_discovery_fragment(&path).is_none());
    }
}
