//! Config loading and validation.
//!
//! See TDD-0001 for the schema, ADR-0007 for YAML-as-sugar policy.

use crate::model::TargetSpec;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

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

/// Top-level config. NOT `deny_unknown_fields` - porcelains (giant-task,
/// future giant-deploy, etc.) own their own top-level sections like
/// `tasks:`. Core silently accepts them; the porcelain re-parses the
/// same file with its own schema. (ADR-0010.)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,

    pub workspace: WorkspaceConfig,

    #[serde(default)]
    pub include: Vec<TargetSpec>,

    #[serde(default)]
    pub targets: Vec<TargetSpec>,

    #[serde(default)]
    pub cache: CacheConfig,

    #[serde(default)]
    pub state: StateConfig,

    #[serde(default)]
    pub discovery: DiscoveryConfig,

    /// How `giant <name>` routes when `<name>` is neither a built-in nor
    /// a `giant-<name>` binary on PATH. See ADR-0021. Core stays
    /// decoupled from giant-task: it reads this table and execs whatever
    /// binary it names. Defaults to `* -> giant-task`.
    #[serde(default)]
    pub dispatch: DispatchConfig,

    #[cfg(feature = "remote")]
    #[serde(default)]
    pub remote: RemoteConfig,
}

/// Routing for unknown subcommands (ADR-0021). `unknown` is either a
/// single catch-all binary or an ordered list of `match -> binary` rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchConfig {
    #[serde(default = "default_unknown_route")]
    pub unknown: UnknownRoute,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            unknown: default_unknown_route(),
        }
    }
}

/// The `dispatch.unknown` value: a bare binary name (sugar for a single
/// `* -> name` rule) or an ordered, first-match-wins rule list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UnknownRoute {
    One(String),
    Rules(Vec<Route>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    /// Glob matched against the subcommand name.
    #[serde(rename = "match")]
    pub pattern: String,
    /// Binary to exec (resolved on PATH), invoked as `<to> <name> <args>`.
    pub to: String,
}

fn default_unknown_route() -> UnknownRoute {
    UnknownRoute::One("giant-task".into())
}

impl DispatchConfig {
    /// The binary that handles `name`: the `to` of the first rule whose
    /// glob matches. The string form is one `* -> binary` rule, so it
    /// always matches. A rule list returns `None` when no rule matches -
    /// the caller emits the "no such subcommand" error.
    pub fn route(&self, name: &str) -> Option<&str> {
        match &self.unknown {
            UnknownRoute::One(bin) => Some(bin.as_str()),
            UnknownRoute::Rules(rules) => rules
                .iter()
                .find(|r| {
                    glob::Pattern::new(&r.pattern)
                        .map(|p| p.matches(name))
                        .unwrap_or(false)
                })
                .map(|r| r.to.as_str()),
        }
    }
}

/// Load just the `dispatch:` table from the nearest config, tolerant of
/// everything else (ADR-0021: degrade, don't fail). Walks up from
/// `start_dir` for `giant.yaml`/`giant.yml`/`giant.json`, honoring the
/// `GIANT_CONFIG` env var first. Any failure - no config, parse error -
/// yields the default table (`* -> giant-task`), because the user is
/// running a subcommand and should still reach the catch-all.
pub fn load_dispatch(start_dir: &Path) -> DispatchConfig {
    // Only `dispatch:` is read; every other top-level field is ignored.
    #[derive(Deserialize, Default)]
    struct DispatchOnly {
        #[serde(default)]
        dispatch: DispatchConfig,
    }

    let path = std::env::var_os("GIANT_CONFIG")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| find_config_upward(start_dir));
    let Some(path) = path else {
        return DispatchConfig::default();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return DispatchConfig::default();
    };
    let parsed: Option<DispatchOnly> = match path.extension().and_then(|e| e.to_str()) {
        Some("json") => serde_json::from_str(&raw).ok(),
        _ => serde_yaml_ng::from_str(&raw).ok(),
    };
    parsed.map(|d| d.dispatch).unwrap_or_default()
}

/// Walk up from `start_dir` looking for a config file. Mirrors the
/// finder the build path uses.
fn find_config_upward(start_dir: &Path) -> Option<PathBuf> {
    let mut dir = Some(start_dir);
    while let Some(d) = dir {
        for name in ["giant.yaml", "giant.yml", "giant.json"] {
            let candidate = d.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        dir = d.parent();
    }
    None
}

/// Workspace-local engine state. Distinct from `cache.dir` (which can
/// be shared / remote) - state is the small set of files giant writes
/// next to the workspace so concurrent runs and other tools can see
/// them: discovery sidecars, the fsmonitor token, build logs.
///
/// Defaults to `.giant/` at the workspace root. Override when sharing
/// a workspace with another tool that already owns `.giant/`, or when
/// state files want a non-gitignored location.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConfig {
    #[serde(default = "default_state_dir")]
    pub dir: String,
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            dir: default_state_dir(),
        }
    }
}

fn default_state_dir() -> String {
    ".giant".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryConfig {
    /// When `true`, a discovery whose output lacks a `reads` manifest
    /// is a hard error (the cooperative protocol is enforced -
    /// ADR-0017). Default `false`: missing `reads` only produces a
    /// warning, and the discovery's output is used once but not
    /// cached.
    #[serde(default)]
    pub strict: bool,
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

    /// Capture each target's stdout + stderr to CAS blobs alongside
    /// outputs, so a future cache hit can replay them. Default true.
    /// Disable to lean out cache size at the cost of silent cache
    /// hits.
    #[serde(default = "default_capture_logs")]
    pub capture_logs: bool,

    /// Replay captured logs on cache hits (local or remote). Default
    /// true. Disable for CI runs that just want the result, not the
    /// scroll-back from a previous build.
    #[serde(default = "default_replay_logs")]
    pub replay_logs: bool,

    /// Per-stream cap on captured bytes (stdout and stderr each get
    /// this much). Lines beyond the cap are still streamed live but
    /// not written to CAS. Default 5 MiB.
    #[serde(default = "default_log_capture_cap")]
    pub log_capture_cap_bytes: usize,
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

fn default_capture_logs() -> bool {
    true
}

fn default_replay_logs() -> bool {
    true
}

fn default_log_capture_cap() -> usize {
    5 * 1024 * 1024
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            dir: default_cache_dir(),
            max_size_gb: default_max_size_gb(),
            evict_when_above_pct: default_evict_above_pct(),
            evict_target_pct: default_evict_target_pct(),
            max_age_days: None,
            capture_logs: default_capture_logs(),
            replay_logs: default_replay_logs(),
            log_capture_cap_bytes: default_log_capture_cap(),
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
    Bearer {
        token_env: String,
    },
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
            return Err(ConfigError::Validation("workspace.name is required".into()));
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

        // Discovery entries traditionally relied on the cooperative
        // `reads` manifest. Per ADR-0017 they may *also* declare
        // `inputs:` - explicit content-hashed files that feed the
        // cache key and are policed by the warm-path verifier even
        // when the discovery script doesn't report them. The two
        // mechanisms compose; no validation needed here.

        // `scope:` is discovery-only; reject on regular targets so a typo
        // isn't silently ignored.
        for t in &self.targets {
            if !t.scope.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "target '{}' declares `scope:`; that field is only \
                     valid on `include:` (discovery) entries.",
                    t.id
                )));
            }
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
            if t.is_cacheable() && t.outputs.is_empty() && t.exists.is_none() {
                return Err(ConfigError::Validation(format!(
                    "target '{}' is cacheable but has no outputs and no `exists:` check",
                    t.id
                )));
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
        assert!(msg.contains("cacheable but has no outputs"), "got: {msg}");
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
    fn accept_unknown_top_level_field() {
        // Porcelains (giant-task, future giant-deploy, etc.) own their
        // own top-level sections. Core silently accepts unknown fields
        // so they don't have to coordinate with us to ship.
        let f = write_yaml(
            r#"
workspace: { name: p }
tasks:
  deploy:
    command: "kubectl apply -f k8s/"
giant_deploy_settings:
  whatever: true
"#,
        );
        Config::load(f.path()).expect("unknown top-level keys must parse");
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

    /// Discovery entries may declare `inputs:` - these flow into the
    /// discovery cache key (content-hashed) alongside the argv-walk
    /// detection. The opposite of the originally-proposed behavior,
    /// which rejected the field outright (ADR-0013, superseded by
    /// ADR-0017).
    #[test]
    fn accept_inputs_on_include_entry() {
        let f = write_yaml(
            r#"
workspace: { name: p }
include:
  - id: "discover:go"
    inputs: ["tools/lib/**/*.sh"]
    outputs: [".giant/d/go.json"]
    command: "tools/discover-go.sh"
"#,
        );
        let cfg = Config::load(f.path()).expect("inputs on discovery should load");
        assert_eq!(cfg.include.len(), 1);
        assert_eq!(cfg.include[0].id.as_str(), "discover:go");
    }

    #[test]
    fn reject_scope_on_regular_target() {
        let f = write_yaml(
            r#"
workspace: { name: p }
targets:
  - id: "rust:build"
    inputs: ["src/**/*.rs"]
    outputs: ["bin/app"]
    command: "cargo build"
    scope: ["src/"]
"#,
        );
        let err = Config::load(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("rust:build") && msg.contains("scope"),
            "expected error naming the target and scope, got: {msg}"
        );
    }

    #[test]
    fn accept_include_with_scope_and_no_inputs() {
        let f = write_yaml(
            r#"
workspace: { name: p }
include:
  - id: "discover:go"
    outputs: [".giant/d/go.json"]
    command: "tools/discover-go.sh > .giant/d/go.json"
    scope: ["pkg/", "cmd/"]
"#,
        );
        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.include.len(), 1);
        assert_eq!(cfg.include[0].scope.len(), 2);
        assert!(cfg.include[0].inputs.is_empty());
    }

    #[test]
    fn accept_include_without_scope() {
        let f = write_yaml(
            r#"
workspace: { name: p }
include:
  - id: "discover:protos"
    outputs: [".giant/d/protos.json"]
    command: "tools/discover-protos.sh > .giant/d/protos.json"
"#,
        );
        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.include.len(), 1);
        assert!(cfg.include[0].scope.is_empty());
    }

    // --- dispatch routing (ADR-0021) ---

    #[test]
    fn dispatch_default_routes_to_giant_task() {
        let d = DispatchConfig::default();
        assert_eq!(d.route("deploy"), Some("giant-task"));
        assert_eq!(d.route("anything-at-all"), Some("giant-task"));
    }

    #[test]
    fn dispatch_string_form_routes_everything_there() {
        let cfg: Config = serde_yaml_ng::from_str(
            "workspace: { name: p }\ndispatch:\n  unknown: \"giant-other\"\n",
        )
        .unwrap();
        assert_eq!(cfg.dispatch.route("foo"), Some("giant-other"));
    }

    #[test]
    fn dispatch_rules_first_match_wins_and_can_miss() {
        let cfg: Config = serde_yaml_ng::from_str(
            r#"
workspace: { name: p }
dispatch:
  unknown:
    - { match: "db:*", to: "giant-dbtool" }
    - { match: "deploy", to: "giant-deploy" }
"#,
        )
        .unwrap();
        assert_eq!(cfg.dispatch.route("db:migrate"), Some("giant-dbtool"));
        assert_eq!(cfg.dispatch.route("deploy"), Some("giant-deploy"));
        // No `*` rule, so an unmatched name falls through to None.
        assert_eq!(cfg.dispatch.route("test-thing"), None);
    }

    #[test]
    fn load_dispatch_reads_table_from_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("giant.yaml"),
            // A full-ish config: load_dispatch ignores everything but `dispatch:`.
            "workspace: { name: p }\ntargets: []\ndispatch:\n  unknown: \"giant-other\"\n",
        )
        .unwrap();
        let d = load_dispatch(dir.path());
        assert_eq!(d.route("foo"), Some("giant-other"));
    }

    #[test]
    fn load_dispatch_degrades_to_default_when_absent_or_malformed() {
        // No config anywhere up the tree → default route.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(load_dispatch(empty.path()).route("x"), Some("giant-task"));

        // Malformed YAML → default route, never a hard error.
        let bad = tempfile::tempdir().unwrap();
        std::fs::write(bad.path().join("giant.yaml"), "this: : : not yaml\n").unwrap();
        assert_eq!(load_dispatch(bad.path()).route("x"), Some("giant-task"));
    }
}
