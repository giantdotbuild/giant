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
use crate::signals::{self, Shutdown};
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
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
/// `positionals` are the bare args after the task name; `arg_kvs` are
/// explicit `--arg name=value` overrides.
pub async fn run(
    cfg: &TaskConfig,
    name: &str,
    positionals: &[String],
    arg_kvs: &[String],
    workspace_root: &Path,
    verbose: bool,
) -> anyhow::Result<u8> {
    let spec = cfg.tasks.get(name).ok_or_else(|| RunError::UnknownTask {
        name: name.to_string(),
    })?;

    let bound = bind_args(spec, positionals, arg_kvs)?;

    // 1. Build deps.
    if !spec.deps.is_empty() {
        let code = deps::build(&spec.deps, workspace_root, verbose).await?;
        if code != 0 {
            return Err(RunError::DepsFailed(code).into());
        }
    }

    // 2. Start services, dependency-ordered and ready-gated.
    let mut running = Vec::new();
    if !spec.services.is_empty() {
        match services::start_group(&cfg.services, &spec.services, workspace_root).await {
            Ok(svcs) => running = svcs,
            Err((started, err)) => {
                // Roll back what we did start, then bail.
                services::stop_all(started).await;
                return Err(err.into());
            }
        }
    }

    // Install signal handling only when there's something to tear down
    // (see `needs_shutdown`). A bare command keeps the default disposition
    // - Ctrl-C kills it (and the child, sharing our process group) with
    // nothing left to clean up.
    let shutdown = if needs_shutdown(spec) {
        Some(Shutdown::install()?)
    } else {
        None
    };

    // 3. Body: run the command (services scaffold around it), or - for a
    //    command-less task - supervise the services in the foreground
    //    until a signal or a service exits (the `giant dev` shape). The
    //    `finally` step is deliberately *not* signal-aware: once we're in
    //    cleanup, it runs to completion.
    let result = if spec.command.is_some() {
        let r = run_body(cfg, spec, &bound, workspace_root, shutdown.as_ref()).await;
        run_finallies(cfg, spec, workspace_root).await;
        r
    } else {
        // Supervise mode is `command.is_none()`, a `needs_shutdown` case,
        // so the handler is always installed here.
        let sd = shutdown.as_ref().expect("needs_shutdown ⇒ installed");
        supervise(&mut running, sd).await
    };

    // 4. Stop services (the whole group).
    if !running.is_empty() {
        render::note(&format!("stopping services: {}", spec.services.join(", ")));
        services::stop_all(running).await;
    }

    result.map_err(Into::into)
}

/// Whether the task has anything to tear down on a signal: services to
/// stop, `finally` to run, or it's a supervise-mode task (no command).
/// When true we install signal handling and give the command its own
/// process group; otherwise a signal takes the default disposition.
fn needs_shutdown(spec: &TaskSpec) -> bool {
    !spec.services.is_empty() || !spec.finally.is_empty() || spec.command.is_none()
}

/// `finally` tasks run after the command, on success or failure. Their
/// own failures are logged but don't change the task's exit code.
async fn run_finallies(cfg: &TaskConfig, spec: &TaskSpec, workspace_root: &Path) {
    if spec.finally.is_empty() {
        return;
    }
    render::note(&format!("finally: {}", spec.finally.join(", ")));
    for fin in &spec.finally {
        if let Err(e) = run_named_task(cfg, fin, workspace_root, None).await {
            render::note(&format!("finally '{fin}' failed: {e}"));
        }
    }
}

/// Foreground supervise mode: the services are up; hold until a shutdown
/// signal (SIGINT/SIGTERM) or any service exits, then return so the
/// caller stops the whole group.
async fn supervise(running: &mut [RunningService], shutdown: &Shutdown) -> Result<u8, RunError> {
    render::note("services up - Ctrl-C to stop");
    tokio::select! {
        _ = shutdown.recv() => {
            render::note("interrupted");
        }
        name = services::wait_any_exit(running) => {
            render::note(&format!("service '{name}' exited; shutting down"));
        }
    }
    Ok(0)
}

/// Inner body: run `needs`, then run the task's `command`. Wrapped so
/// the outer driver can always reach the finally/cleanup steps. A
/// shutdown signal during a need or the command aborts it (non-zero exit)
/// and falls through to cleanup.
async fn run_body(
    cfg: &TaskConfig,
    spec: &TaskSpec,
    bound: &BoundArgs,
    workspace_root: &Path,
    shutdown: Option<&Shutdown>,
) -> Result<u8, RunError> {
    // 3. needs (sequential).
    for need in &spec.needs {
        render::note(&format!("need: {need}"));
        run_named_task(cfg, need, workspace_root, shutdown).await?;
    }

    // 4. The main command.
    run_command(spec, bound, workspace_root, shutdown).await
}

/// Run another task by name to completion with no arguments. A non-zero
/// exit becomes `NeedFailed`. Shared by `needs:` (signal-aware) and
/// `finally:` (not - cleanup runs to completion).
async fn run_named_task(
    cfg: &TaskConfig,
    name: &str,
    workspace_root: &Path,
    shutdown: Option<&Shutdown>,
) -> Result<(), RunError> {
    let spec = cfg.tasks.get(name).ok_or_else(|| RunError::UnknownTask {
        name: name.to_string(),
    })?;
    let code = run_command(spec, &BoundArgs::default(), workspace_root, shutdown).await?;
    if code != 0 {
        return Err(RunError::NeedFailed(name.to_string()));
    }
    Ok(())
}

/// The result of binding a task's declared args to an invocation.
#[derive(Debug, Default)]
struct BoundArgs {
    /// Scalar `(name, value)` pairs, exported as `GIANT_ARG_<NAME>` and
    /// plain `$name`.
    scalars: Vec<(String, String)>,
    /// Values of the trailing `variadic` arg, if any - they become the
    /// command's positional parameters (`$@`).
    variadic: Vec<String>,
    /// Name of the variadic arg (for its `GIANT_ARG_<NAME>` convenience
    /// binding), if one was declared.
    variadic_name: Option<String>,
}

/// Bind `positionals` (bare args after the task name) and `--arg
/// name=value` overrides against the task's declared `args:`. Positionals
/// fill the scalar args in order; a trailing `variadic` arg collects the
/// rest. `--arg` sets a named arg explicitly and conflicts with a
/// positional for the same arg. Applies defaults, enforces `choices`, and
/// errors on missing-required / too-many / unknown-name.
fn bind_args(
    spec: &TaskSpec,
    positionals: &[String],
    kvs: &[String],
) -> Result<BoundArgs, RunError> {
    let scalar_count = spec.args.iter().filter(|a| !a.variadic).count();

    // 1. Bind positionals to args in order; a variadic absorbs the tail.
    let mut values: Vec<Option<String>> = vec![None; spec.args.len()];
    let mut variadic = Vec::new();
    let mut pi = 0;
    for (i, arg) in spec.args.iter().enumerate() {
        if arg.variadic {
            variadic.extend(positionals[pi..].iter().cloned());
            pi = positionals.len();
            break; // variadic is validated to be last
        }
        if pi < positionals.len() {
            values[i] = Some(positionals[pi].clone());
            pi += 1;
        }
    }
    if pi < positionals.len() {
        return Err(RunError::BadArg {
            name: "<positional>".into(),
            detail: format!(
                "task takes {scalar_count} argument(s); got {} extra: {:?}",
                positionals.len() - scalar_count,
                &positionals[pi..],
            ),
        });
    }

    // 2. Apply `--arg name=value` overrides by name.
    for kv in kvs {
        let (k, v) = kv.split_once('=').ok_or_else(|| RunError::BadArg {
            name: kv.clone(),
            detail: "expected --arg name=value".into(),
        })?;
        let idx = spec
            .args
            .iter()
            .position(|a| a.name == k && !a.variadic)
            .ok_or_else(|| RunError::BadArg {
                name: k.into(),
                detail: "no such declared arg for this task".into(),
            })?;
        if values[idx].is_some() {
            return Err(RunError::BadArg {
                name: k.into(),
                detail: "set both positionally and via --arg".into(),
            });
        }
        values[idx] = Some(v.to_string());
    }

    // 3. Resolve defaults, required, and choices into the scalar set.
    let mut scalars = Vec::with_capacity(scalar_count);
    for (i, arg) in spec.args.iter().enumerate() {
        if arg.variadic {
            continue;
        }
        let val = match values[i].take().or_else(|| arg.default.clone()) {
            Some(v) => v,
            None => {
                return Err(RunError::BadArg {
                    name: arg.name.clone(),
                    detail: "required (no value supplied and no default)".into(),
                });
            }
        };
        if let Some(choices) = &arg.choices
            && !choices.contains(&val)
        {
            return Err(RunError::BadArg {
                name: arg.name.clone(),
                detail: format!("value {val:?} is not one of {choices:?}"),
            });
        }
        scalars.push((arg.name.clone(), val));
    }

    Ok(BoundArgs {
        scalars,
        variadic,
        variadic_name: spec
            .args
            .iter()
            .find(|a| a.variadic)
            .map(|a| a.name.clone()),
    })
}

async fn run_command(
    spec: &TaskSpec,
    bound: &BoundArgs,
    workspace_root: &Path,
    shutdown: Option<&Shutdown>,
) -> Result<u8, RunError> {
    let cwd = match &spec.cwd {
        Some(rel) => workspace_root.join(rel),
        None => workspace_root.to_path_buf(),
    };

    // Positional parameters ($@): the variadic arg's values.
    let positional: Vec<OsString> = bound.variadic.iter().map(OsString::from).collect();

    // Shebang body → exec a temp script; plain body → `sh -c`. The temp
    // file (if any) must outlive the child, so `_script` is held here.
    // Only reached for tasks that have a command (supervise-mode tasks
    // never run a command).
    let body = spec
        .command
        .as_deref()
        .expect("run_command requires a command");
    let (mut cmd, _script) = build_command(body, &positional)?;
    cmd.current_dir(&cwd);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    // Scalar args: `GIANT_ARG_<NAME>` (unambiguous) + plain `$name`.
    for (name, val) in &bound.scalars {
        cmd.env(format!("GIANT_ARG_{}", name.to_ascii_uppercase()), val);
        cmd.env(name, val);
    }
    // Variadic convenience binding: the values space-joined under its name.
    if let Some(vname) = &bound.variadic_name {
        let joined = bound.variadic.join(" ");
        cmd.env(format!("GIANT_ARG_{}", vname.to_ascii_uppercase()), &joined);
        cmd.env(vname, &joined);
    }
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    // When we're handling shutdown, give the command its own process
    // group so a forwarded signal reaches its whole subtree and the
    // terminal's group-SIGINT doesn't race us. Without shutdown handling
    // the child stays in our group (terminal Ctrl-C kills both, as before).
    let as_group = shutdown.is_some();
    if as_group {
        cmd.process_group(0);
    }

    render::running(&spec_label(spec));

    let mut child = cmd.spawn()?;

    // Race the command against a shutdown signal. `on_shutdown` pends
    // forever when there's no `Shutdown`, so that arm only fires in the
    // cleanup-bearing case. Killing happens *after* the select so the wait
    // future releases its borrow on `child` first.
    tokio::select! {
        waited = wait_with_timeout(&mut child, spec.timeout_secs) => match waited {
            Ok(Some(status)) => Ok(status.code().map(|c| c.clamp(0, 255) as u8).unwrap_or(1)),
            Ok(None) => {
                signals::terminate(&mut child, libc::SIGTERM, as_group).await;
                let secs = spec.timeout_secs.unwrap_or(0);
                render::note(&format!("task timed out after {secs}s"));
                Ok(124)
            }
            Err(e) => Err(e.into()),
        },
        sig = on_shutdown(shutdown) => {
            signals::terminate(&mut child, sig, as_group).await;
            render::note("interrupted - running cleanup");
            // Conventional 128 + signal number (SIGINT → 130, SIGTERM → 143).
            Ok(128u8.saturating_add(sig as u8))
        }
    }
}

/// Wait for the child, optionally bounded by a timeout. `Ok(None)` means
/// the timeout elapsed (the child is still running and unreaped).
async fn wait_with_timeout(
    child: &mut tokio::process::Child,
    timeout_secs: Option<u64>,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    match timeout_secs {
        Some(secs) => match tokio::time::timeout(Duration::from_secs(secs), child.wait()).await {
            Ok(status) => status.map(Some),
            Err(_) => Ok(None),
        },
        None => child.wait().await.map(Some),
    }
}

/// Await a shutdown signal's number, or pend forever when there's no
/// handler installed (so the caller's `select!` arm never fires).
async fn on_shutdown(shutdown: Option<&Shutdown>) -> i32 {
    match shutdown {
        Some(s) => s.recv().await,
        None => std::future::pending().await,
    }
}

/// Build the command for a task body. A body that begins with `#!` is a
/// script: write it to a temp file, make it executable, and exec it
/// directly with the positional args (the kernel honors the shebang), so
/// tasks can be written in any language. Otherwise run it under `sh -c`.
/// The returned tempfile (if any) must be kept alive until the command
/// finishes - its drop deletes the script.
fn build_command(
    body: &str,
    positional: &[OsString],
) -> Result<(Command, Option<tempfile::TempPath>), RunError> {
    if body.trim_start().starts_with("#!") {
        use std::io::Write;
        let mut tmp = tempfile::Builder::new().prefix("giant-task-").tempfile()?;
        tmp.write_all(body.as_bytes())?;
        tmp.flush()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755))?;
        }
        // Close the write handle (keeping the file on disk) before exec -
        // a file still open for writing can't be exec'd (ETXTBSY).
        let path = tmp.into_temp_path();
        let mut cmd = Command::new(&path);
        cmd.args(positional);
        Ok((cmd, Some(path)))
    } else {
        // Pass-through/variadic args become $1..$N inside `sh -c`.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(body).arg("--").args(positional);
        Ok((cmd, None))
    }
}

/// Short label for the "▶ <task>" header. Uses the description when
/// available; falls back to the command's first 60 chars.
fn spec_label(spec: &TaskSpec) -> String {
    if let Some(d) = &spec.description {
        return d.clone();
    }
    match spec.command.as_deref() {
        Some(c) if c.len() > 60 => {
            let mut s: String = c.chars().take(57).collect();
            s.push_str("...");
            s
        }
        Some(c) => c.to_string(),
        None => "(services)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ArgSpec;
    use std::collections::HashMap;

    fn task(args: Vec<ArgSpec>) -> TaskSpec {
        TaskSpec {
            command: Some("true".into()),
            description: None,
            deps: vec![],
            needs: vec![],
            services: vec![],
            finally: vec![],
            args,
            env: HashMap::new(),
            cwd: None,
            timeout_secs: None,
            inputs: vec![],
        }
    }

    fn arg(name: &str, default: Option<&str>) -> ArgSpec {
        ArgSpec {
            name: name.into(),
            default: default.map(Into::into),
            choices: None,
            variadic: false,
            description: None,
        }
    }

    fn scalar<'a>(b: &'a BoundArgs, name: &str) -> Option<&'a str> {
        b.scalars
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn positional_binds_by_order() {
        let s = task(vec![arg("env", None), arg("tag", Some("latest"))]);
        let b = bind_args(&s, &["prod".into(), "v2".into()], &[]).unwrap();
        assert_eq!(scalar(&b, "env"), Some("prod"));
        assert_eq!(scalar(&b, "tag"), Some("v2"));
    }

    #[test]
    fn default_applied_when_positional_omitted() {
        let s = task(vec![arg("env", None), arg("tag", Some("latest"))]);
        let b = bind_args(&s, &["prod".into()], &[]).unwrap();
        assert_eq!(scalar(&b, "tag"), Some("latest"));
    }

    #[test]
    fn required_missing_errors() {
        let s = task(vec![arg("env", None)]);
        let err = bind_args(&s, &[], &[]).unwrap_err();
        assert!(format!("{err}").contains("required"));
    }

    #[test]
    fn explicit_arg_sets_and_conflicts() {
        let s = task(vec![arg("env", Some("staging"))]);
        // `--arg` sets it when no positional is given.
        let b = bind_args(&s, &[], &["env=prod".into()]).unwrap();
        assert_eq!(scalar(&b, "env"), Some("prod"));
        // positional + `--arg` for the same arg → conflict.
        let err = bind_args(&s, &["prod".into()], &["env=stg".into()]).unwrap_err();
        assert!(format!("{err}").contains("both positionally and via --arg"));
    }

    #[test]
    fn choices_enforced() {
        let mut a = arg("env", Some("staging"));
        a.choices = Some(vec!["staging".into(), "prod".into()]);
        let s = task(vec![a]);
        let err = bind_args(&s, &["nope".into()], &[]).unwrap_err();
        assert!(format!("{err}").contains("not one of"));
    }

    #[test]
    fn variadic_collects_the_rest() {
        let mut v = arg("flags", None);
        v.variadic = true;
        let s = task(vec![arg("env", None), v]);
        let b = bind_args(&s, &["prod".into(), "a".into(), "b".into()], &[]).unwrap();
        assert_eq!(scalar(&b, "env"), Some("prod"));
        assert_eq!(b.variadic, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(b.variadic_name.as_deref(), Some("flags"));
    }

    #[test]
    fn too_many_positionals_without_variadic_errors() {
        let s = task(vec![arg("env", None)]);
        let err = bind_args(&s, &["a".into(), "b".into()], &[]).unwrap_err();
        assert!(format!("{err}").contains("extra"));
    }

    #[test]
    fn unknown_arg_name_errors() {
        let s = task(vec![]);
        let err = bind_args(&s, &[], &["nope=1".into()]).unwrap_err();
        assert!(format!("{err}").contains("no such declared arg"));
    }

    #[test]
    fn malformed_kv_errors() {
        let s = task(vec![]);
        let err = bind_args(&s, &[], &["nosep".into()]).unwrap_err();
        assert!(format!("{err}").contains("--arg name=value"));
    }
}
