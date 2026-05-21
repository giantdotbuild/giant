//! `giant-tui` entry point. Dispatched by `giant tui` (porcelain).
//!
//! Owns a single `giant session` subprocess for the TUI's lifetime.
//! Reads NDJSON events from its stdout, writes NDJSON commands to
//! its stdin. The state machine in `state.rs` and the layout in
//! `ui.rs` do the rest. See TDD-0013.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::ExecutableCommand;
use crossterm::event::{Event as CtEvent, EventStream, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use giant::commands::Command;
use giant::events::Event;
use giant_tui::keys::{Action, handle};
use giant_tui::state::{Screen, State};
use giant_tui::ui;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command as TokioCommand};
use tokio::sync::mpsc;

const GIANT_BIN_ENV: &str = "GIANT_TUI_BUILD_BIN";
const REDRAW_INTERVAL: Duration = Duration::from_millis(33); // ~30 Hz cap
/// How long to wait for the session to drain after we ask it to
/// shut down. The session just needs to close files and exit, so
/// this should be near-instant; if it isn't we kill the child.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

#[derive(Parser, Debug)]
#[command(
    name = "giant-tui",
    about = "Interactive target browser + build runner.",
    trailing_var_arg = true,
    disable_help_subcommand = true
)]
struct Cli {
    /// Selection patterns to pre-seed the search filter. Empty = the
    /// browser opens on the full catalog.
    #[arg(value_name = "PATTERN")]
    patterns: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    if !std::io::stdout().is_terminal() {
        return passthrough(&cli.patterns).await;
    }

    match run(&cli.patterns).await {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            let _ = restore_terminal();
            eprintln!("giant-tui: {e:#}");
            ExitCode::from(127)
        }
    }
}

async fn run(initial_patterns: &[String]) -> Result<i32> {
    // ---- Spawn the engine session ---------------------------------
    let bin = std::env::var_os(GIANT_BIN_ENV).unwrap_or_else(|| OsString::from("giant"));
    let mut child = TokioCommand::new(&bin)
        .args(["session", "--events", "ndjson"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning `{} session`", bin.to_string_lossy()))?;

    let stdout = child.stdout.take().expect("stdout is piped - take is safe");
    let stdin = child.stdin.take().expect("stdin is piped - take is safe");

    // ---- Set up the terminal --------------------------------------
    enable_raw_mode().context("could not enable raw mode")?;
    let mut stdout_w = std::io::stdout();
    stdout_w.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout_w);
    let mut terminal = Terminal::new(backend).context("could not init ratatui terminal")?;

    // ---- Wire up the channels -------------------------------------
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(2048);
    let (key_tx, mut key_rx) = mpsc::channel::<CtEvent>(64);
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(64);

    tokio::spawn(pump_events(stdout, event_tx));
    tokio::spawn(read_keys(key_tx));
    tokio::spawn(write_commands(stdin, cmd_rx));

    let mut state = State::default();
    if !initial_patterns.is_empty() {
        state.filters.search = initial_patterns.join(",");
    }

    // ---- Main loop ------------------------------------------------
    let mut render_pending = true;
    let mut ticker = tokio::time::interval(REDRAW_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut user_quit = false;

    loop {
        tokio::select! {
            biased;
            ct = key_rx.recv() => {
                let Some(ct) = ct else { break; };
                if let CtEvent::Key(k) = ct {
                    if k.kind != KeyEventKind::Press { continue; }
                    match handle(&mut state, k) {
                        Action::Redraw => render_pending = true,
                        Action::Quit => {
                            user_quit = true;
                            break;
                        }
                        Action::StartBuild => {
                            let sel = state.selection_for_build();
                            if sel.is_empty() {
                                state.last_error = Some("no targets in current selection".into());
                                render_pending = true;
                            } else {
                                state.start_build_locally();
                                let _ = cmd_tx.send(Command::Build {
                                    command_id: Some(format!("c_{}", new_command_seq())),
                                    targets: sel,
                                    fresh: false,
                                }).await;
                                render_pending = true;
                            }
                        }
                        Action::CancelChild => {
                            if let Some(build_id) = state.pending_build_id.clone()
                                && matches!(state.screen, Screen::Building | Screen::BuildFinished)
                            {
                                let _ = cmd_tx.send(Command::Cancel {
                                    command_id: Some(format!("c_{}", new_command_seq())),
                                    build: build_id,
                                }).await;
                            }
                            render_pending = true;
                        }
                        Action::Ignore => {}
                    }
                } else if matches!(ct, CtEvent::Resize(_, _)) {
                    render_pending = true;
                }
            }
            ev = event_rx.recv() => {
                match ev {
                    Some(ev) => {
                        let significant = matches!(
                            ev,
                            Event::EngineReady
                                | Event::BuildStarted { .. }
                                | Event::BuildFinished { .. }
                                | Event::TargetStarted { .. }
                                | Event::TargetFinished { .. }
                                | Event::CommandRejected { .. }
                                | Event::CommandError { .. }
                                | Event::CatalogReady
                        );
                        state.apply(ev);
                        if significant {
                            terminal.draw(|f| ui::draw(f, &state))?;
                            render_pending = false;
                        } else {
                            render_pending = true;
                        }
                    }
                    None => {
                        // Session ended. Render one last frame, wait
                        // briefly for a keypress, then exit.
                        terminal.draw(|f| ui::draw(f, &state))?;
                        let _ = tokio::time::timeout(
                            Duration::from_secs(3),
                            key_rx.recv(),
                        )
                        .await;
                        break;
                    }
                }
            }
            _ = ticker.tick(), if render_pending || is_time_dependent(&state) => {
                terminal.draw(|f| ui::draw(f, &state))?;
                render_pending = false;
            }
        }
    }

    // ---- Shutdown -------------------------------------------------
    //
    // Render one frame with the "quitting…" overlay so the user sees
    // immediate feedback while the session drains. Then restore the
    // terminal and wait for the child.
    if user_quit {
        state.quitting = true;
        let _ = terminal.draw(|f| ui::draw(f, &state));
        let _ = cmd_tx
            .send(Command::Shutdown {
                command_id: Some("c_quit".into()),
            })
            .await;
    }
    // Closing stdin is the EOF signal the session reads. After that
    // we wait briefly; the session normally exits in < 50 ms.
    drop(cmd_tx);
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await;
    if let Ok(None) = child.try_wait() {
        let _ = child.kill().await;
    }
    let _ = restore_terminal();
    Ok(state.exit_code())
}

async fn passthrough(patterns: &[String]) -> ExitCode {
    let bin = std::env::var_os(GIANT_BIN_ENV).unwrap_or_else(|| "giant".into());
    let status = TokioCommand::new(&bin)
        .arg("build")
        .args(patterns)
        .status()
        .await;
    match status {
        Ok(s) => ExitCode::from(s.code().unwrap_or(1) as u8),
        Err(e) => {
            eprintln!(
                "giant-tui: could not spawn `{}`: {e}",
                bin.to_string_lossy()
            );
            ExitCode::from(127)
        }
    }
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode().ok();
    std::io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

async fn pump_events<R: tokio::io::AsyncRead + Unpin>(stdout: R, tx: mpsc::Sender<Event>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(ev) = serde_json::from_str::<Event>(&line)
            && tx.send(ev).await.is_err()
        {
            break;
        }
    }
}

async fn read_keys(tx: mpsc::Sender<CtEvent>) {
    let mut events = EventStream::new();
    while let Some(ev) = events.next().await {
        match ev {
            Ok(ev) => {
                if tx.send(ev).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Owns the child's stdin. Receives Commands, serialises one per
/// line, writes to the session.
async fn write_commands(mut stdin: ChildStdin, mut rx: mpsc::Receiver<Command>) {
    while let Some(cmd) = rx.recv().await {
        let mut buf = match serde_json::to_vec(&cmd) {
            Ok(b) => b,
            Err(_) => continue,
        };
        buf.push(b'\n');
        if stdin.write_all(&buf).await.is_err() {
            break;
        }
        if stdin.flush().await.is_err() {
            break;
        }
    }
    // Dropping stdin closes it, which the session reads as EOF and
    // exits.
}

/// Whether the current frame contains anything that changes over time
/// (running durations in the build header, per-target elapsed timers).
/// The tick handler force-redraws when this is true so those counters
/// advance even with no inbound events.
fn is_time_dependent(state: &State) -> bool {
    matches!(state.screen, Screen::Building)
}

fn new_command_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}
