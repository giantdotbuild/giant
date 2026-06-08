//! A small client for the `giant session` NDJSON protocol: spawn the engine,
//! send one correlated command, collect the reply events.
//!
//! This is what the read-query porcelains (`giant explain`, `giant logs`) use
//! instead of recomputing anything themselves. Core does the work and emits
//! events; the porcelain renders them (ADR-0034). The same protocol feeds the
//! TUI, and a future warm daemon answers without a per-call graph rebuild.

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command as TokioCommand;

use crate::commands::Command;
use crate::events::Event;

/// Env override for the giant binary the client spawns. Lets tests point at a
/// freshly built binary instead of whatever `giant` is on PATH.
const GIANT_BIN_ENV: &str = "GIANT_BIN";

/// Spawn `giant session`, send `command` (which must carry a `command_id`), and
/// collect every event up to and including the first one matching `is_terminal`.
///
/// The returned vec includes the catalog stream and any other traffic the
/// session emitted in the meantime; callers filter for the reply variant they
/// care about. A `command.rejected` / `command.error` carrying our `command_id`
/// aborts with that reason, and an early EOF is an error.
pub async fn query_session(
    config: Option<&Path>,
    command: Command,
    is_terminal: impl Fn(&Event) -> bool,
) -> Result<Vec<Event>> {
    let our_id = command.command_id().map(str::to_owned);

    let bin = std::env::var_os(GIANT_BIN_ENV).unwrap_or_else(|| OsString::from("giant"));
    let mut spawn = TokioCommand::new(&bin);
    if let Some(p) = config {
        spawn.arg("--config").arg(p);
    }
    spawn.args(["session", "--events", "ndjson"]);
    let mut child = spawn
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning `{} session`", bin.to_string_lossy()))?;

    let mut stdin = child.stdin.take().expect("stdin is piped");
    let mut lines = BufReader::new(child.stdout.take().expect("stdout is piped")).lines();

    // One JSON object per line. The session buffers stdin until its dispatch
    // loop starts (after the catalog), so sending now is safe.
    let mut buf = serde_json::to_vec(&command).context("serializing command")?;
    buf.push(b'\n');
    stdin.write_all(&buf).await.context("writing command")?;
    let _ = stdin.flush().await;

    let mut collected = Vec::new();
    let outcome = loop {
        match lines.next_line().await.context("reading session output")? {
            None => break Err(anyhow!("session closed before replying")),
            Some(line) => {
                let Ok(event) = serde_json::from_str::<Event>(line.trim()) else {
                    continue; // tolerate any non-event lines
                };
                match &event {
                    Event::CommandRejected { command_id, reason }
                        if Some(command_id.as_str()) == our_id.as_deref() =>
                    {
                        break Err(anyhow!("{reason}"));
                    }
                    Event::CommandError {
                        command_id,
                        message,
                    } if Some(command_id.as_str()) == our_id.as_deref() => {
                        break Err(anyhow!("{message}"));
                    }
                    _ => {}
                }
                let done = is_terminal(&event);
                collected.push(event);
                if done {
                    break Ok(collected);
                }
            }
        }
    };

    // Closing stdin is the EOF the session shuts down on. Best-effort reap.
    drop(stdin);
    let _ = child.wait().await;
    outcome
}
