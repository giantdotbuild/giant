//! giant-logs - replay the captured stdout/stderr from the last cached
//! invocation of a target. Answer "what did the build say?" without busting
//! the cache.
//!
//! Porcelain (ADR-0034), dispatched as `giant logs`. It does not read the cache
//! itself: it asks a `giant session` over the protocol (`logs.get`) and writes
//! the replayed `logs.line` events back out, honoring the stream filters.

use std::io::Write;

use anyhow::Result;
use clap::Parser;
use giant::commands::Command;
use giant::events::{Event, LogStream};

#[derive(Parser, Debug)]
#[command(
    name = "giant-logs",
    about = "Replay a target's captured logs from the last cached build"
)]
struct Cli {
    /// Target ID to show logs for.
    target: String,

    /// Inspect a specific historical AC entry by its cache-key hex. Defaults to
    /// the target's current cache key.
    #[arg(long, value_name = "HEX")]
    key: Option<String>,

    /// Print stdout only.
    #[arg(long, conflicts_with_all = ["stderr_only", "merged"])]
    stdout_only: bool,

    /// Print stderr only.
    #[arg(long, conflicts_with_all = ["stdout_only", "merged"])]
    stderr_only: bool,

    /// Merge stdout + stderr to the current stdout, in stdout-then-stderr order.
    #[arg(long)]
    merged: bool,

    /// Path to giant.yaml (defaults to walking up from the current directory).
    #[arg(long)]
    config: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("giant logs: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    let cli = Cli::parse();

    let command = Command::LogsGet {
        command_id: Some("L1".into()),
        target: giant::TargetId::new(&cli.target),
        follow: false,
        key: cli.key.clone(),
    };
    let events = giant::query_session(
        cli.config.as_deref(),
        command,
        |e| matches!(e, Event::LogsEnd { command_id, .. } if command_id.as_deref() == Some("L1")),
    )
    .await?;

    let want_stdout = !cli.stderr_only;
    let want_stderr = !cli.stdout_only;

    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let mut wrote = false;

    for event in events {
        let Event::LogsLine {
            command_id,
            stream,
            line,
            ..
        } = event
        else {
            continue;
        };
        if command_id.as_deref() != Some("L1") {
            continue;
        }
        match stream {
            LogStream::Stdout if want_stdout => {
                let _ = writeln!(out, "{line}");
                wrote = true;
            }
            LogStream::Stderr if want_stderr => {
                // --merged folds stderr into stdout, in arrival order (the
                // session replays the stdout blob before the stderr blob).
                if cli.merged {
                    let _ = writeln!(out, "{line}");
                } else {
                    let _ = writeln!(err, "{line}");
                }
                wrote = true;
            }
            _ => {}
        }
    }

    if !wrote {
        eprintln!(
            "no captured logs for {} \
             (run the target first, or check cache.capture_logs is on)",
            cli.target
        );
    }
    Ok(())
}
