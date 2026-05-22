//! Task lifecycle: deps → services → needs → command → finally → stop.
//!
//! Each step blocks the next. If a step fails:
//!  - deps fail: stop, run nothing else, return the dep error
//!  - services fail to come ready: stop already-started services,
//!    skip needs/command/finally, return the service error
//!  - a `need` task fails: skip command, still run `finally`, still
//!    stop services
//!  - command fails: still run `finally`, still stop services. The
//!    exit code from `command` is what the task returns.
//!  - `finally` failures don't change the exit code but are logged

use crate::config::TaskConfig;
use crate::deps;
use crate::render;
use crate::schema::TaskSpec;
use crate::services::{self, RunningService};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("no task named '{name}' - try `giant task --list`")]
    UnknownTask { name: String },

    #[error("argument '{name}': {detail}")]
    BadArg { name: String, detail: String },

    #[error("dependency build failed (exit code {0})")]
    DepsFailed(i32),

    #[error("service startup failed: {0}")]
    Service(#[from] crate::services::ServiceError),

    #[error("need '{0}' failed")]
    NeedFailed(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Resolve + run. Returns the task command's exit code (0 on success).
pub async fn run(
    cfg: &TaskConfig,
    name: &str,
    arg_kvs: &[String],
    passthrough: &[OsString],
    workspace_root: &Path,
    verbose: bool,
) -> anyhow::Result<u8> {
    let spec = cfg.tasks.get(name).ok_or_else(|| RunError::UnknownTask {
        name: name.to_string(),
    })?;

    let resolved_args = resolve_args(spec, arg_kvs)?;

    // 1. Build deps.
    if !spec.deps.is_empty() {
        let code = deps::build(&spec.deps, workspace_root, verbose).await?;
        if code != 0 {
            return Err(RunError::DepsFailed(code).into());
        }
    }

    // 2. Start services (parallel) and wait for all to be ready.
    let mut running_services = Vec::new();
    if !spec.services.is_empty() {
        match start_services(cfg, &spec.services, workspace_root).await {
            Ok(svcs) => running_services = svcs,
            Err((started, err)) => {
                // Roll back what we did start, then bail.
                services::stop_all(started).await;
                return Err(err.into());
            }
        }
    }

    // The rest of the lifecycle wraps in a single helper so we can
    // ALWAYS run `finally` + stop services on the way out, no matter
    // how the body exits.
    let body_result = run_body(cfg, spec, &resolved_args, passthrough, workspace_root).await;

    // 5. finally tasks (sequential).
    if !spec.finally.is_empty() {
        render::note(&format!("finally: {}", spec.finally.join(", ")));
        for fin in &spec.finally {
            if let Err(e) = run_finally(cfg, fin, workspace_root).await {
                render::note(&format!("finally '{fin}' failed: {e}"));
            }
        }
    }

    // 6. Stop services.
    if !running_services.is_empty() {
        render::note(&format!("stopping services: {}", spec.services.join(", ")));
        services::stop_all(running_services).await;
    }

    body_result.map_err(Into::into)
}

/// Inner body: run `needs`, then run the task's `command`. Wrapped so
/// the outer driver can always reach the finally/cleanup steps.
async fn run_body(
    cfg: &TaskConfig,
    spec: &TaskSpec,
    resolved_args: &HashMap<String, String>,
    passthrough: &[OsString],
    workspace_root: &Path,
) -> Result<u8, RunError> {
    // 3. needs (sequential).
    for need in &spec.needs {
        let need_spec = cfg
            .tasks
            .get(need)
            .ok_or_else(|| RunError::UnknownTask { name: need.clone() })?;
        render::note(&format!("need: {need}"));
        let code = run_command(need_spec, &HashMap::new(), &[], workspace_root).await?;
        if code != 0 {
            return Err(RunError::NeedFailed(need.clone()));
        }
    }

    // 4. The main command.
    run_command(spec, resolved_args, passthrough, workspace_root).await
}

/// Spawn each service concurrently. If any fails, returns the
/// already-started ones so the caller can stop them.
async fn start_services(
    cfg: &TaskConfig,
    names: &[String],
    workspace_root: &Path,
) -> Result<Vec<RunningService>, (Vec<RunningService>, crate::services::ServiceError)> {
    render::note(&format!("starting services: {}", names.join(", ")));
    let mut futures = Vec::with_capacity(names.len());
    for name in names {
        let spec = cfg
            .services
            .get(name)
            .expect("validated at config-load: every name in task.services exists");
        let name = name.clone();
        let spec = spec.clone();
        let root = workspace_root.to_path_buf();
        futures.push(tokio::spawn(async move {
            services::start(&name, &spec, &root).await
        }));
    }

    let mut started = Vec::new();
    let mut failed: Option<crate::services::ServiceError> = None;
    for fut in futures {
        match fut.await {
            Ok(Ok(svc)) => started.push(svc),
            Ok(Err(e)) => {
                failed.get_or_insert(e);
            }
            Err(join_err) => {
                failed.get_or_insert(crate::services::ServiceError::Spawn {
                    name: "<panicked>".into(),
                    source: std::io::Error::other(join_err.to_string()),
                });
            }
        }
    }
    match failed {
        Some(e) => Err((started, e)),
        None => Ok(started),
    }
}

async fn run_finally(cfg: &TaskConfig, name: &str, workspace_root: &Path) -> Result<(), RunError> {
    let spec = cfg.tasks.get(name).ok_or_else(|| RunError::UnknownTask {
        name: name.to_string(),
    })?;
    let code = run_command(spec, &HashMap::new(), &[], workspace_root).await?;
    if code != 0 {
        return Err(RunError::NeedFailed(name.to_string()));
    }
    Ok(())
}

/// Bind CLI `--arg key=value` pairs against the declared `args:`
/// table. Applies defaults, validates against `choices`, and returns a
/// map suitable for `GIANT_ARG_<NAME>=<VALUE>` env injection.
fn resolve_args(spec: &TaskSpec, kvs: &[String]) -> Result<HashMap<String, String>, RunError> {
    let mut user_supplied = HashMap::new();
    for kv in kvs {
        let (k, v) = kv.split_once('=').ok_or_else(|| RunError::BadArg {
            name: kv.clone(),
            detail: "expected --arg key=value".into(),
        })?;
        user_supplied.insert(k.to_string(), v.to_string());
    }

    let mut out = HashMap::new();
    for (arg_name, arg_spec) in &spec.args {
        let val = match user_supplied
            .remove(arg_name)
            .or_else(|| arg_spec.default.clone())
        {
            Some(v) => v,
            None => {
                return Err(RunError::BadArg {
                    name: arg_name.clone(),
                    detail: "no value supplied and no default declared".into(),
                });
            }
        };
        if let Some(choices) = &arg_spec.choices
            && !choices.contains(&val)
        {
            return Err(RunError::BadArg {
                name: arg_name.clone(),
                detail: format!("value {val:?} is not one of {choices:?}"),
            });
        }
        out.insert(arg_name.clone(), val);
    }

    // Anything left over in user_supplied was a key we don't declare.
    if let Some((unknown, _)) = user_supplied.into_iter().next() {
        return Err(RunError::BadArg {
            name: unknown,
            detail: "no such declared arg for this task".into(),
        });
    }

    Ok(out)
}

async fn run_command(
    spec: &TaskSpec,
    args: &HashMap<String, String>,
    passthrough: &[OsString],
    workspace_root: &Path,
) -> Result<u8, RunError> {
    let cwd = match &spec.cwd {
        Some(rel) => workspace_root.join(rel),
        None => workspace_root.to_path_buf(),
    };

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&spec.command);
    // Pass-through args become $1..$N inside sh -c.
    cmd.arg("--");
    for p in passthrough {
        cmd.arg(p);
    }
    cmd.current_dir(&cwd);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    for (k, v) in args {
        cmd.env(format!("GIANT_ARG_{}", k.to_ascii_uppercase()), v);
    }
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    render::running(&spec_label(spec));

    let status = if let Some(secs) = spec.timeout_secs {
        let mut child = cmd.spawn()?;
        let dur = std::time::Duration::from_secs(secs);
        match tokio::time::timeout(dur, child.wait()).await {
            Ok(s) => s?,
            Err(_) => {
                // timed out - best-effort kill, propagate non-zero.
                let _ = child.start_kill();
                render::note(&format!("task timed out after {secs}s"));
                return Ok(124);
            }
        }
    } else {
        cmd.status().await?
    };

    Ok(status.code().map(|c| c.clamp(0, 255) as u8).unwrap_or(1))
}

/// Short label for the "▶ <task>" header. Uses the description when
/// available; falls back to the command's first 60 chars.
fn spec_label(spec: &TaskSpec) -> String {
    spec.description.clone().unwrap_or_else(|| {
        let mut c = spec.command.clone();
        if c.len() > 60 {
            c.truncate(57);
            c.push_str("...");
        }
        c
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn empty_spec() -> TaskSpec {
        TaskSpec {
            command: "true".into(),
            description: None,
            deps: vec![],
            needs: vec![],
            services: vec![],
            finally: vec![],
            args: IndexMap::new(),
            env: HashMap::new(),
            cwd: None,
            timeout_secs: None,
            inputs: vec![],
        }
    }

    #[test]
    fn arg_default_applied_when_user_omits() {
        use crate::schema::TaskArg;
        let mut s = empty_spec();
        s.args.insert(
            "env".into(),
            TaskArg {
                default: Some("staging".into()),
                choices: None,
                description: None,
            },
        );
        let out = resolve_args(&s, &[]).unwrap();
        assert_eq!(out.get("env").unwrap(), "staging");
    }

    #[test]
    fn arg_user_value_overrides_default() {
        use crate::schema::TaskArg;
        let mut s = empty_spec();
        s.args.insert(
            "env".into(),
            TaskArg {
                default: Some("staging".into()),
                choices: None,
                description: None,
            },
        );
        let out = resolve_args(&s, &["env=prod".into()]).unwrap();
        assert_eq!(out.get("env").unwrap(), "prod");
    }

    #[test]
    fn arg_value_must_be_in_choices() {
        use crate::schema::TaskArg;
        let mut s = empty_spec();
        s.args.insert(
            "env".into(),
            TaskArg {
                default: Some("staging".into()),
                choices: Some(vec!["staging".into(), "prod".into()]),
                description: None,
            },
        );
        let err = resolve_args(&s, &["env=staging-2".into()]).unwrap_err();
        assert!(format!("{err}").contains("not one of"));
    }

    #[test]
    fn arg_with_no_default_and_no_value_errors() {
        use crate::schema::TaskArg;
        let mut s = empty_spec();
        s.args.insert(
            "env".into(),
            TaskArg {
                default: None,
                choices: None,
                description: None,
            },
        );
        let err = resolve_args(&s, &[]).unwrap_err();
        assert!(format!("{err}").contains("no value supplied"));
    }

    #[test]
    fn unknown_arg_key_errors() {
        let s = empty_spec();
        let err = resolve_args(&s, &["nope=1".into()]).unwrap_err();
        assert!(format!("{err}").contains("no such declared arg"));
    }

    #[test]
    fn malformed_kv_errors() {
        let s = empty_spec();
        let err = resolve_args(&s, &["nosep".into()]).unwrap_err();
        assert!(format!("{err}").contains("expected --arg key=value"));
    }
}
