//! `giant-task --watch` - re-run a task when relevant files change.
//!
//! The porcelain does no file watching itself. It spawns a `giant
//! session` (the headless engine) and subscribes to a
//! notify-only watch scoped to the task's `deps:` (as target ids, which
//! the engine expands through the graph - so a change to a *transitive*
//! dependency counts) and `inputs:` (as extra globs). On each
//! `watch.changed` the task re-runs.
//!
//! The engine owns the watcher, the debouncer, and the graph; the
//! porcelain only declares what to watch and reacts. Ctrl-C (or stdin
//! EOF) ends the loop and drains the session.

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;

use giant::commands::Command as EngineCommand;
use giant::events::Event;
use giant::model::TargetId;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};

use crate::config::{Task, TaskConfig};
use crate::deps::GIANT_BIN_ENV;
use crate::runner;

pub async fn loop_forever(
    mut cfg: TaskConfig,
    explicit: Option<&Path>,
    label: &str,
    inv: runner::Invocation<'_>,
    verbose: bool,
) -> anyhow::Result<u8> {
    let mut task = cfg
        .tasks
        .get(label)
        .ok_or_else(|| anyhow::anyhow!("unknown task: {label}"))?
        .clone();

    // Run once up front, like the old watcher did.
    println!("· initial run");
    let _ = runner::run(&cfg, label, inv, verbose).await;

    // Spawn the engine session: it loads config + discovery once, then
    // streams events. We hold its stdin to send the subscribe command.
    let bin = std::env::var_os(GIANT_BIN_ENV).unwrap_or_else(|| OsString::from("giant"));
    let mut child = Command::new(&bin)
        .args(["session", "--events", "ndjson"])
        .current_dir(&cfg.workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            anyhow::anyhow!(
                "could not start `giant session` ({e}). Watch mode needs the \
                 engine - ensure `giant` is on PATH or set {GIANT_BIN_ENV}."
            )
        })?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let mut events = BufReader::new(stdout).lines();

    println!("· watching via engine - Ctrl-C to exit");

    // Note: while a re-run (`runner::run`) is in flight we aren't reading
    // the session's stdout. `watch.changed` events are tiny and debounced,
    // so the pipe won't fill in practice - but a large `catalog.ready`
    // re-emit during a long re-run could. If that ever bites, pump events
    // through a background reader task instead of reading inline.
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            line = events.next_line() => {
                let Some(line) = line? else { break }; // session closed stdout
                let Some(event) = parse_event(&line) else { continue };
                match event {
                    Event::EngineReady => {
                        subscribe(&mut stdin, &task).await?;
                    }
                    Event::WatchChanged { paths } => {
                        announce(&paths);
                        let _ = runner::run(&cfg, label, inv, verbose).await;
                    }
                    Event::CatalogReady => {
                        // Config / discovery changed. Reload our task config so
                        // future runs use it. But `catalog.ready` also fires on
                        // discovery-output churn unrelated to this task - only
                        // re-subscribe + re-run when the *watch scope* (deps or
                        // inputs) actually moved, so we don't rerun on noise.
                        match reload(explicit, label) {
                            Ok((fresh_cfg, fresh_task)) => {
                                let scope_moved = fresh_task.spec.deps != task.spec.deps
                                    || fresh_task.spec.inputs != task.spec.inputs;
                                cfg = fresh_cfg;
                                task = fresh_task;
                                if scope_moved {
                                    subscribe(&mut stdin, &task).await?;
                                    println!("· deps/inputs changed, re-running");
                                    let _ = runner::run(&cfg, label, inv, verbose).await;
                                }
                            }
                            Err(e) => {
                                eprintln!("· config reload failed: {e}");
                                break;
                            }
                        }
                    }
                    Event::EngineShutdown { error, .. } => {
                        if let Some(e) = error {
                            eprintln!("· engine stopped: {e}");
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    // Closing stdin tells the session to drain and exit. We must keep
    // reading its stdout while it does: once we stop, its event writer
    // blocks on a full pipe, never sees the stdin-EOF, and never exits -
    // a deadlock on `child.wait()`. Drain to EOF, then reap. Bounded so a
    // wedged session can't hang us; SIGKILL as a backstop.
    drop(stdin);
    let drained = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while matches!(events.next_line().await, Ok(Some(_))) {}
        child.wait().await
    })
    .await;
    if drained.is_err() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    Ok(0)
}

/// Reload the workspace config, returning the fresh config and the task
/// (by label). Errors if the task vanished from the reloaded config.
fn reload(explicit: Option<&Path>, label: &str) -> anyhow::Result<(TaskConfig, Task)> {
    let cfg = TaskConfig::scan(explicit)?;
    let task = cfg
        .tasks
        .get(label)
        .ok_or_else(|| anyhow::anyhow!("task '{label}' no longer exists after reload"))?
        .clone();
    Ok((cfg, task))
}

/// Send `watch.subscribe { targets: deps, globs: inputs }`. The engine
/// expands the targets through the graph; the globs cover files no
/// target owns (e2e sources, fixtures).
async fn subscribe(stdin: &mut ChildStdin, task: &Task) -> anyhow::Result<()> {
    let cmd = EngineCommand::WatchSubscribe {
        command_id: None,
        targets: task.spec.deps.iter().map(TargetId::new).collect(),
        globs: task.spec.inputs.clone(),
    };
    let mut line = serde_json::to_vec(&cmd)?;
    line.push(b'\n');
    stdin.write_all(&line).await?;
    stdin.flush().await?;
    Ok(())
}

/// Parse one NDJSON event line, tolerating blanks and the odd non-event
/// line rather than bricking the loop.
fn parse_event(line: &str) -> Option<Event> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

fn announce(paths: &[String]) {
    println!();
    match paths {
        [] => println!("· change detected, re-running"),
        [one] => println!("· {one} changed, re-running"),
        [first, rest @ ..] => {
            println!("· {} changed (+{}), re-running", first, rest.len());
        }
    }
}
