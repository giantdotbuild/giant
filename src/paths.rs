//! Path newtypes for type-safety in handling absolute / workspace-relative /
//! cwd-relative paths.
//!
//! Path-format mixups are a recurring source of bugs. Each variant carries
//! its expected shape in the type.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// An absolute filesystem path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AbsPath(PathBuf);

impl AbsPath {
    /// Wrap a path, panicking if it isn't absolute.
    pub fn new(p: impl Into<PathBuf>) -> Self {
        let p = p.into();
        assert!(p.is_absolute(), "AbsPath requires an absolute path: {}", p.display());
        Self(p)
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }

    /// Join a workspace-relative path onto this absolute base.
    pub fn join_ws(&self, rel: &WsRelPath) -> Self {
        Self(self.0.join(rel.as_path()))
    }
}

/// A workspace-relative path: must not be absolute, must not contain `..`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WsRelPath(PathBuf);

impl WsRelPath {
    pub fn new(p: impl Into<PathBuf>) -> Result<Self, PathError> {
        let p = p.into();
        if p.is_absolute() {
            return Err(PathError::Absolute(p));
        }
        if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            return Err(PathError::ParentRef(p));
        }
        Ok(Self(p))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

/// A target output path: workspace-relative, validated like `WsRelPath`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutputPath(WsRelPath);

impl OutputPath {
    pub fn new(p: impl Into<PathBuf>) -> Result<Self, PathError> {
        WsRelPath::new(p).map(Self)
    }

    pub fn as_path(&self) -> &Path {
        self.0.as_path()
    }

    pub fn as_ws_rel(&self) -> &WsRelPath {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("path must be relative, got {0:?}")]
    Absolute(PathBuf),

    #[error("path must not contain `..`, got {0:?}")]
    ParentRef(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_rel_rejects_absolute() {
        assert!(WsRelPath::new("/etc/passwd").is_err());
    }

    #[test]
    fn ws_rel_rejects_parent_ref() {
        assert!(WsRelPath::new("../foo").is_err());
    }

    #[test]
    fn ws_rel_accepts_simple() {
        assert!(WsRelPath::new("src/main.rs").is_ok());
    }

    #[test]
    #[should_panic(expected = "AbsPath requires")]
    fn abs_path_rejects_relative() {
        AbsPath::new("foo");
    }
}
