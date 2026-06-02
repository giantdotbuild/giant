//! Dynamic completion providers - invoked by the shell at TAB time
//! via `clap_complete::engine::ArgValueCompleter`.
//!
//! These have to be fast: shells block on them. We read `giant.yaml`
//! statically and complete the target ids it declares - no build, no
//! subprocess. Result: targets are completed for everything in the
//! checked-in config.

use clap_complete::CompletionCandidate;
use std::ffi::OsStr;

/// All target ids declared in the nearest workspace config. Any error -
/// no config up the tree, parse/validation failure - yields no
/// candidates, because completion failing quietly beats erroring at TAB
/// time.
pub fn complete_target_ids(current: &OsStr) -> Vec<CompletionCandidate> {
    let Ok((cfg, _root)) = super::prep::load_config(None) else {
        return Vec::new();
    };
    let prefix = current.to_string_lossy();

    let mut ids: Vec<String> = cfg
        .targets
        .iter()
        .map(|t| t.id.as_str().to_string())
        .collect();

    ids.sort();
    ids.dedup();
    ids.into_iter()
        .filter(|id| id.starts_with(prefix.as_ref()))
        .map(CompletionCandidate::new)
        .collect()
}
