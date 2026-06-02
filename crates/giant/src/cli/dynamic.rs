//! Dynamic completion providers - invoked by the shell at TAB time
//! via `clap_complete::engine::ArgValueCompleter`.
//!
//! These have to be fast: shells block on them. We read only the
//! workspace-root `giant.yaml` and complete the root package's target
//! labels - no build, no subprocess, and no full-tree scan (that would
//! parse every package config on each keystroke). Completing labels from
//! nested packages is left to a future cached target-id list.

use clap_complete::CompletionCandidate;
use std::ffi::OsStr;

/// Root-package target labels. Any error - no workspace config up the
/// tree, parse/validation failure - yields no candidates, because
/// completion failing quietly beats erroring at TAB time.
pub fn complete_target_ids(current: &OsStr) -> Vec<CompletionCandidate> {
    let Ok((cfg, _root)) = crate::config::Config::load_root(None) else {
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
