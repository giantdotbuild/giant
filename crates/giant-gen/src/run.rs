//! Producing a generator's output: the built-in Starlark host in-process
//! (ADR-0029 §4), or an external command under the invocation contract
//! (TDD-0022 §C - cwd is the workspace root, `GIANT_GEN_OUT` the output root,
//! `GIANT_WORKSPACE` the root for generators that prefer not to rely on cwd).

use crate::config::Generator;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// Build an external generator's command. A value with whitespace runs via
/// `sh -c`; a value with a `/` is a path resolved from the workspace root; a
/// bare name resolves on `PATH` (the `giant-gen-<name>` default).
fn external_command(command: &str, workspace_root: &Path, out_root: &Path) -> Command {
    let mut cmd = if command.contains(char::is_whitespace) {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    } else if command.contains('/') {
        Command::new(workspace_root.join(command))
    } else {
        Command::new(command)
    };
    cmd.current_dir(workspace_root)
        .env("GIANT_GEN_OUT", out_root)
        .env("GIANT_WORKSPACE", workspace_root)
        .stdin(Stdio::null());
    cmd
}

/// Run the built-in Starlark host on a blocking thread (it does filesystem and
/// subprocess I/O). `script` is resolved from the workspace root.
async fn run_builtin(
    script: &Path,
    infix: &str,
    out_root: &Path,
    workspace_root: &Path,
) -> Result<Vec<PathBuf>> {
    let script = workspace_root.join(script);
    let infix = infix.to_string();
    let out_root = out_root.to_path_buf();
    let root = workspace_root.to_path_buf();
    tokio::task::spawn_blocking(move || crate::star::run(&script, &infix, &out_root, &root)).await?
}

/// Run a generator with its output streamed live (used by `giant gen`).
/// Returns whether it succeeded; failures are reported to stderr here so the
/// caller only tallies them.
pub async fn run_live(g: &Generator, workspace_root: &Path, out_root: &Path) -> Result<bool> {
    match g {
        Generator::Builtin { infix, script } => {
            match run_builtin(script, infix, out_root, workspace_root).await {
                Ok(paths) => {
                    println!("{infix}\tgenerated {} file(s)", paths.len());
                    Ok(true)
                }
                Err(e) => {
                    eprintln!("giant gen: {infix}:\n{e:#}");
                    Ok(false)
                }
            }
        }
        Generator::External { name, command } => {
            let mut cmd = external_command(command, workspace_root, out_root);
            cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
            match cmd.status().await {
                Ok(status) => {
                    if !status.success() {
                        eprintln!("giant gen: generator '{name}' failed ({status})");
                    }
                    Ok(status.success())
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!(
                        "giant gen: generator '{name}': command '{command}' not found on PATH"
                    );
                    Ok(false)
                }
                Err(e) => Err(e.into()),
            }
        }
    }
}

/// Whether producing into `out_root` ran cleanly, capturing the failure
/// message for the `--check` report.
pub enum Produced {
    Ran,
    Failed(String),
}

/// Produce a generator's output into `out_root` quietly, capturing any failure
/// (used by `giant gen --check`).
pub async fn produce_quiet(
    g: &Generator,
    workspace_root: &Path,
    out_root: &Path,
) -> Result<Produced> {
    match g {
        Generator::Builtin { infix, script } => {
            match run_builtin(script, infix, out_root, workspace_root).await {
                Ok(_) => Ok(Produced::Ran),
                Err(e) => Ok(Produced::Failed(format!("{e:#}"))),
            }
        }
        Generator::External { command, .. } => {
            let mut cmd = external_command(command, workspace_root, out_root);
            cmd.stdout(Stdio::null()).stderr(Stdio::piped());
            match cmd.output().await {
                Ok(o) if !o.status.success() => Ok(Produced::Failed(
                    String::from_utf8_lossy(&o.stderr).trim().to_string(),
                )),
                Ok(_) => Ok(Produced::Ran),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Produced::Failed(
                    format!("command '{command}' not found on PATH"),
                )),
                Err(e) => Err(e.into()),
            }
        }
    }
}
