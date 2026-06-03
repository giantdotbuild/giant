//! Running a generator: command resolution and the invocation contract
//! (TDD-0022 §C). cwd is the workspace root; `GIANT_GEN_OUT` is the output
//! root; `GIANT_WORKSPACE` is the workspace root for generators that prefer
//! not to rely on cwd.

use crate::config::Generator;
use anyhow::Result;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

/// Build the command for a generator, applying the invocation contract.
/// Resolution: a value with whitespace runs via `sh -c` (so `giant task foo`
/// or a command with args works); a value with a `/` is a path resolved from
/// the workspace root; a bare name resolves on `PATH` (the `giant-gen-<name>`
/// default).
pub fn command(g: &Generator, workspace_root: &Path, out_root: &Path) -> Command {
    let mut cmd = if g.command.contains(char::is_whitespace) {
        let mut c = Command::new("sh");
        c.arg("-c").arg(&g.command);
        c
    } else if g.command.contains('/') {
        Command::new(workspace_root.join(&g.command))
    } else {
        Command::new(&g.command)
    };
    cmd.current_dir(workspace_root)
        .env("GIANT_GEN_OUT", out_root)
        .env("GIANT_WORKSPACE", workspace_root)
        .stdin(Stdio::null());
    cmd
}

/// Run a generator with its output streamed live (used by `giant gen`).
/// Returns whether it succeeded; resolution and exit failures are reported to
/// stderr here so the caller only tallies them.
pub async fn run_live(g: &Generator, workspace_root: &Path, out_root: &Path) -> Result<bool> {
    let mut cmd = command(g, workspace_root, out_root);
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    match cmd.status().await {
        Ok(status) => {
            if !status.success() {
                eprintln!("giant gen: generator '{}' failed ({status})", g.name);
            }
            Ok(status.success())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "giant gen: generator '{}': command '{}' not found on PATH",
                g.name, g.command
            );
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}
