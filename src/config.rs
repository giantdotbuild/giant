//! Config loading and validation.
//!
//! See TDD-0001 for the schema, ADR-0007 for YAML-as-sugar policy.

use crate::model::TargetSpec;
use crate::paths::WsRelPath;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse YAML: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),

    #[error("failed to parse JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("config not found")]
    NotFound,

    #[error("validation: {0}")]
    Validation(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,

    pub workspace: WorkspaceConfig,

    #[serde(default)]
    pub include: Vec<TargetSpec>,

    #[serde(default)]
    pub targets: Vec<TargetSpec>,

    #[serde(default)]
    pub tasks: IndexMap<String, TaskSpec>,

    #[serde(default)]
    pub cache: CacheConfig,

    #[cfg(feature = "remote")]
    #[serde(default)]
    pub remote: RemoteConfig,
}

fn default_schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    #[serde(default = "default_cache_dir")]
    pub dir: String,

    #[serde(default = "default_max_size_gb")]
    pub max_size_gb: Option<u64>,

    #[serde(default = "default_evict_above_pct")]
    pub evict_when_above_pct: u32,

    #[serde(default = "default_evict_target_pct")]
    pub evict_target_pct: u32,

    #[serde(default)]
    pub max_age_days: Option<u32>,
}

fn default_cache_dir() -> String {
    "~/.cache/giant".to_string()
}

fn default_max_size_gb() -> Option<u64> {
    Some(20)
}

fn default_evict_above_pct() -> u32 {
    100
}

fn default_evict_target_pct() -> u32 {
    80
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            dir: default_cache_dir(),
            max_size_gb: default_max_size_gb(),
            evict_when_above_pct: default_evict_above_pct(),
            evict_target_pct: default_evict_target_pct(),
            max_age_days: None,
        }
    }
}

#[cfg(feature = "remote")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfig {
    #[serde(default)]
    pub enabled: bool,
    pub url: Option<String>,
    #[serde(default)]
    pub auth: RemoteAuth,
    #[serde(default)]
    pub skip_head: bool,
    #[serde(default = "default_max_blob_size_mb")]
    pub max_blob_size_mb: u64,
    #[serde(default)]
    pub tls: TlsConfig,
}

#[cfg(feature = "remote")]
fn default_max_blob_size_mb() -> u64 {
    500
}

#[cfg(feature = "remote")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteAuth {
    #[default]
    None,
    Bearer { token_env: String },
    Basic {
        username_env: String,
        password_env: String,
    },
}

#[cfg(feature = "remote")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    #[serde(default)]
    pub skip_verify: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub command: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub args: IndexMap<String, TaskArg>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub cwd: Option<WsRelPath>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskArg {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub choices: Option<Vec<String>>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Task names that shadow built-in subcommands. Validated at load.
const RESERVED_TASK_NAMES: &[&str] = &[
    "build",
    "affected",
    "graph",
    "clean",
    "explain",
    "serve",
    "verify",
    "watch",
    "test",
    "help",
];

impl Config {
    /// Load a config from a file. Detects YAML vs JSON by extension.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path)?;
        let cfg: Config = match path.extension().and_then(|e| e.to_str()) {
            Some("json") => serde_json::from_str(&raw)?,
            _ => serde_yaml_ng::from_str(&raw)?,
        };
        cfg.validate_static()?;
        Ok(cfg)
    }

    /// Static validation: things checkable without the merged graph.
    /// (TDD-0001 §Validation; merged validation runs after discovery per TDD-0003.)
    pub fn validate_static(&self) -> Result<(), ConfigError> {
        // workspace.name
        if self.workspace.name.is_empty() {
            return Err(ConfigError::Validation(
                "workspace.name is required".into(),
            ));
        }
        if !is_valid_workspace_name(&self.workspace.name) {
            return Err(ConfigError::Validation(format!(
                "workspace.name '{}' contains invalid characters (use alphanumeric, '-', '_')",
                self.workspace.name
            )));
        }

        // schema_version
        if self.schema_version != 1 {
            return Err(ConfigError::Validation(format!(
                "schema_version {} not supported (only v1 in this build)",
                self.schema_version
            )));
        }

        // target ID uniqueness across `include` and `targets`
        let mut seen = HashSet::new();
        for t in self.include.iter().chain(self.targets.iter()) {
            if !seen.insert(t.id.clone()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate target id '{}'",
                    t.id
                )));
            }
            if t.command.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "target '{}' has empty command",
                    t.id
                )));
            }
            // Cacheable target with no outputs and no exists check is meaningless.
            if t.cache.unwrap_or(!t.test) && t.outputs.is_empty() && t.exists.is_none() {
                return Err(ConfigError::Validation(format!(
                    "target '{}' is cacheable but has no outputs and no `exists:` check",
                    t.id
                )));
            }
        }

        // tasks: reserved names + non-empty commands
        for name in self.tasks.keys() {
            if RESERVED_TASK_NAMES.contains(&name.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "task name '{name}' shadows a reserved subcommand"
                )));
            }
            if name.is_empty() || !is_valid_task_name(name) {
                return Err(ConfigError::Validation(format!(
                    "task name '{name}' is invalid (alphanumeric, '-', '_'; no leading digit)"
                )));
            }
        }
        for (name, task) in &self.tasks {
            if task.command.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "task '{name}' has empty command"
                )));
            }
            // arg defaults must be in choices if choices are declared
            for (arg_name, arg) in &task.args {
                if let (Some(default), Some(choices)) = (&arg.default, &arg.choices)
                    && !choices.contains(default)
                {
                    return Err(ConfigError::Validation(format!(
                        "task '{name}' arg '{arg_name}' default '{default}' not in choices {choices:?}"
                    )));
                }
                if let Some(choices) = &arg.choices
                    && choices.is_empty()
                {
                    return Err(ConfigError::Validation(format!(
                        "task '{name}' arg '{arg_name}' has empty choices list"
                    )));
                }
            }
        }

        Ok(())
    }
}

fn is_valid_workspace_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_valid_task_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_yaml(s: &str) -> NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
        f.write_all(s.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn load_minimal_config() {
        let f = write_yaml(
            r#"
workspace:
  name: myproject
"#,
        );
        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.workspace.name, "myproject");
        assert_eq!(cfg.schema_version, 1);
        assert!(cfg.targets.is_empty());
    }

    #[test]
    fn load_with_static_target() {
        let f = write_yaml(
            r#"
workspace:
  name: myproject
targets:
  - id: "rust:build"
    inputs: ["src/**/*.rs", "Cargo.toml"]
    outputs: ["bin/app"]
    command: "cargo build --release"
"#,
        );
        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.targets.len(), 1);
        assert_eq!(cfg.targets[0].id.as_str(), "rust:build");
        assert_eq!(cfg.targets[0].inputs.len(), 2);
    }

    #[test]
    fn load_with_structural_input() {
        let f = write_yaml(
            r#"
workspace:
  name: myproject
targets:
  - id: "discover:go"
    inputs:
      - "go.mod"
      - kind: structural
        files: "**/*.go"
        lines: ["^package ", "^import "]
    outputs: [".giant/d/go.json"]
    command: "tools/discover-go.sh"
"#,
        );
        let cfg = Config::load(f.path()).unwrap();
        let t = &cfg.targets[0];
        assert_eq!(t.inputs.len(), 2);
    }

    #[test]
    fn reject_missing_workspace_name() {
        let f = write_yaml(
            r#"
workspace: { name: "" }
"#,
        );
        let err = Config::load(f.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }

    #[test]
    fn reject_invalid_workspace_name() {
        let f = write_yaml(
            r#"
workspace: { name: "has spaces" }
"#,
        );
        let err = Config::load(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid characters"), "got: {msg}");
    }

    #[test]
    fn reject_duplicate_target_id() {
        let f = write_yaml(
            r#"
workspace: { name: p }
targets:
  - { id: "a", inputs: [], outputs: ["x"], command: "true" }
  - { id: "a", inputs: [], outputs: ["y"], command: "true" }
"#,
        );
        let err = Config::load(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("duplicate target id"), "got: {msg}");
    }

    #[test]
    fn reject_cacheable_without_outputs_or_exists() {
        let f = write_yaml(
            r#"
workspace: { name: p }
targets:
  - { id: "a", inputs: [], outputs: [], command: "true" }
"#,
        );
        let err = Config::load(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("cacheable but has no outputs"),
            "got: {msg}"
        );
    }

    #[test]
    fn accept_no_outputs_with_exists_check() {
        let f = write_yaml(
            r#"
workspace: { name: p }
targets:
  - id: "docker:img"
    inputs: ["Dockerfile"]
    outputs: []
    command: "docker push"
    exists: "docker manifest inspect"
"#,
        );
        Config::load(f.path()).unwrap();
    }

    #[test]
    fn accept_no_outputs_when_cache_disabled() {
        let f = write_yaml(
            r#"
workspace: { name: p }
targets:
  - id: "lint"
    inputs: ["src/**/*.rs"]
    outputs: []
    cache: false
    command: "cargo clippy"
"#,
        );
        Config::load(f.path()).unwrap();
    }

    #[test]
    fn test_targets_default_uncached() {
        // test: true means cache defaults to false → empty outputs is OK
        let f = write_yaml(
            r#"
workspace: { name: p }
targets:
  - id: "rust:test"
    inputs: ["src/**/*.rs"]
    outputs: []
    test: true
    command: "cargo test"
"#,
        );
        Config::load(f.path()).unwrap();
    }

    #[test]
    fn reject_reserved_task_name() {
        let f = write_yaml(
            r#"
workspace: { name: p }
tasks:
  build:
    command: "echo hi"
"#,
        );
        let err = Config::load(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("reserved subcommand"), "got: {msg}");
    }

    #[test]
    fn reject_task_with_invalid_name() {
        let f = write_yaml(
            r#"
workspace: { name: p }
tasks:
  "1bad":
    command: "echo hi"
"#,
        );
        let err = Config::load(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid"), "got: {msg}");
    }

    #[test]
    fn reject_task_default_not_in_choices() {
        let f = write_yaml(
            r#"
workspace: { name: p }
tasks:
  deploy:
    command: "kubectl"
    args:
      env:
        default: "prod"
        choices: ["staging"]
"#,
        );
        let err = Config::load(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not in choices"), "got: {msg}");
    }

    #[test]
    fn reject_unknown_top_level_field() {
        let f = write_yaml(
            r#"
workspace: { name: p }
typo_here: "no"
"#,
        );
        let err = Config::load(f.path()).unwrap_err();
        // deny_unknown_fields on Config catches this
        assert!(matches!(err, ConfigError::Yaml(_)));
    }

    #[test]
    fn reject_unsupported_schema_version() {
        let f = write_yaml(
            r#"
schema_version: 999
workspace: { name: p }
"#,
        );
        let err = Config::load(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("schema_version 999"), "got: {msg}");
    }
}
