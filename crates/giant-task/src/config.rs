//! Discover + validate the workspace's tasks, packaged like targets.
//!
//! The runner uses core's workspace scan to find every package (each
//! `giant.yaml` directory), then re-reads each package's `tasks:` /
//! `services:` block with this porcelain's own schema. Tasks and
//! services are keyed by a `//<package>:<name>` label, exactly as core
//! keys targets. A reference inside a `needs:` / `finally:` / `services:`
//! list resolves within the task's own package, unless it is itself a
//! `//pkg:name` label.
//!
//! Validation here is strictly task-shape: empty command, default not in
//! choices, unresolved cross-references, service cycles. Target
//! validation is core's job; we don't touch targets.

use crate::schema::{ArgSpec, ServiceSpec, TaskSpec, TopLevel};
use indexmap::IndexMap;
use std::path::{Path, PathBuf};

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

    #[error("workspace scan failed: {0}")]
    Scan(String),

    #[error("validation: {0}")]
    Validation(String),
}

/// Resolving a user-typed task reference to a single task.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no task named '{0}' - try `giant task --list`")]
    Unknown(String),

    #[error(
        "task '{name}' is defined in several packages; qualify it, e.g. `giant task {first}` (candidates: {all})"
    )]
    Ambiguous {
        name: String,
        first: String,
        all: String,
    },
}

/// One task plus the package it was declared in. The map key is the
/// `//<package>:<name>` label; `name` is the bare name, kept for display
/// and bare-name resolution.
#[derive(Debug, Clone)]
pub struct Task {
    pub package: String,
    pub name: String,
    pub spec: TaskSpec,
}

/// One service plus its package. Same labelling as tasks.
#[derive(Debug, Clone)]
pub struct Service {
    pub package: String,
    pub name: String,
    pub spec: ServiceSpec,
}

/// The merged, packaged view of the workspace's tasks and services.
#[derive(Debug)]
pub struct TaskConfig {
    pub workspace_name: String,
    pub workspace_root: PathBuf,
    /// Tasks keyed by `//<package>:<name>` label, in discovery order.
    pub tasks: IndexMap<String, Task>,
    /// Services keyed by `//<package>:<name>` label.
    pub services: IndexMap<String, Service>,
}

/// Build a `//<package>:<name>` label. The root package (`""`) yields
/// `//:name`, matching how core labels root-package targets.
pub fn label(package: &str, name: &str) -> String {
    format!("//{package}:{name}")
}

/// The working directory for a task or service declared in `package`. An
/// unset cwd defaults to the package directory; an explicit cwd resolves
/// package-relative (`//x` escapes to the workspace root, `.` is the
/// package dir), matching how core resolves target paths.
pub fn package_cwd(workspace_root: &Path, package: &str, cwd: Option<&str>) -> PathBuf {
    match cwd {
        Some(raw) if !raw.is_empty() => {
            if let Some(rooted) = raw.strip_prefix("//") {
                workspace_root.join(rooted)
            } else if raw == "." {
                workspace_root.join(package)
            } else if package.is_empty() {
                workspace_root.join(raw)
            } else {
                workspace_root.join(format!("{package}/{raw}"))
            }
        }
        _ => workspace_root.join(package),
    }
}

/// Resolve a reference written inside `from_pkg` (a `needs` / `finally` /
/// `services` entry) to a label key. A `//…` reference is already a
/// label; a bare name binds to the writer's own package.
pub(crate) fn resolve_ref(reference: &str, from_pkg: &str) -> String {
    if reference.starts_with("//") {
        reference.to_string()
    } else {
        label(from_pkg, reference)
    }
}

/// Whether package `pkg` encloses the workspace-relative `cwd_rel`. The
/// root package (`""`) encloses everything.
fn package_encloses(pkg: &str, cwd_rel: &str) -> bool {
    pkg.is_empty() || cwd_rel == pkg || cwd_rel.starts_with(&format!("{pkg}/"))
}

impl TaskConfig {
    /// Discover every package via core's workspace scan, then load each
    /// package's `tasks:` / `services:` into one label-keyed view.
    /// `explicit` pins the workspace root config (the `--config` flag);
    /// `None` walks up from cwd.
    pub fn scan(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let (core, workspace_root) = giant::config::Config::scan_workspace(explicit)
            .map_err(|e| ConfigError::Scan(format!("{e}")))?;

        let mut tasks = IndexMap::new();
        let mut services = IndexMap::new();
        for pkg in &core.packages {
            let path = workspace_root.join(&pkg.config);
            let top = parse(&path)?;
            for (name, spec) in top.tasks {
                tasks.insert(
                    label(&pkg.package, &name),
                    Task {
                        package: pkg.package.clone(),
                        name,
                        spec,
                    },
                );
            }
            for (name, spec) in top.services {
                services.insert(
                    label(&pkg.package, &name),
                    Service {
                        package: pkg.package.clone(),
                        name,
                        spec,
                    },
                );
            }
        }

        let cfg = Self {
            workspace_name: core.workspace.name,
            workspace_root,
            tasks,
            services,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Resolve a user-typed task reference (`name` or `//pkg:name`),
    /// interpreted from `cwd`, to a task label key. Bare names prefer the
    /// nearest enclosing package; a name unique across the workspace
    /// resolves even from outside any defining package; a name shared by
    /// several packages with none enclosing `cwd` is ambiguous.
    pub fn resolve(&self, reference: &str, cwd: &Path) -> Result<String, ResolveError> {
        if reference.starts_with("//") {
            return if self.tasks.contains_key(reference) {
                Ok(reference.to_string())
            } else {
                Err(ResolveError::Unknown(reference.to_string()))
            };
        }

        let candidates: Vec<&Task> = self
            .tasks
            .values()
            .filter(|t| t.name == reference)
            .collect();
        match candidates.as_slice() {
            [] => Err(ResolveError::Unknown(reference.to_string())),
            [only] => Ok(label(&only.package, &only.name)),
            many => {
                let cwd_rel = cwd
                    .strip_prefix(&self.workspace_root)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                // Nearest enclosing package wins (deepest dir).
                if let Some(t) = many
                    .iter()
                    .filter(|t| package_encloses(&t.package, &cwd_rel))
                    .max_by_key(|t| t.package.len())
                {
                    return Ok(label(&t.package, &t.name));
                }
                let mut labels: Vec<String> =
                    many.iter().map(|t| label(&t.package, &t.name)).collect();
                labels.sort();
                Err(ResolveError::Ambiguous {
                    name: reference.to_string(),
                    first: labels[0].clone(),
                    all: labels.join(", "),
                })
            }
        }
    }

    /// Resolve a `needs` / `finally` reference written in `from_pkg` to a
    /// task label, if it exists.
    pub fn task_ref(&self, reference: &str, from_pkg: &str) -> Option<&Task> {
        self.tasks.get(&resolve_ref(reference, from_pkg))
    }

    /// Task and service validation across the merged, packaged view.
    fn validate(&self) -> Result<(), ConfigError> {
        for (lbl, svc) in &self.services {
            if !is_valid_name(&svc.name) {
                return Err(ConfigError::Validation(format!(
                    "service name {:?} is invalid (alphanumeric, '-', '_'; no leading digit)",
                    svc.name
                )));
            }
            if svc.spec.command.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "service '{lbl}' has an empty command"
                )));
            }
            if let Some(probe) = &svc.spec.ready {
                if probe.command.is_empty() {
                    return Err(ConfigError::Validation(format!(
                        "service '{lbl}' ready.command is empty"
                    )));
                }
                if probe.period_secs == 0 {
                    return Err(ConfigError::Validation(format!(
                        "service '{lbl}' ready.period_secs must be at least 1"
                    )));
                }
                if probe.timeout_secs == 0 {
                    return Err(ConfigError::Validation(format!(
                        "service '{lbl}' ready.timeout_secs must be at least 1"
                    )));
                }
            }
            for needed in &svc.spec.needs {
                let nlbl = resolve_ref(needed, &svc.package);
                if !self.services.contains_key(&nlbl) {
                    return Err(ConfigError::Validation(format!(
                        "service '{lbl}' needs '{needed}' but no such service is defined"
                    )));
                }
                if nlbl == *lbl {
                    return Err(ConfigError::Validation(format!(
                        "service '{lbl}' lists itself in `needs:`"
                    )));
                }
            }
        }
        self.validate_services_acyclic()?;

        for (lbl, task) in &self.tasks {
            if !is_valid_name(&task.name) {
                return Err(ConfigError::Validation(format!(
                    "task name {:?} is invalid (alphanumeric, '-', '_'; no leading digit)",
                    task.name
                )));
            }
            match &task.spec.command {
                Some(c) if c.is_empty() => {
                    return Err(ConfigError::Validation(format!(
                        "task '{lbl}' has an empty command"
                    )));
                }
                // A task with no command must supervise services (the
                // `giant dev` shape); one with neither does nothing.
                None if task.spec.services.is_empty() => {
                    return Err(ConfigError::Validation(format!(
                        "task '{lbl}' has no command and no services - it does nothing"
                    )));
                }
                _ => {}
            }
            validate_args(lbl, &task.spec.args)?;
            for needed in &task.spec.needs {
                let nlbl = resolve_ref(needed, &task.package);
                if !self.tasks.contains_key(&nlbl) {
                    return Err(ConfigError::Validation(format!(
                        "task '{lbl}' needs '{needed}' but no such task is defined"
                    )));
                }
                if nlbl == *lbl {
                    return Err(ConfigError::Validation(format!(
                        "task '{lbl}' lists itself in `needs:`"
                    )));
                }
            }
            for fin in &task.spec.finally {
                if !self.tasks.contains_key(&resolve_ref(fin, &task.package)) {
                    return Err(ConfigError::Validation(format!(
                        "task '{lbl}' has finally '{fin}' but no such task is defined"
                    )));
                }
            }
            for svc in &task.spec.services {
                if !self.services.contains_key(&resolve_ref(svc, &task.package)) {
                    return Err(ConfigError::Validation(format!(
                        "task '{lbl}' wants service '{svc}' but no such service is defined"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Detect cycles in the service `needs:` graph (DFS with a recursion
    /// stack), resolving each reference within its service's package. A
    /// cycle would deadlock the topological foreground start.
    fn validate_services_acyclic(&self) -> Result<(), ConfigError> {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Visiting,
            Done,
        }
        fn dfs(
            node: &str,
            cfg: &TaskConfig,
            marks: &mut std::collections::HashMap<String, Mark>,
        ) -> Result<(), ConfigError> {
            match marks.get(node) {
                Some(Mark::Done) => return Ok(()),
                Some(Mark::Visiting) => {
                    return Err(ConfigError::Validation(format!(
                        "service `needs:` graph has a cycle through '{node}'"
                    )));
                }
                None => {}
            }
            marks.insert(node.to_string(), Mark::Visiting);
            if let Some(svc) = cfg.services.get(node) {
                for n in &svc.spec.needs {
                    dfs(&resolve_ref(n, &svc.package), cfg, marks)?;
                }
            }
            marks.insert(node.to_string(), Mark::Done);
            Ok(())
        }
        let mut marks = std::collections::HashMap::new();
        for lbl in self.services.keys() {
            dfs(lbl, self, &mut marks)?;
        }
        Ok(())
    }
}

/// Parse one config file's task-relevant top level. Other sections core
/// owns (targets, cache, …) are ignored by `TopLevel`'s shape.
fn parse(path: &Path) -> Result<TopLevel, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Read {
        path: path.display().to_string(),
        source: e,
    })?;
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => serde_json::from_str(&raw).map_err(|e| ConfigError::Json {
            path: path.display().to_string(),
            source: e,
        }),
        _ => serde_yaml_ng::from_str(&raw).map_err(|e| ConfigError::Yaml {
            path: path.display().to_string(),
            source: e,
        }),
    }
}

fn validate_args(task: &str, args: &[ArgSpec]) -> Result<(), ConfigError> {
    let mut seen = std::collections::HashSet::new();
    for (i, arg) in args.iter().enumerate() {
        if arg.name.is_empty() {
            return Err(ConfigError::Validation(format!(
                "task '{task}' has an arg with an empty name"
            )));
        }
        if !seen.insert(&arg.name) {
            return Err(ConfigError::Validation(format!(
                "task '{task}' declares arg '{}' more than once",
                arg.name
            )));
        }
        // A variadic arg must be the last one.
        if arg.variadic && i != args.len() - 1 {
            return Err(ConfigError::Validation(format!(
                "task '{task}' arg '{}' is variadic but not last; only the \
                 final arg may be variadic",
                arg.name
            )));
        }
        validate_arg(task, arg)?;
    }
    Ok(())
}

fn validate_arg(task: &str, spec: &ArgSpec) -> Result<(), ConfigError> {
    let arg = &spec.name;
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
    use std::fs;

    /// Write a workspace with the given `(relative-dir, yaml)` configs and
    /// scan it. An empty dir means the root config.
    fn scan_workspace(files: &[(&str, &str)]) -> Result<(TaskConfig, PathBuf), ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for (rel, body) in files {
            let d = if rel.is_empty() {
                root.clone()
            } else {
                root.join(rel)
            };
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("giant.yaml"), body).unwrap();
        }
        // Keep the tempdir alive for the test's duration by leaking it into
        // the returned root path's backing dir; the OS cleans /tmp anyway.
        let cfg = TaskConfig::scan(Some(&root.join("giant.yaml")))?;
        std::mem::forget(dir);
        Ok((cfg, root))
    }

    #[test]
    fn scans_tasks_across_packages_with_labels() {
        let (cfg, _root) = scan_workspace(&[
            (
                "",
                "workspace: { name: w }\ntasks:\n  build:\n    command: \"true\"\n",
            ),
            ("blackmetal", "tasks:\n  test:\n    command: \"true\"\n"),
        ])
        .unwrap();
        assert!(cfg.tasks.contains_key("//:build"));
        assert!(cfg.tasks.contains_key("//blackmetal:test"));
        assert_eq!(cfg.tasks["//blackmetal:test"].package, "blackmetal");
        assert_eq!(cfg.tasks["//blackmetal:test"].name, "test");
    }

    #[test]
    fn bare_name_prefers_nearest_enclosing_package() {
        let (cfg, root) = scan_workspace(&[
            (
                "",
                "workspace: { name: w }\ntasks:\n  test:\n    command: \"echo root\"\n",
            ),
            ("blackmetal", "tasks:\n  test:\n    command: \"echo bm\"\n"),
        ])
        .unwrap();
        // From inside blackmetal, `test` is blackmetal's.
        assert_eq!(
            cfg.resolve("test", &root.join("blackmetal")).unwrap(),
            "//blackmetal:test"
        );
        // From the root, `test` is the root's.
        assert_eq!(cfg.resolve("test", &root).unwrap(), "//:test");
    }

    #[test]
    fn bare_name_unique_anywhere_resolves_from_root() {
        let (cfg, root) = scan_workspace(&[
            ("", "workspace: { name: w }\n"),
            ("blackmetal", "tasks:\n  deploy:\n    command: \"true\"\n"),
        ])
        .unwrap();
        assert_eq!(cfg.resolve("deploy", &root).unwrap(), "//blackmetal:deploy");
    }

    #[test]
    fn bare_name_ambiguous_when_several_and_none_enclosing() {
        let (cfg, root) = scan_workspace(&[
            ("", "workspace: { name: w }\n"),
            ("blackmetal", "tasks:\n  test:\n    command: \"true\"\n"),
            ("cryosleep", "tasks:\n  test:\n    command: \"true\"\n"),
        ])
        .unwrap();
        let err = cfg.resolve("test", &root).unwrap_err();
        assert!(matches!(err, ResolveError::Ambiguous { .. }), "got: {err}");
        assert!(format!("{err}").contains("//blackmetal:test"));
    }

    #[test]
    fn explicit_label_resolves_directly() {
        let (cfg, root) = scan_workspace(&[
            ("", "workspace: { name: w }\n"),
            ("blackmetal", "tasks:\n  test:\n    command: \"true\"\n"),
        ])
        .unwrap();
        assert_eq!(
            cfg.resolve("//blackmetal:test", &root).unwrap(),
            "//blackmetal:test"
        );
        assert!(matches!(
            cfg.resolve("//nope:test", &root).unwrap_err(),
            ResolveError::Unknown(_)
        ));
    }

    #[test]
    fn cross_package_needs_resolve_by_label() {
        // A root task that `needs:` a package task via its label.
        let (cfg, _root) = scan_workspace(&[
            (
                "",
                "workspace: { name: w }\ntasks:\n  all:\n    command: \"true\"\n    needs: [\"//bm:test\"]\n",
            ),
            ("bm", "tasks:\n  test:\n    command: \"true\"\n"),
        ])
        .unwrap();
        assert!(cfg.task_ref("//bm:test", "").is_some());
    }

    #[test]
    fn rejects_unresolved_need() {
        let err = scan_workspace(&[(
            "",
            "workspace: { name: w }\ntasks:\n  a:\n    command: \"true\"\n    needs: [\"ghost\"]\n",
        )])
        .unwrap_err();
        assert!(format!("{err}").contains("no such task"));
    }

    #[test]
    fn rejects_empty_command() {
        let err = scan_workspace(&[(
            "",
            "workspace: { name: w }\ntasks:\n  bad:\n    command: \"\"\n",
        )])
        .unwrap_err();
        assert!(format!("{err}").contains("empty command"));
    }

    #[test]
    fn rejects_service_needs_cycle() {
        let err = scan_workspace(&[(
            "",
            "workspace: { name: w }\nservices:\n  a: { command: \"x\", needs: [\"b\"] }\n  b: { command: \"y\", needs: [\"a\"] }\n",
        )])
        .unwrap_err();
        assert!(format!("{err}").contains("cycle"));
    }

    #[test]
    fn accepts_command_less_task_that_supervises_services() {
        let (cfg, _root) = scan_workspace(&[(
            "",
            "workspace: { name: w }\nservices:\n  db: { command: \"postgres\" }\ntasks:\n  dev:\n    services: [\"db\"]\n",
        )])
        .unwrap();
        assert!(cfg.tasks["//:dev"].spec.command.is_none());
    }
}
