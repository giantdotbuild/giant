//! Locate `giant.yaml` / `giant.json` by walking up from cwd.
//!
//! Same convention as core's `cli::prep::find_config`, deliberately
//! re-implemented here so the porcelain doesn't reach into core's
//! private modules. ~30 LOC.

use std::path::{Path, PathBuf};

const CANDIDATES: &[&str] = &["giant.yaml", "giant.yml", "giant.json"];

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("couldn't find giant.yaml - searched up from {start} to the filesystem root")]
    NotFound { start: String },
}

pub fn find_config(start: &Path) -> Result<PathBuf, WorkspaceError> {
    let mut here = Some(start);
    while let Some(dir) = here {
        for name in CANDIDATES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        here = dir.parent();
    }
    Err(WorkspaceError::NotFound {
        start: start.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_in_current_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("giant.yaml"), "workspace: { name: p }\n").unwrap();
        let found = find_config(dir.path()).unwrap();
        assert_eq!(found, dir.path().join("giant.yaml"));
    }

    #[test]
    fn walks_up_through_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("giant.yaml"), "workspace: { name: p }\n").unwrap();
        let found = find_config(&nested).unwrap();
        assert_eq!(found, dir.path().join("giant.yaml"));
    }

    #[test]
    fn prefers_yaml_over_json_in_same_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("giant.yaml"), "workspace: { name: p }\n").unwrap();
        std::fs::write(dir.path().join("giant.json"), "{}").unwrap();
        let found = find_config(dir.path()).unwrap();
        assert!(found.ends_with("giant.yaml"));
    }

    #[test]
    fn not_found_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let err = find_config(dir.path()).unwrap_err();
        assert!(matches!(err, WorkspaceError::NotFound { .. }));
    }
}
