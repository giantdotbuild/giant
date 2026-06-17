//! Service supervisor for giant-task.
//!
//! Wraps `tokio-process-tools` for the cross-platform tricky bits
//! (signal escalation, broadcast output streams) and adds our own
//! policy: spawn → optional exec-based readiness probe → run task →
//! stop. No restart, no in-supervisor dependency graph, no log
//! rotation - that's process-compose's territory if users need it.
//!
//! Logs from each service are streamed to giant-task's stdout/stderr
//! with a `[name]` prefix in the service's hashed color. Lifetime of
//! the inspector handles is tied to the `RunningService` they
//! belong to.

use crate::config::{Service, TaskConfig, package_cwd, resolve_ref};
use crate::render;
use crate::schema::ReadyProbe;
use anstyle::{AnsiColor, Color, Style};
use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio_process_tools::broadcast::BroadcastOutputStream;
use tokio_process_tools::{Inspector, LineParsingOptions, Next, ProcessHandle, RunningState};

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("failed to spawn service '{name}': {source}")]
    Spawn {
        name: String,
        #[source]
        source: std::io::Error,
    },

    #[error("service '{name}' didn't become ready within {timeout_secs}s")]
    NotReady { name: String, timeout_secs: u64 },

    #[error("service '{name}' exited before becoming ready (status: {status})")]
    ExitedEarly { name: String, status: String },

    #[error("supervisor error: {detail}")]
    Internal { detail: String },
}

/// One running service. The Inspectors keep the per-line callbacks
/// alive; dropping them stops the log streaming. Inspector itself
/// doesn't impl Debug, so we hand-roll a sparse one.
pub struct RunningService {
    pub name: String,
    pub handle: ProcessHandle<BroadcastOutputStream>,
    _stdout_inspector: Inspector,
    _stderr_inspector: Inspector,
}

impl std::fmt::Debug for RunningService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningService")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Start a single service: spawn, wire up log prefixing, optionally
/// poll the readiness probe. Returns once the service is ready (or
/// errors).
pub async fn start(
    label: &str,
    svc: &Service,
    workspace_root: &Path,
) -> Result<RunningService, ServiceError> {
    let spec = &svc.spec;
    // Default cwd is the service's package directory.
    let cwd = package_cwd(workspace_root, &svc.package, spec.cwd.as_deref());

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&spec.command);
    cmd.current_dir(&cwd);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    // Inherit a few useful vars; rest comes from the parent's env via
    // tokio's default behaviour (it doesn't clear).
    cmd.kill_on_drop(false);

    let mut handle = ProcessHandle::<BroadcastOutputStream>::spawn(label.to_string(), cmd)
        .map_err(|source| ServiceError::Spawn {
            name: label.to_string(),
            source,
        })?;

    let prefix_for_stdout = colored_prefix(&svc.name);
    let prefix_for_stderr = prefix_for_stdout.clone();
    let stdout_inspector = handle.stdout().inspect_lines(
        move |line| {
            println!("{prefix_for_stdout} {line}");
            Next::Continue
        },
        LineParsingOptions::default(),
    );
    let stderr_inspector = handle.stderr().inspect_lines(
        move |line| {
            eprintln!("{prefix_for_stderr} {line}");
            Next::Continue
        },
        LineParsingOptions::default(),
    );

    if let Some(probe) = &spec.ready
        && let Err(e) = wait_ready(label, &mut handle, probe, &cwd).await
    {
        // The probe timed out or the service exited prematurely - we
        // need to clean up the handle so its on-drop assertion
        // doesn't panic. Best-effort terminate, then propagate the
        // original error.
        let _ = handle
            .terminate(Duration::from_secs(2), Duration::from_secs(2))
            .await;
        return Err(e);
    }

    Ok(RunningService {
        name: svc.name.clone(),
        handle,
        _stdout_inspector: stdout_inspector,
        _stderr_inspector: stderr_inspector,
    })
}

/// Start `names` plus everything they transitively `need`, in dependency
/// order: a service starts only once all its `needs:` have passed their
/// ready probe. Services with satisfied needs start concurrently. Returns
/// the started services in start order (so the caller can stop them - and
/// roll them back on failure - sensibly).
pub async fn start_group(
    cfg: &TaskConfig,
    refs: &[String],
    from_pkg: &str,
) -> Result<Vec<RunningService>, (Vec<RunningService>, ServiceError)> {
    let services = &cfg.services;
    let workspace_root = cfg.workspace_root.as_path();

    // The group is the listed services plus their transitive `needs`, all
    // as labels (each reference resolved within its declaring package).
    let mut group: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = refs.iter().map(|r| resolve_ref(r, from_pkg)).collect();
    while let Some(lbl) = stack.pop() {
        if !seen.insert(lbl.clone()) {
            continue;
        }
        if let Some(svc) = services.get(&lbl) {
            stack.extend(svc.spec.needs.iter().map(|n| resolve_ref(n, &svc.package)));
        }
        group.push(lbl);
    }
    render::note(&format!("starting services: {}", group.join(", ")));

    let mut started: Vec<RunningService> = Vec::new();
    let mut ready: HashSet<String> = HashSet::new();

    while started.len() < group.len() {
        // Services not yet started whose needs are all ready.
        let eligible: Vec<String> = group
            .iter()
            .filter(|lbl| !ready.contains(*lbl))
            .filter(|lbl| {
                services.get(*lbl).is_some_and(|s| {
                    s.spec
                        .needs
                        .iter()
                        .all(|d| ready.contains(&resolve_ref(d, &s.package)))
                })
            })
            .cloned()
            .collect();
        if eligible.is_empty() {
            // Acyclicity is validated at config load, so this is a
            // belt-and-braces guard, never expected to fire.
            return Err((
                started,
                ServiceError::Internal {
                    detail: "service `needs:` cannot be satisfied".into(),
                },
            ));
        }

        // Start this level concurrently; each `start` returns once ready.
        let futures = eligible.into_iter().map(|label| {
            let svc = services.get(&label).expect("in group → defined").clone();
            let root = workspace_root.to_path_buf();
            tokio::spawn(async move { (label.clone(), start(&label, &svc, &root).await) })
        });
        let results = futures_util::future::join_all(futures).await;

        let mut failed = None;
        for r in results {
            match r {
                Ok((name, Ok(svc))) => {
                    ready.insert(name);
                    started.push(svc);
                }
                Ok((_, Err(e))) => {
                    failed.get_or_insert(e);
                }
                Err(join_err) => {
                    failed.get_or_insert(ServiceError::Internal {
                        detail: format!("a service start task panicked: {join_err}"),
                    });
                }
            }
        }
        if let Some(e) = failed {
            return Err((started, e));
        }
    }
    Ok(started)
}

/// Stop all running services in parallel. Each gets a SIGINT, then
/// SIGTERM after `interrupt_timeout`, then SIGKILL after another
/// `terminate_timeout` if it still hasn't exited. Best-effort: errors
/// are logged but don't propagate (we're shutting down anyway).
pub async fn stop_all(services: Vec<RunningService>) {
    const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(2);
    const TERMINATE_TIMEOUT: Duration = Duration::from_secs(3);
    let futures = services.into_iter().map(|mut svc| async move {
        let _ = svc
            .handle
            .terminate(INTERRUPT_TIMEOUT, TERMINATE_TIMEOUT)
            .await;
    });
    futures_util::future::join_all(futures).await;
}

/// Wait until any running service exits, returning its name. Foreground
/// supervise mode uses this to fall out of its hold when a service dies.
/// Event-driven (races each service's completion future), not polled.
pub async fn wait_any_exit(running: &mut [RunningService]) -> String {
    if running.is_empty() {
        // No services to watch - hold forever; the caller's Ctrl-C arm wins.
        return std::future::pending().await;
    }
    let waits = running.iter_mut().map(|svc| {
        Box::pin(async move {
            let _ = svc.handle.wait_for_completion(None).await;
            svc.name.clone()
        })
    });
    let (name, _, _) = futures_util::future::select_all(waits).await;
    name
}

/// Poll the ready probe until it returns 0 (success) or the timeout
/// elapses. Also bails fast if the service itself exits before
/// becoming ready.
async fn wait_ready(
    name: &str,
    handle: &mut ProcessHandle<BroadcastOutputStream>,
    probe: &ReadyProbe,
    cwd: &Path,
) -> Result<(), ServiceError> {
    let deadline = Instant::now() + Duration::from_secs(probe.timeout_secs);
    let period = Duration::from_millis(probe.period_secs.saturating_mul(1000));
    let probe_cmd = probe.command.clone();
    let cwd = cwd.to_path_buf();

    loop {
        match handle.is_running() {
            RunningState::Running | RunningState::Uncertain(_) => {}
            RunningState::Terminated(status) => {
                return Err(ServiceError::ExitedEarly {
                    name: name.to_string(),
                    status: format!("{status}"),
                });
            }
        }

        if run_probe(&probe_cmd, &cwd).await {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(ServiceError::NotReady {
                name: name.to_string(),
                timeout_secs: probe.timeout_secs,
            });
        }

        tokio::time::sleep(period).await;
    }
}

async fn run_probe(probe_cmd: &str, cwd: &Path) -> bool {
    let status = Command::new("sh")
        .arg("-c")
        .arg(probe_cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    matches!(status, Ok(s) if s.success())
}

/// Per-service line prefix in a stable hashed color (same palette as
/// the core renderer uses for target log lines). Colors off on
/// non-tty / NO_COLOR.
fn colored_prefix(name: &str) -> String {
    let bare = format!("[{name}]");
    if std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        let style = palette_color(name);
        format!("{style}{bare}{style:#}")
    } else {
        bare
    }
}

fn palette_color(name: &str) -> Style {
    let palette = [
        AnsiColor::Cyan,
        AnsiColor::Magenta,
        AnsiColor::Blue,
        AnsiColor::Yellow,
        AnsiColor::BrightCyan,
        AnsiColor::BrightMagenta,
        AnsiColor::BrightBlue,
        AnsiColor::BrightYellow,
    ];
    let h = fnv1a(name.as_bytes()) as usize;
    Style::new().fg_color(Some(Color::Ansi(palette[h % palette.len()])))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ServiceSpec;
    use std::collections::HashMap;

    fn service(name: &str, command: &str, ready: Option<ReadyProbe>) -> Service {
        Service {
            package: String::new(),
            name: name.into(),
            spec: ServiceSpec {
                command: command.into(),
                description: None,
                deps: vec![],
                needs: vec![],
                ready,
                env: HashMap::new(),
                cwd: None,
            },
        }
    }

    #[tokio::test]
    async fn service_with_no_ready_probe_starts_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let svc = start(
            "//:echo",
            // sleep keeps the child alive long enough to terminate cleanly.
            &service("echo", "exec sleep 30", None),
            dir.path(),
        )
        .await
        .expect("spawn should succeed");
        // Stop it.
        stop_all(vec![svc]).await;
    }

    #[tokio::test]
    async fn ready_probe_succeeds_when_marker_appears() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ready");
        let marker_str = marker.display().to_string();
        let cmd = format!(
            "sleep 0.2 && touch {} && exec sleep 30",
            shell_quote(&marker_str)
        );
        let probe = ReadyProbe {
            command: format!("test -f {}", shell_quote(&marker_str)),
            period_secs: 1,
            timeout_secs: 5,
        };
        let svc = start("//:db", &service("db", &cmd, Some(probe)), dir.path())
            .await
            .expect("ready probe should pass");
        // Once ready, our marker must exist.
        assert!(marker.exists());
        stop_all(vec![svc]).await;
    }

    #[tokio::test]
    async fn ready_probe_times_out_when_marker_never_appears() {
        let dir = tempfile::tempdir().unwrap();
        // Service runs but never creates the file we're probing for.
        let probe = ReadyProbe {
            command: "test -f /does-not-exist/nope".into(),
            period_secs: 1,
            timeout_secs: 1,
        };
        let err = start(
            "//:never-ready",
            &service("never-ready", "exec sleep 30", Some(probe)),
            dir.path(),
        )
        .await
        .expect_err("should have timed out");
        assert!(matches!(err, ServiceError::NotReady { .. }), "got: {err:?}");
    }

    #[tokio::test]
    async fn early_exit_fails_with_exited_early() {
        let dir = tempfile::tempdir().unwrap();
        let probe = ReadyProbe {
            command: "false".into(),
            period_secs: 1,
            timeout_secs: 5,
        };
        // The "service" exits immediately with a non-zero status.
        let err = start(
            "//:quitter",
            &service("quitter", "exit 7", Some(probe)),
            dir.path(),
        )
        .await
        .expect_err("should have detected early exit");
        assert!(
            matches!(err, ServiceError::ExitedEarly { .. }),
            "got: {err:?}"
        );
    }

    fn shell_quote(s: &str) -> String {
        // Tests only use temp-dir paths with no shell-special chars.
        s.to_string()
    }
}
