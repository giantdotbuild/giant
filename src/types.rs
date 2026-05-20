//! Small shared types.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A glob pattern, validated on parse.
///
/// Stored as the original string; compiled on demand. Validation happens
/// at config load (TDD-0001 §Validation).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GlobPattern(String);

impl GlobPattern {
    pub fn new(s: impl Into<String>) -> Result<Self, glob::PatternError> {
        let s = s.into();
        glob::Pattern::new(&s)?;
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GlobPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
