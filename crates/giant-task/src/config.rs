//! Load + validate a `giant.yaml` from the task-runner's point of view.
//!
//! Validation here is strictly task-shape: empty command, default not
//! in choices, etc. Target validation is core's job; we don't touch
//! targets at all.

use crate::schema::{ServiceSpec, TaskArg, TaskSpec, TopLevel};
use indexmap::IndexMap;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse YAML in {path}: {source}")]
    Yaml {
        path: String,
        #[source]
        source: serde_yaml_ng::Error,
    },

    #[error("failed to parse JSON in {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("validation: {0}")]
    Validation(String),
}

/// What giant-task uses at runtime: the workspace name plus the tasks
/// and services maps. Both come from `giant.yaml` (`giant.json` is
/// also supported by extension).
#[derive(Debug)]
pub struct TaskConfig {
    pub workspace_name: String,
    pub tasks: IndexMap<String, TaskSpec>,
    pub services: IndexMap<String, ServiceSpec>,
}

impl TaskConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Read {
            path: path.display().to_string(),
            source: e,
        })?;

        let top: TopLevel = match path.extension().and_then(|e| e.to_str()) {
            Some("json") => serde_json::from_str(&raw).map_err(|e| ConfigError::Json {
                path: path.display().to_string(),
                source: e,
            })?,
            _ => serde_yaml_ng::from_str(&raw).map_err(|e| ConfigError::Yaml {
                path: path.display().to_string(),
                source: e,
            })?,
        };

        validate(&top.tasks, &top.services)?;

        Ok(Self {
            workspace_name: top.workspace.name,
            tasks: top.tasks,
            services: top.services,
        })
    }
}

/// Task and service validation: names, commands, arg constraints,
/// and cross-references (every name in `needs`/`services`/`finally`
/// must resolve).
fn validate(
    tasks: &IndexMap<String, TaskSpec>,
    services: &IndexMap<String, ServiceSpec>,
) -> Result<(), ConfigError> {
    // Names that would shadow a built-in subcommand if dispatched via
    // `giant <name>`. The dispatch shim picks built-ins first, but
    // erroring here keeps the giant.yaml self-consistent.
    const RESERVED: &[&str] = &[
        "build", "test", "watch", "affected", "graph", "clean", "explain", "help",
    ];

    for (name, spec) in services {
        if !is_valid_name(name) {
            return Err(ConfigError::Validation(format!(
                "service name {name:?} is invalid (alphanumeric, '-', '_'; no leading digit)"
            )));
        }
        if spec.command.is_empty() {
            return Err(ConfigError::Validation(format!(
                "service '{name}' has an empty command"
            )));
        }
        if let Some(probe) = &spec.ready {
            if probe.command.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "service '{name}' ready.command is empty"
                )));
            }
            if probe.period_secs == 0 {
                return Err(ConfigError::Validation(format!(
                    "service '{name}' ready.period_secs must be at least 1"
                )));
            }
            if probe.timeout_secs == 0 {
                return Err(ConfigError::Validation(format!(
                    "service '{name}' ready.timeout_secs must be at least 1"
                )));
            }
        }
    }

    for (name, spec) in tasks {
        if !is_valid_name(name) {
            return Err(ConfigError::Validation(format!(
                "task name {name:?} is invalid (alphanumeric, '-', '_'; no leading digit)"
            )));
        }
        if RESERVED.contains(&name.as_str()) {
            return Err(ConfigError::Validation(format!(
                "task name '{name}' shadows a built-in `giant` subcommand"
            )));
        }
        if spec.command.is_empty() {
            return Err(ConfigError::Validation(format!(
                "task '{name}' has an empty command"
            )));
        }
        for (arg_name, arg) in &spec.args {
            validate_arg(name, arg_name, arg)?;
        }
        for needed in &spec.needs {
            if !tasks.contains_key(needed) {
                return Err(ConfigError::Validation(format!(
                    "task '{name}' needs '{needed}' but no such task is defined"
                )));
            }
            if needed == name {
                return Err(ConfigError::Validation(format!(
                    "task '{name}' lists itself in `needs:`"
                )));
            }
        }
        for fin in &spec.finally {
            if !tasks.contains_key(fin) {
                return Err(ConfigError::Validation(format!(
                    "task '{name}' has finally '{fin}' but no such task is defined"
                )));
            }
        }
        for svc in &spec.services {
            if !services.contains_key(svc) {
                return Err(ConfigError::Validation(format!(
                    "task '{name}' wants service '{svc}' but no such service is defined"
                )));
            }
        }
    }
    Ok(())
}

fn validate_arg(task: &str, arg: &str, spec: &TaskArg) -> Result<(), ConfigError> {
    if let Some(choices) = &spec.choices {
        if choices.is_empty() {
            return Err(ConfigError::Validation(format!(
                "task '{task}' arg '{arg}' has an empty choices list"
            )));
        }
        if let Some(default) = &spec.default
            && !choices.contains(default)
        {
            return Err(ConfigError::Validation(format!(
                "task '{task}' arg '{arg}' default {default:?} is not in choices {choices:?}"
            )));
        }
    }
    Ok(())
}

fn is_valid_name(s: &str) -> bool {
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

    fn write_yaml(s: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
        f.write_all(s.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn load_minimal() {
        let f = write_yaml(
            r#"
workspace: { name: p }
tasks:
  deploy:
    command: "kubectl apply -f k8s/"
"#,
        );
        let cfg = TaskConfig::load(f.path()).unwrap();
        assert_eq!(cfg.workspace_name, "p");
        assert_eq!(cfg.tasks.len(), 1);
        let t = cfg.tasks.get("deploy").unwrap();
        assert_eq!(t.command, "kubectl apply -f k8s/");
    }

    #[test]
    fn load_with_args_and_deps() {
        let f = write_yaml(
            r#"
workspace: { name: p }
tasks:
  deploy:
    command: "kubectl apply -f k8s/$GIANT_ARG_ENV/"
    deps: ["docker:api"]
    args:
      env:
        default: "staging"
        choices: ["staging", "prod"]
        description: "Target environment"
"#,
        );
        let cfg = TaskConfig::load(f.path()).unwrap();
        let t = cfg.tasks.get("deploy").unwrap();
        assert_eq!(t.deps, vec!["docker:api"]);
        assert_eq!(t.args.len(), 1);
        let env_arg = t.args.get("env").unwrap();
        assert_eq!(env_arg.default.as_deref(), Some("staging"));
    }

    #[test]
    fn ignores_targets_and_cache() {
        let f = write_yaml(
            r#"
workspace: { name: p }
targets:
  - id: "lib:foo"
    command: "true"
    outputs: []
    exists: "true"
    cache: false
cache:
  dir: ~/somewhere
tasks:
  hi:
    command: "echo hi"
"#,
        );
        // Core fields (targets, cache) are silently ignored by us.
        let cfg = TaskConfig::load(f.path()).unwrap();
        assert!(cfg.tasks.contains_key("hi"));
    }

    #[test]
    fn rejects_empty_command() {
        let f = write_yaml(
            r#"
workspace: { name: p }
tasks:
  bad:
    command: ""
"#,
        );
        let err = TaskConfig::load(f.path()).unwrap_err();
        assert!(format!("{err}").contains("empty command"));
    }

    #[test]
    fn rejects_reserved_name() {
        let f = write_yaml(
            r#"
workspace: { name: p }
tasks:
  build:
    command: "true"
"#,
        );
        let err = TaskConfig::load(f.path()).unwrap_err();
        assert!(format!("{err}").contains("shadows a built-in"));
    }

    #[test]
    fn rejects_default_not_in_choices() {
        let f = write_yaml(
            r#"
workspace: { name: p }
tasks:
  deploy:
    command: "true"
    args:
      env:
        default: "prod"
        choices: ["staging"]
"#,
        );
        let err = TaskConfig::load(f.path()).unwrap_err();
        assert!(format!("{err}").contains("not in choices"));
    }

    #[test]
    fn rejects_invalid_task_name() {
        let f = write_yaml(
            r#"
workspace: { name: p }
tasks:
  "1bad":
    command: "true"
"#,
        );
        let err = TaskConfig::load(f.path()).unwrap_err();
        assert!(format!("{err}").contains("invalid"));
    }
}
