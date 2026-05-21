//! Core types: `TargetId`, `ContentHash`, `CacheKey`, `TargetSpec`.
//!
//! See TDD-0001 for the schema, TDD-0009 for cache-key composition,
//! TDD-0002 for structural input semantics.

use crate::paths::{OutputPath, WsRelPath};
use crate::types::GlobPattern;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::path::Path;

/// Content-addressed hash (sha256, 32 bytes). 64 hex chars when stringified.
///
/// sha256 chosen for ecosystem alignment with bazel-remote, sccache, and
/// `sha256sum` (debuggability via shell). See ADR-0006.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    pub fn of_bytes(data: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        Self(Sha256::digest(data).into())
    }

    pub fn of_file(path: &Path) -> io::Result<Self> {
        use sha2::{Digest, Sha256};
        let mut file = File::open(path)?;
        let mut hasher = Sha256::new();
        io::copy(&mut file, &mut hasher)?;
        Ok(Self(hasher.finalize().into()))
    }

    pub fn to_hex(&self) -> String {
        const_hex::encode(self.0)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn from_raw(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Build a streaming hasher when you want to feed bytes incrementally.
    pub fn hasher() -> Hasher {
        use sha2::Digest;
        Hasher(sha2::Sha256::new())
    }
}

/// Streaming hasher wrapper. Use for incremental hashing
/// (`update(...)` then `finalize()`).
pub struct Hasher(sha2::Sha256);

impl Hasher {
    pub fn update(&mut self, bytes: &[u8]) {
        use sha2::Digest;
        self.0.update(bytes);
    }

    pub fn finalize(self) -> ContentHash {
        use sha2::Digest;
        ContentHash(self.0.finalize().into())
    }
}

/// Cache key - newtype over `ContentHash` for type-checked composition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CacheKey(ContentHash);

impl CacheKey {
    pub fn new(hash: ContentHash) -> Self {
        Self(hash)
    }

    pub fn to_hex(&self) -> String {
        self.0.to_hex()
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    pub fn as_content_hash(&self) -> ContentHash {
        self.0
    }
}

/// Unique identifier for a target.
///
/// Convention is `lang:kind:name` (e.g. `go:bin:server`) but the engine
/// treats this as opaque.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TargetId(String);

impl TargetId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TargetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for TargetId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for TargetId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TargetId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for TargetId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// A target - the unit of work. Schema in TDD-0001.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetSpec {
    pub id: TargetId,
    pub inputs: Vec<Input>,
    #[serde(default)]
    pub outputs: Vec<OutputPath>,
    #[serde(default)]
    pub deps: Vec<TargetId>,
    pub command: String,
    #[serde(default)]
    pub cwd: WsRelPath,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub cache: Option<bool>,
    #[serde(default = "default_true")]
    pub remote_cache: bool,
    #[serde(default)]
    pub exists: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub test: bool,
    #[serde(default)]
    pub tags: HashSet<String>,
    #[serde(default)]
    pub label: Option<String>,

    /// Runtime-only: the subset of `deps` populated by output-based
    /// inference. Display metadata for `giant explain`; never serialized.
    #[serde(skip)]
    pub inferred_deps: HashSet<TargetId>,
}

fn default_true() -> bool {
    true
}

/// One input declaration. Three forms per TDD-0001.
///
/// In YAML/JSON config, a bare string is sugar for `{kind: file, glob: "..."}`.
/// Deserialization handles both via the `try_from` attribute.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "InputRaw", into = "InputRaw")]
pub enum Input {
    File {
        glob: GlobPattern,
    },
    Structural {
        /// One or many globs; matches are unioned.
        files: Vec<GlobPattern>,
        /// Substring-prefix patterns. A line matches if it `starts_with` one.
        lines: Vec<String>,
        /// Optional scope for `git status` pathspecs. Auto-derived from
        /// `files` when absent (TDD-0002 §Scope auto-derivation).
        #[serde(default)]
        scope: Vec<WsRelPath>,
    },
}

/// Wire format for `Input` - accepts a bare string or a tagged object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum InputRaw {
    Bare(String),
    Tagged(InputTagged),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum InputTagged {
    File {
        glob: String,
    },
    Structural {
        files: GlobOrList,
        lines: Vec<String>,
        #[serde(default)]
        scope: Vec<WsRelPath>,
    },
}

/// One glob string or a list of glob strings, untagged so YAML/JSON can give
/// either form.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum GlobOrList {
    One(String),
    Many(Vec<String>),
}

impl GlobOrList {
    fn into_vec(self) -> Vec<String> {
        match self {
            GlobOrList::One(s) => vec![s],
            GlobOrList::Many(v) => v,
        }
    }
}

impl TryFrom<InputRaw> for Input {
    type Error = String;
    fn try_from(raw: InputRaw) -> Result<Self, Self::Error> {
        match raw {
            InputRaw::Bare(s) => GlobPattern::new(s)
                .map(|glob| Input::File { glob })
                .map_err(|e| format!("invalid glob: {e}")),
            InputRaw::Tagged(InputTagged::File { glob }) => GlobPattern::new(glob)
                .map(|g| Input::File { glob: g })
                .map_err(|e| format!("invalid glob: {e}")),
            InputRaw::Tagged(InputTagged::Structural {
                files,
                lines,
                scope,
            }) => {
                if lines.is_empty() {
                    return Err("structural input requires at least one line pattern".into());
                }
                let files: Result<Vec<GlobPattern>, _> = files
                    .into_vec()
                    .into_iter()
                    .map(|s| GlobPattern::new(s).map_err(|e| format!("invalid glob: {e}")))
                    .collect();
                Ok(Input::Structural {
                    files: files?,
                    lines,
                    scope,
                })
            }
        }
    }
}

impl From<Input> for InputRaw {
    fn from(i: Input) -> Self {
        match i {
            Input::File { glob } => InputRaw::Tagged(InputTagged::File {
                glob: glob.as_str().to_string(),
            }),
            Input::Structural {
                files,
                lines,
                scope,
            } => {
                let files = if files.len() == 1 {
                    GlobOrList::One(files.into_iter().next().unwrap().as_str().to_string())
                } else {
                    GlobOrList::Many(files.into_iter().map(|g| g.as_str().to_string()).collect())
                };
                InputRaw::Tagged(InputTagged::Structural {
                    files,
                    lines,
                    scope,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_deterministic() {
        let a = ContentHash::of_bytes(b"hello");
        let b = ContentHash::of_bytes(b"hello");
        assert_eq!(a, b);
    }

    #[test]
    fn content_hash_to_hex_64_chars() {
        let h = ContentHash::of_bytes(b"x");
        assert_eq!(h.to_hex().len(), 64);
    }

    #[test]
    fn target_id_borrow_as_str() {
        let id = TargetId::new("go:bin:server");
        let s: &str = id.as_ref();
        assert_eq!(s, "go:bin:server");
    }

    #[test]
    fn input_deserializes_from_bare_string() {
        let yaml = r#""src/**/*.go""#;
        let input: Input = serde_yaml_ng::from_str(yaml).unwrap();
        match input {
            Input::File { glob } => assert_eq!(glob.as_str(), "src/**/*.go"),
            _ => panic!("expected File"),
        }
    }

    #[test]
    fn input_deserializes_from_tagged_file() {
        let yaml = r#"{ kind: file, glob: "src/**/*.go" }"#;
        let input: Input = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(input, Input::File { .. }));
    }

    #[test]
    fn input_deserializes_structural_with_single_glob() {
        let yaml = r#"
            kind: structural
            files: "**/*.go"
            lines: ["^package ", "^import "]
        "#;
        let input: Input = serde_yaml_ng::from_str(yaml).unwrap();
        match input {
            Input::Structural {
                files,
                lines,
                scope,
            } => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].as_str(), "**/*.go");
                assert_eq!(lines, vec!["^package ", "^import "]);
                assert!(scope.is_empty());
            }
            _ => panic!("expected Structural"),
        }
    }

    #[test]
    fn input_deserializes_structural_with_multiple_globs() {
        let yaml = r#"
            kind: structural
            files: ["**/*.go", "**/*.s"]
            lines: ["^package "]
        "#;
        let input: Input = serde_yaml_ng::from_str(yaml).unwrap();
        match input {
            Input::Structural { files, .. } => assert_eq!(files.len(), 2),
            _ => panic!("expected Structural"),
        }
    }

    #[test]
    fn input_rejects_structural_without_lines() {
        let yaml = r#"
            kind: structural
            files: "**/*.go"
            lines: []
        "#;
        let result: Result<Input, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn input_rejects_unknown_kind() {
        let yaml = r#"{ kind: bogus, glob: "x" }"#;
        let result: Result<Input, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn input_rejects_unknown_field_via_deny_unknown_fields() {
        // The wire format uses #[serde(deny_unknown_fields)] on InputTagged.
        let yaml = r#"{ kind: file, glob: "x", unexpected: 1 }"#;
        let result: Result<Input, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err());
    }
}
