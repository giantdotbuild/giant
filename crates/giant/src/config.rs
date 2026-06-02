//! Config loading and validation.
//!
//! See TDD-0001 for the schema, ADR-0007 for YAML-as-sugar policy.

use crate::model::{Input, TargetId, TargetSpec};
use crate::paths::{OutputPath, WsRelPath};
use crate::types::GlobPattern;
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

    // Defaulted so a package config (targets only, no `workspace:`) parses;
    // `validate_static` still requires a non-empty name on the root.
    #[serde(default)]
    pub workspace: WorkspaceConfig,

    #[serde(default)]
    pub targets: Vec<TargetSpec>,

    #[serde(default)]
    pub cache: CacheConfig,

    #[serde(default)]
    pub state: StateConfig,

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
        if let Some(found) = find_config_in_dir(d) {
            return Some(found);
        }
        dir = d.parent();
    }
    None
}

/// Workspace-local engine state. Distinct from `cache.dir` (which can
/// be shared / remote) - state is the small set of files giant writes
/// next to the workspace so concurrent runs and other tools can see
/// them: build logs and other per-workspace state.
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
        let mut cfg = Self::parse(path)?;
        cfg.validate_static()?;
        // A single root config → the root package (`""`). The workspace
        // scan (`scan`) derives the package per file from its directory.
        cfg.finalize_package("")?;
        Ok(cfg)
    }

    /// Read + deserialize a config file (YAML or JSON by extension). No
    /// validation or finalization - callers run those.
    fn parse(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path)?;
        let cfg = match path.extension().and_then(|e| e.to_str()) {
            Some("json") => serde_json::from_str(&raw)?,
            _ => serde_yaml_ng::from_str(&raw)?,
        };
        Ok(cfg)
    }

    /// Assign each target its `//<package>:<name>` label, resolve its dep
    /// references, and rewrite its package-relative `inputs`/`outputs`/`cwd`
    /// to workspace-relative form (TDD-0001 §Path resolution). `package` is
    /// this file's directory, workspace-relative (`""` for the root).
    fn finalize_package(&mut self, package: &str) -> Result<(), ConfigError> {
        for t in &mut self.targets {
            let name = t.name.clone();
            t.id = TargetId::label(package, &name);
            for d in &mut t.deps {
                *d = resolve_dep_label(package, d.as_str());
            }
            for input in &mut t.inputs {
                let Input::File { glob } = input;
                let resolved = resolve_in_package(package, glob.as_str());
                *glob = GlobPattern::new(&resolved).map_err(|e| {
                    ConfigError::Validation(format!(
                        "target '{name}': invalid input glob '{resolved}': {e}"
                    ))
                })?;
            }
            let mut outputs = Vec::with_capacity(t.outputs_raw.len());
            for raw in &t.outputs_raw {
                let resolved = resolve_in_package(package, raw);
                outputs.push(OutputPath::new(&resolved).map_err(|e| {
                    ConfigError::Validation(format!(
                        "target '{name}': invalid output '{resolved}': {e}"
                    ))
                })?);
            }
            t.outputs = outputs;
            // An unset (or empty) `cwd` defaults to the package directory.
            let raw_cwd = t
                .cwd_raw
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(".");
            let resolved_cwd = resolve_in_package(package, raw_cwd);
            t.cwd = WsRelPath::new(&resolved_cwd).map_err(|e| {
                ConfigError::Validation(format!(
                    "target '{name}': invalid cwd '{resolved_cwd}': {e}"
                ))
            })?;
        }
        Ok(())
    }

    /// Static validation: things checkable on a single config file.
    /// (TDD-0001 §Validation.)
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

        self.validate_targets()
    }

    /// Per-file target checks (no workspace/schema). Shared by the root
    /// `validate_static` and the per-package validation in `scan`.
    fn validate_targets(&self) -> Result<(), ConfigError> {
        // Name rules + uniqueness within this file (= within the package).
        let mut seen = HashSet::new();
        for t in &self.targets {
            if t.name.is_empty() {
                return Err(ConfigError::Validation("target has empty name".into()));
            }
            if t.name.contains('/') || t.name.contains(':') {
                return Err(ConfigError::Validation(format!(
                    "target name '{}' may not contain '/' or ':' (those are label separators)",
                    t.name
                )));
            }
            if !seen.insert(t.name.clone()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate target name '{}' in the same package",
                    t.name
                )));
            }
            if t.command.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "target '{}' has empty command",
                    t.name
                )));
            }
            // Cacheable target with no outputs and no exists check is meaningless.
            if t.is_cacheable() && t.outputs_raw.is_empty() && t.exists.is_none() {
                return Err(ConfigError::Validation(format!(
                    "target '{}' is cacheable but has no outputs and no `exists:` check",
                    t.name
                )));
            }
        }
        Ok(())
    }

    /// Scan the workspace rooted at `root_dir` and merge every package's
    /// `giant.yaml` / `giant.json` into one config (TDD-0001 §scan and
    /// merge). The root config supplies workspace-global settings and its
    /// own root-package targets; each nested file contributes the targets
    /// of its package (its directory, workspace-relative).
    pub fn scan(root_dir: &Path) -> Result<Self, ConfigError> {
        let root_path = find_config_in_dir(root_dir).ok_or(ConfigError::NotFound)?;

        // Root config: full schema, root package `""`.
        let mut cfg = Self::parse(&root_path)?;
        cfg.validate_static()?;
        cfg.finalize_package("")?;

        // Nested package configs: targets only, package = their directory.
        for path in scan_config_files(root_dir) {
            if path == root_path {
                continue;
            }
            let package = path
                .parent()
                .and_then(|d| d.strip_prefix(root_dir).ok())
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();

            reject_root_only_sections(&path)?;
            let mut pkg = Self::parse(&path)?;
            pkg.validate_targets()
                .map_err(|e| ConfigError::Validation(format!("{}: {e}", path.display())))?;
            pkg.finalize_package(&package)?;
            cfg.targets.append(&mut pkg.targets);
        }

        cfg.validate_merged()?;
        Ok(cfg)
    }

    /// Locate the workspace root - the directory of an explicit config,
    /// else the nearest ancestor whose `giant.yaml` declares `workspace:`
    /// - then scan + merge the whole workspace. Returns the merged config
    /// and the workspace root directory.
    pub fn scan_workspace(explicit: Option<&Path>) -> Result<(Self, PathBuf), ConfigError> {
        let root_dir = find_workspace_root(explicit)?;
        let cfg = Self::scan(&root_dir)?;
        Ok((cfg, root_dir))
    }

    /// Load only the workspace root config (no package scan). For
    /// commands that need workspace-global settings (e.g. `cache.dir`)
    /// without building the graph, and so a broken package config can't
    /// stop them.
    pub fn load_root(explicit: Option<&Path>) -> Result<(Self, PathBuf), ConfigError> {
        let root_dir = find_workspace_root(explicit)?;
        let path = find_config_in_dir(&root_dir).ok_or(ConfigError::NotFound)?;
        Ok((Self::load(&path)?, root_dir))
    }

    /// Whole-graph check after the scan: target-label uniqueness across
    /// every package (TDD-0001 §scan and merge conflict rules). Output
    /// collisions are caught when the graph builds its inferred edges
    /// (`graph::BuildGraph`, `GraphError::OutputCollision`).
    fn validate_merged(&self) -> Result<(), ConfigError> {
        let mut labels = HashSet::new();
        for t in &self.targets {
            if !labels.insert(&t.id) {
                return Err(ConfigError::Validation(format!(
                    "duplicate target label '{}'",
                    t.id
                )));
            }
        }
        Ok(())
    }
}

/// Walk up from cwd (or use an explicit config's directory) to the
/// workspace root: the nearest ancestor whose `giant.yaml` declares a
/// non-empty `workspace.name`. Package configs (targets only) are passed
/// over on the way up.
fn find_workspace_root(explicit: Option<&Path>) -> Result<PathBuf, ConfigError> {
    if let Some(p) = explicit {
        let abs = std::fs::canonicalize(p)?;
        return abs
            .parent()
            .map(Path::to_path_buf)
            .ok_or(ConfigError::NotFound);
    }
    let cwd = std::env::current_dir()?;
    let mut here: &Path = &cwd;
    loop {
        if let Some(cand) = find_config_in_dir(here)
            && config_declares_workspace(&cand)
        {
            return Ok(here.to_path_buf());
        }
        match here.parent() {
            Some(parent) => here = parent,
            None => return Err(ConfigError::NotFound),
        }
    }
}

/// A package config may carry only `targets:` (plus the porcelain-reserved
/// `tasks:`/`services:`, which the engine passes over). Reject any
/// workspace-global section so a copied root config fails loudly instead
/// of being silently ignored (TDD-0001 §Root config vs package config).
fn reject_root_only_sections(path: &Path) -> Result<(), ConfigError> {
    const ROOT_ONLY: &[&str] = &[
        "workspace",
        "cache",
        "remote",
        "dispatch",
        "state",
        "schema_version",
    ];
    let raw = std::fs::read_to_string(path)?;
    let keys: std::collections::BTreeMap<String, serde::de::IgnoredAny> =
        match path.extension().and_then(|e| e.to_str()) {
            Some("json") => serde_json::from_str(&raw)?,
            _ => serde_yaml_ng::from_str(&raw)?,
        };
    if let Some(k) = keys.keys().find(|k| ROOT_ONLY.contains(&k.as_str())) {
        return Err(ConfigError::Validation(format!(
            "{}: '{k}:' is only valid in the workspace-root config, not a package config",
            path.display()
        )));
    }
    Ok(())
}

/// Whether `path`'s config declares a non-empty `workspace.name` - the
/// marker that identifies the workspace root vs a package config.
fn config_declares_workspace(path: &Path) -> bool {
    #[derive(Deserialize)]
    struct Peek {
        #[serde(default)]
        workspace: WorkspaceConfig,
    }
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let parsed: Option<Peek> = match path.extension().and_then(|e| e.to_str()) {
        Some("json") => serde_json::from_str(&raw).ok(),
        _ => serde_yaml_ng::from_str(&raw).ok(),
    };
    parsed.is_some_and(|p| !p.workspace.name.is_empty())
}

/// The `giant.yaml` / `giant.yml` / `giant.json` in `dir`, if any.
fn find_config_in_dir(dir: &Path) -> Option<PathBuf> {
    ["giant.yaml", "giant.yml", "giant.json"]
        .into_iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
}

/// Every `giant.yaml` / `giant.yml` / `giant.json` under `root_dir`,
/// respecting `.gitignore` and skipping the usual noise directories.
fn scan_config_files(root_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root_dir)
        .hidden(false)
        .build()
        .flatten()
    {
        if entry.file_type().is_some_and(|t| t.is_file())
            && matches!(
                entry.file_name().to_str(),
                Some("giant.yaml" | "giant.yml" | "giant.json")
            )
        {
            out.push(entry.into_path());
        }
    }
    out
}

fn is_valid_workspace_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Resolve a `deps:` reference to a full `//pkg:name` label. `//pkg:name`
/// is already absolute; `:name` (and a bare `name`) are same-package
/// (TDD-0001 §Path resolution).
fn resolve_dep_label(package: &str, dep: &str) -> TargetId {
    if dep.starts_with("//") {
        TargetId::new(dep)
    } else {
        TargetId::label(package, dep.strip_prefix(':').unwrap_or(dep))
    }
}

/// Resolve a package-relative or `//`-rooted config path (input glob,
/// output, cwd) to its workspace-relative form (TDD-0001 §Path
/// resolution). `package` is the target's package directory.
///
/// - `//x` → `x` (workspace root).
/// - `.` → the package directory.
/// - bare `x` → `<package>/x` (or `x` in the root package).
///
/// `..` is not handled here; the typed path constructors
/// (`WsRelPath`/`OutputPath`) reject it after resolution.
fn resolve_in_package(package: &str, raw: &str) -> String {
    if let Some(rooted) = raw.strip_prefix("//") {
        rooted.to_string()
    } else if raw == "." {
        package.to_string()
    } else if package.is_empty() {
        raw.to_string()
    } else {
        format!("{package}/{raw}")
    }
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
  - name: "build"
    inputs: ["src/**/*.rs", "Cargo.toml"]
    outputs: ["bin/app"]
    command: "cargo build --release"
"#,
        );
        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.targets.len(), 1);
        assert_eq!(cfg.targets[0].id.as_str(), "//:build");
        assert_eq!(cfg.targets[0].inputs.len(), 2);
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
    fn reject_duplicate_target_name() {
        let f = write_yaml(
            r#"
workspace: { name: p }
targets:
  - { name: "a", inputs: [], outputs: ["x"], command: "true" }
  - { name: "a", inputs: [], outputs: ["y"], command: "true" }
"#,
        );
        let err = Config::load(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("duplicate target"), "got: {msg}");
    }

    #[test]
    fn reject_cacheable_without_outputs_or_exists() {
        let f = write_yaml(
            r#"
workspace: { name: p }
targets:
  - { name: "a", inputs: [], outputs: [], command: "true" }
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
  - name: "img"
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
  - name: "lint"
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
  - name: "test"
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

    // --- scan + merge + package-relative paths (TDD-0001, M2) ---

    #[test]
    fn resolve_in_package_rules() {
        assert_eq!(resolve_in_package("", "x.go"), "x.go");
        assert_eq!(resolve_in_package("src/go", "x.go"), "src/go/x.go");
        assert_eq!(resolve_in_package("src/go", "sub/y"), "src/go/sub/y");
        assert_eq!(resolve_in_package("src/go", "//proto/a.go"), "proto/a.go");
        assert_eq!(resolve_in_package("src/go", "."), "src/go");
        assert_eq!(resolve_in_package("", "."), "");
    }

    #[test]
    fn scan_merges_packages_with_path_derived_labels_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("giant.yaml"), "workspace:\n  name: w\n").unwrap();
        std::fs::create_dir_all(root.join("src/lib")).unwrap();
        std::fs::write(
            root.join("src/lib/giant.yaml"),
            "targets:\n  - name: build\n    inputs: [\"a.go\"]\n    outputs: [\"out\"]\n    command: \"true\"\n",
        )
        .unwrap();

        let cfg = Config::scan(root).unwrap();
        let t = cfg
            .targets
            .iter()
            .find(|t| t.id.as_str() == "//src/lib:build")
            .expect("package label //src/lib:build");
        // Output + cwd resolved package-relative.
        assert_eq!(t.outputs[0].as_path().to_str().unwrap(), "src/lib/out");
        assert_eq!(t.cwd.as_path().to_str().unwrap(), "src/lib");
    }

    #[test]
    fn scan_disambiguates_same_name_across_packages() {
        // The same target name in different packages is fine - the package
        // path disambiguates the label, and package-relative outputs can't
        // collide. (`//`-anchored outputs that could collide are M2b.)
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("giant.yaml"),
            "workspace:\n  name: w\ntargets:\n  - name: build\n    outputs: [\"out\"]\n    command: \"true\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(
            root.join("pkg/giant.yaml"),
            "targets:\n  - name: build\n    outputs: [\"out\"]\n    command: \"true\"\n",
        )
        .unwrap();
        let cfg = Config::scan(root).unwrap();
        let labels: std::collections::HashSet<&str> =
            cfg.targets.iter().map(|t| t.id.as_str()).collect();
        assert!(labels.contains("//:build"));
        assert!(labels.contains("//pkg:build"));
    }

    #[test]
    fn finalize_resolves_rooted_output_and_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("giant.yaml"), "workspace:\n  name: w\n").unwrap();
        std::fs::create_dir_all(root.join("src/tool")).unwrap();
        std::fs::write(
            root.join("src/tool/giant.yaml"),
            "targets:\n  - name: build\n    inputs: [\"m.txt\"]\n    outputs: [\"//bin/tool\"]\n    cwd: \"//\"\n    command: \"true\"\n",
        )
        .unwrap();
        let cfg = Config::scan(root).unwrap();
        let t = cfg
            .targets
            .iter()
            .find(|t| t.id.as_str() == "//src/tool:build")
            .unwrap();
        // `//bin/tool` → workspace-root output; `//` cwd → workspace root.
        assert_eq!(t.outputs[0].as_path().to_str().unwrap(), "bin/tool");
        assert_eq!(t.cwd.as_path().to_str().unwrap(), "");
    }

    #[test]
    fn scan_rejects_root_only_field_in_package_config() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("giant.yaml"), "workspace:\n  name: w\n").unwrap();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(
            root.join("pkg/giant.yaml"),
            "cache:\n  dir: ./c\ntargets:\n  - name: x\n    outputs: [\"o\"]\n    command: \"true\"\n",
        )
        .unwrap();
        let msg = format!("{}", Config::scan(root).unwrap_err());
        assert!(
            msg.contains("cache") && msg.contains("workspace-root"),
            "got: {msg}"
        );
    }
}
