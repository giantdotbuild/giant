//! The `giant.yaml` wire schema - the typed config contract (ADR-0007's "JSON
//! is the contract", as a first-class type rather than serde annotations on a
//! hybrid struct).
//!
//! This crate is the single definition of the serialized target shape, shared
//! one-way by both the engine (which deserializes it and resolves it into its
//! internal `TargetSpec`) and generators (which construct and serialize it).
//! Keeping it here means producer and consumer cannot drift: a bad field is a
//! compile error against `WireTarget`, not a runtime surprise at load. The
//! dependency direction is `giant-schema <- engine` and `giant-schema <-
//! generator host`; this crate depends on neither (ADR-0029 §5).
//!
//! See TDD-0001 for the schema and ADR-0007 for the YAML-as-sugar input forms.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// A generator's output document, or any package config's `targets:` block:
/// `{ targets: [ ... ] }`. The unit a generator emits and the engine scans.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Document {
    #[serde(default)]
    pub targets: Vec<WireTarget>,
}

/// A target exactly as it appears on the wire (YAML/JSON). Every field is
/// public and serde-visible; there are no resolved or runtime fields here.
/// The engine's loader deserializes this and builds the internal resolved
/// `TargetSpec` from it; a generator builds this and serializes it.
///
/// Schema in TDD-0001. Paths (`outputs`, `cwd`, input globs) are written
/// package-relative or `//`-rooted; the engine resolves them against the
/// declaring file's package on load.
/// `env` and `tags` are sorted maps/sets, and empty/default fields are omitted
/// on serialize, so a generator's emitted YAML is deterministic and clean -
/// the property `giant gen --check` relies on (TDD-0024 §F).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireTarget {
    /// Local name, unique within the package.
    pub name: String,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<Input>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<String>,

    pub command: String,

    /// Working directory; `None` defaults to the package directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<bool>,

    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub remote_cache: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exists: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,

    #[serde(default, skip_serializing_if = "is_false")]
    pub test: bool,

    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tags: BTreeSet<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn default_true() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde skip_serializing_if needs &T
fn is_true(b: &bool) -> bool {
    *b
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
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
        let glob = match raw {
            InputRaw::Bare(s) => s,
            InputRaw::Tagged(InputTagged::File { glob }) => glob,
        };
        GlobPattern::new(glob)
            .map(|glob| Input::File { glob })
            .map_err(|e| format!("invalid glob: {e}"))
    }
}

impl From<Input> for InputRaw {
    fn from(i: Input) -> Self {
        // Serialize a file input as the bare-string sugar (ADR-0007), so
        // generated configs read cleanly (`- "src/*.go"`, not a tagged map).
        // The tagged form still deserializes, so the round-trip is preserved.
        match i {
            Input::File { glob } => InputRaw::Bare(glob.as_str().to_string()),
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn input_serializes_as_bare_string() {
        let input = Input::File {
            glob: GlobPattern::new("src/**/*.go").unwrap(),
        };
        let yaml = serde_yaml_ng::to_string(&input).unwrap();
        assert_eq!(yaml.trim(), "src/**/*.go");
    }

    #[test]
    fn input_rejects_unknown_kind() {
        let yaml = r#"{ kind: bogus, glob: "x" }"#;
        let result: Result<Input, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn input_rejects_unknown_field_via_deny_unknown_fields() {
        let yaml = r#"{ kind: file, glob: "x", unexpected: 1 }"#;
        let result: Result<Input, _> = serde_yaml_ng::from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn wire_target_defaults() {
        let t: WireTarget = serde_yaml_ng::from_str("name: x\ncommand: \"true\"\n").unwrap();
        assert_eq!(t.name, "x");
        assert!(t.outputs.is_empty());
        assert!(t.deps.is_empty());
        assert!(t.remote_cache, "remote_cache defaults to true");
        assert!(!t.test);
        assert!(t.cache.is_none());
    }

    #[test]
    fn wire_target_round_trip() {
        let yaml = "name: build\ncommand: go build\noutputs: [bin/x]\ndeps: [\"//:dep\"]\n";
        let t: WireTarget = serde_yaml_ng::from_str(yaml).unwrap();
        let back = serde_yaml_ng::to_string(&t).unwrap();
        let again: WireTarget = serde_yaml_ng::from_str(&back).unwrap();
        assert_eq!(again.outputs, vec!["bin/x".to_string()]);
        assert_eq!(again.deps, vec!["//:dep".to_string()]);
    }
}
