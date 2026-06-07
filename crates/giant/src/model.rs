//! Core types: `TargetId`, `ContentHash`, `CacheKey`, `TargetSpec`.
//!
//! See TDD-0001 for the schema, TDD-0009 for cache-key composition,
//! ADR-0007 for the YAML-as-sugar input forms.

use crate::paths::{OutputPath, WsRelPath};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::path::Path;

/// The wire `Input` form lives in `giant-schema`; re-exported so existing
/// `crate::model::Input` paths keep resolving.
pub use giant_schema::Input;

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
///
/// The engine's *resolved* form. It is built from a
/// [`giant_schema::WireTarget`] on load (the `From` impl below; that is the
/// only way it is deserialized) and then finalized by the config loader, which
/// fills `id` and resolves the package-relative paths into `outputs`/`cwd`.
/// It is never serialized - the wire form is `WireTarget`.
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "giant_schema::WireTarget")]
pub struct TargetSpec {
    /// Local name, unique within the package.
    pub name: String,

    /// Path-derived label `//<package>:<name>`, filled by the loader. The
    /// engine's identity - graph keys, deps, selection all use it.
    pub id: TargetId,

    pub inputs: Vec<Input>,

    /// Package-relative output strings as written, awaiting resolution into
    /// `outputs`. Two-phase-resolution scratch, consumed by the loader.
    pub(crate) outputs_raw: Vec<String>,

    /// Workspace-relative outputs, resolved from `outputs_raw` by the loader.
    pub outputs: Vec<OutputPath>,

    pub deps: Vec<TargetId>,
    pub command: String,

    /// Raw `cwd` string awaiting resolution; `None` = the package directory.
    /// Two-phase-resolution scratch, consumed by the loader.
    pub(crate) cwd_raw: Option<String>,

    /// Workspace-relative working directory, resolved by the loader.
    pub cwd: WsRelPath,

    pub env: HashMap<String, String>,
    pub cache: Option<bool>,
    pub remote_cache: bool,

    /// Network allowed when sandboxed (ADR-0030 §4a). Inert unless `--sandbox`
    /// mode is on; never contributes to the cache key.
    pub network: bool,

    /// Sandbox eligibility (ADR-0030 §4a); `false` exempts the target from
    /// `--sandbox`. Never contributes to the cache key.
    pub sandbox: bool,

    pub exists: Option<String>,
    pub timeout_secs: Option<u64>,
    pub test: bool,
    pub tags: HashSet<String>,
    pub label: Option<String>,

    /// Runtime-only: workspace-relative directories of subpackages (nested
    /// `giant.yaml` files) that this target's globs must not cross into,
    /// so no two packages claim the same file (TDD-0001 §Path resolution).
    /// Computed by the loader from the full package set.
    pub prune_dirs: Vec<WsRelPath>,
}

impl From<giant_schema::WireTarget> for TargetSpec {
    /// Build the resolved spec from the wire form. Path resolution
    /// (`id`, `outputs`, `cwd`, and package-prefixed `inputs`/`deps`) is left
    /// to the loader, which runs once the declaring file's package is known.
    fn from(w: giant_schema::WireTarget) -> Self {
        Self {
            name: w.name,
            id: TargetId::default(),
            inputs: w.inputs,
            outputs_raw: w.outputs,
            outputs: Vec::new(),
            deps: w.deps.into_iter().map(TargetId::new).collect(),
            command: w.command,
            cwd_raw: w.cwd,
            cwd: WsRelPath::default(),
            env: w.env.into_iter().collect(),
            cache: w.cache,
            remote_cache: w.remote_cache,
            network: w.network,
            sandbox: w.sandbox,
            exists: w.exists,
            timeout_secs: w.timeout_secs,
            test: w.test,
            tags: w.tags.into_iter().collect(),
            label: w.label,
            prune_dirs: Vec::new(),
        }
    }
}

impl TargetSpec {
    /// Whether this target participates in the content-addressed cache.
    /// Build targets default to cached, `test:` targets to uncached; an
    /// explicit `cache:` overrides either way. See TDD-0009.
    pub fn is_cacheable(&self) -> bool {
        self.cache.unwrap_or(!self.test)
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
    fn target_spec_from_wire_resolves_later() {
        // The wire→spec conversion copies fields and leaves path resolution
        // (id, outputs, cwd) to the loader.
        let w: giant_schema::WireTarget =
            serde_yaml_ng::from_str("name: build\ncommand: \"go build\"\noutputs: [bin/x]\n")
                .unwrap();
        let spec = TargetSpec::from(w);
        assert_eq!(spec.name, "build");
        assert_eq!(spec.outputs_raw, vec!["bin/x".to_string()]);
        assert!(spec.outputs.is_empty(), "outputs resolved by the loader");
        assert!(spec.remote_cache);
    }
}
