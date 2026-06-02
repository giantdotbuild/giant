//! Core types: `TargetId`, `ContentHash`, `CacheKey`, `TargetSpec`.
//!
//! See TDD-0001 for the schema, TDD-0009 for cache-key composition,
//! ADR-0007 for the YAML-as-sugar input forms.

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

    /// Parse a 64-char hex string. `None` if it isn't valid 32-byte hex -
    /// used when reading hashes back out of cache sidecars / AC entries.
    pub fn from_hex(s: &str) -> Option<Self> {
        let bytes = const_hex::decode(s).ok()?;
        let arr: [u8; 32] = bytes.as_slice().try_into().ok()?;
        Some(Self(arr))
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

/// Path-derived target label `//<package>:<name>` (TDD-0001, ADR-0024).
///
/// The package is the workspace-relative directory of the target's
/// `giant.yaml`; the root package is empty, so a root target is
/// `//:name`. The engine treats the whole string as opaque past
/// construction.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TargetId(String);

impl TargetId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Build the label for `name` in `package` (a workspace-relative dir,
    /// `""` for the root package): `//<package>:<name>`.
    pub fn label(package: &str, name: &str) -> Self {
        Self(format!("//{package}:{name}"))
    }

    /// Split a `//<package>:<name>` label into its package (may be empty
    /// or contain `/`) and name parts. The inverse of `label`.
    pub fn split(&self) -> (&str, &str) {
        let body = self.0.strip_prefix("//").unwrap_or(&self.0);
        body.rsplit_once(':').unwrap_or((body, ""))
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
    /// Local name, unique within the package. The wire field a config
    /// declares; the engine's identity is the derived `id` label below.
    pub name: String,

    /// Path-derived label `//<package>:<name>`. Computed by the loader
    /// from the file's package and `name`; never serialized. This is the
    /// engine's identity - graph keys, deps, selection all use it.
    #[serde(skip)]
    pub id: TargetId,

    #[serde(default)]
    pub inputs: Vec<Input>,

    /// Raw `outputs:` strings as written - package-relative or `//`-rooted.
    /// The loader resolves these into `outputs`; this is the wire form.
    #[serde(rename = "outputs", default)]
    pub(crate) outputs_raw: Vec<String>,

    /// Workspace-relative outputs, resolved from `outputs_raw` by the
    /// loader. Never deserialized; the rest of the engine reads this.
    #[serde(skip)]
    pub outputs: Vec<OutputPath>,

    #[serde(default)]
    pub deps: Vec<TargetId>,
    pub command: String,

    /// Raw `cwd:` string (package-relative or `//`-rooted); `None` = the
    /// default (the package directory). The wire form.
    #[serde(rename = "cwd", default)]
    pub(crate) cwd_raw: Option<String>,

    /// Workspace-relative working directory, resolved by the loader.
    #[serde(skip)]
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

    /// Runtime-only: workspace-relative directories of subpackages (nested
    /// `giant.yaml` files) that this target's globs must not cross into,
    /// so no two packages claim the same file (TDD-0001 §Path resolution).
    /// Computed by the loader from the full package set; never serialized.
    #[serde(skip)]
    pub prune_dirs: Vec<WsRelPath>,
}

fn default_true() -> bool {
    true
}

impl TargetSpec {
    /// Whether this target participates in the content-addressed cache.
    /// Build targets default to cached, `test:` targets to uncached; an
    /// explicit `cache:` overrides either way. See TDD-0009.
    pub fn is_cacheable(&self) -> bool {
        self.cache.unwrap_or(!self.test)
    }
}

/// One input declaration. Three forms per TDD-0001.
///
/// In YAML/JSON config, a bare string is sugar for `{kind: file, glob: "..."}`.
/// Deserialization handles both via the `try_from` attribute.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "InputRaw", into = "InputRaw")]
pub enum Input {
    File { glob: GlobPattern },
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
    File { glob: String },
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
        }
    }
}

impl From<Input> for InputRaw {
    fn from(i: Input) -> Self {
        match i {
            Input::File { glob } => InputRaw::Tagged(InputTagged::File {
                glob: glob.as_str().to_string(),
            }),
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
        let Input::File { glob } = input;
        assert_eq!(glob.as_str(), "src/**/*.go");
    }

    #[test]
    fn input_deserializes_from_tagged_file() {
        let yaml = r#"{ kind: file, glob: "src/**/*.go" }"#;
        let input: Input = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(input, Input::File { .. }));
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
