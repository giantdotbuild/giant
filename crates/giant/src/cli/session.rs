//! `giant session` - persistent engine over stdio (TDD-0014).
//!
//! Lifecycle:
//!   1. Refuse if stdout is a TTY (would corrupt the protocol).
//!   2. Load config + run discovery once.
//!   3. Emit `engine.hello` + `target.described` × N + `engine.ready`.
//!   4. Read commands on stdin (one JSON object per line), dispatch
//!      to the engine, multiplex events back on stdout.
//!   5. On EOF or `{"c":"shutdown"}`: cancel in-flight work, drain
//!      events, exit.
//!
//! For the first cut we support `build`, `cancel`, and `shutdown`.
//! `watch.start`/`watch.stop` and `config.reload` arrive in follow-up
//! changesets; the protocol accepts them (Command enum has the
//! variants) but the engine rejects them with `command.rejected`.

use crate::cache::LocalCache;
use crate::cli::{GlobalFlags, SilentExit};
use crate::cli::prep;
use crate::commands::Command;
use crate::events::{Event, EventSender, ShutdownReason, TargetCounts};
use crate::executor::{BuildJob, build};
use crate::graph::BuildGraph;
use crate::model::TargetId;
use crate::paths::AbsPath;
use clap::Args;
use std::io::IsTerminal;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Args, Debug)]
pub struct SessionArgs {
    /// Output format. Only `ndjson` is supported today; flag shape
    /// matches `giant build --events ndjson`.
    #[arg(long, value_name = "FORMAT", default_value = "ndjson")]
    pub events: String,
}

pub async fn execute(args: SessionArgs, global: &GlobalFlags) -> anyhow::Result<()> {
    if args.events != "ndjson" {
        anyhow::bail!("unsupported --events format: {}", args.events);
    }
    // stdout is the protocol channel. If it's a TTY a human is about
    // to see line noise; bail with a hint.
    if std::io::stdout().is_terminal() {
        anyhow::bail!(
            "giant session writes the protocol stream to stdout - pipe it \
             (`giant session | jq` or via a porcelain like `giant tui`), \
             don't run it interactively"
        );
    }

    // Event channel: every event the engine emits goes through here
    // and out via the writer task. Bounded buffer per TDD-0004
    // §Event delivery; drops are reported via protocol.dropped.
    let (event_tx, event_rx) = mpsc::channel::<Event>(2048);

    // Writer task owns stdout. Only this task writes to stdout, ever.
    let writer = tokio::spawn(write_events(event_rx));

    // engine.hello first so the client knows the protocol version.
    let _ = event_tx
        .send(Event::EngineHello {
            version: env!("CARGO_PKG_VERSION").into(),
            protocol: 1,
            workspace: workspace_hint(global).unwrap_or_default(),
        })
        .await;

    let prep_cancel = CancellationToken::new();
    let prepared = match prep::prepare(
        global.config.as_deref(),
        prep::num_cpus_estimate(),
        global.fresh,
        // Send bootstrap events into the same writer so the TUI sees
        // discovery progress as it happens.
        event_tx.clone(),
        prep_cancel.clone(),
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            let _ = event_tx
                .send(Event::EngineShutdown {
                    reason: ShutdownReason::Error,
                    error: Some(format!("{e:#}")),
                })
                .await;
            drop(event_tx);
            let _ = writer.await;
            return Err(SilentExit.into());
        }
    };

    // Catalog stream: one target.described per target in the merged
    // graph, then engine.ready.
    emit_catalog(&event_tx, &prepared.graph).await;
    let _ = event_tx.send(Event::EngineReady).await;

    let mut state = SessionState::new(prepared, event_tx.clone(), global.fresh);

    // Drive stdin → command channel.
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(64);
    tokio::spawn(read_commands(cmd_tx));

    // Main dispatch loop. A finishing build can trigger the next
    // queued build, so we also watch for build-completion events
    // out of band via the build-done channel.
    let (build_done_tx, mut build_done_rx) = mpsc::channel::<()>(8);

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => {
                        let exit = state.handle_command(cmd, &build_done_tx).await;
                        if exit {
                            break;
                        }
                    }
                    None => break, // stdin EOF
                }
            }
            Some(()) = build_done_rx.recv() => {
                state.on_build_finished(&build_done_tx).await;
            }
        }
    }

    // Drain: cancel anything still running, wait briefly.
    state.shutdown().await;
    let _ = event_tx
        .send(Event::EngineShutdown {
            reason: ShutdownReason::Graceful,
            error: None,
        })
        .await;
    drop(event_tx);
    let _ = writer.await;
    Ok(())
}

/// Carries the engine state across commands. One per session.
struct SessionState {
    graph: Arc<BuildGraph>,
    cache: LocalCache,
    workspace_root: AbsPath,
    fresh_default: bool,
    event_tx: EventSender,
    /// At most one build runs at a time in v1. A second `build`
    /// command while another is in flight is queued (only the most
    /// recent kept; older queued builds are dropped with a synthetic
    /// `build.finished`).
    running: Option<RunningBuild>,
    queued: Option<QueuedBuild>,
    next_build_seq: u64,
}

struct RunningBuild {
    build_id: String,
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

struct QueuedBuild {
    build_id: String,
    targets: Vec<TargetId>,
    fresh: bool,
    cancel: CancellationToken,
}

impl SessionState {
    fn new(prepared: prep::Prepared, event_tx: EventSender, fresh_default: bool) -> Self {
        Self {
            graph: Arc::new(prepared.graph),
            cache: prepared.cache,
            workspace_root: prepared.workspace_root,
            fresh_default,
            event_tx,
            running: None,
            queued: None,
            next_build_seq: 0,
        }
    }

    async fn handle_command(
        &mut self,
        cmd: Command,
        build_done_tx: &mpsc::Sender<()>,
    ) -> bool {
        match cmd {
            Command::Shutdown { command_id } => {
                self.ack(command_id, None).await;
                return true;
            }
            Command::Build {
                command_id,
                targets,
                fresh,
            } => {
                if let Some(reason) = self.validate_targets(&targets) {
                    self.reject(command_id, reason).await;
                    return false;
                }
                let build_id = self.next_build_id();
                let cancel = CancellationToken::new();
                if self.running.is_none() {
                    self.start_build(
                        build_id.clone(),
                        targets,
                        fresh || self.fresh_default,
                        cancel,
                        build_done_tx.clone(),
                    );
                } else {
                    // Drop any previously-queued build with a
                    // synthetic finished event so the client sees a
                    // terminal state for the build it asked for.
                    if let Some(prev) = self.queued.take() {
                        let _ = self
                            .event_tx
                            .send(Event::BuildFinished {
                                id: prev.build_id,
                                ok: false,
                                duration_ms: 0,
                                counts: TargetCounts::default(),
                            })
                            .await;
                    }
                    self.queued = Some(QueuedBuild {
                        build_id: build_id.clone(),
                        targets,
                        fresh: fresh || self.fresh_default,
                        cancel,
                    });
                }
                self.ack(command_id, Some(build_id)).await;
            }
            Command::Cancel { command_id, build } => {
                let found = self.cancel_build(&build).await;
                if found {
                    self.ack(command_id, Some(build)).await;
                } else {
                    self.reject(command_id, format!("no build with id {build}"))
                        .await;
                }
            }
            Command::WatchStart { command_id, .. }
            | Command::WatchStop { command_id }
            | Command::ConfigReload { command_id } => {
                self.reject(
                    command_id,
                    "watch / reload commands arrive in a follow-up changeset".into(),
                )
                .await;
            }
        }
        false
    }

    fn next_build_id(&mut self) -> String {
        self.next_build_seq += 1;
        format!("b_{:04x}", self.next_build_seq)
    }

    fn validate_targets(&self, targets: &[TargetId]) -> Option<String> {
        if targets.is_empty() {
            return Some("build command has empty target list".into());
        }
        for id in targets {
            if self.graph.get(id).is_none() {
                return Some(format!("unknown target: {id}"));
            }
        }
        None
    }

    fn start_build(
        &mut self,
        build_id: String,
        targets: Vec<TargetId>,
        fresh: bool,
        cancel: CancellationToken,
        build_done_tx: mpsc::Sender<()>,
    ) {
        let job = BuildJob {
            graph: self.graph.clone(),
            selection: targets,
            cache: self.cache.clone(),
            workspace_root: self.workspace_root.clone(),
            parallelism: prep::num_cpus_estimate(),
            fresh,
            events: self.event_tx.clone(),
            cancel: cancel.clone(),
            build_id: build_id.clone(),
            #[cfg(feature = "remote")]
            remote: None,
            #[cfg(feature = "remote")]
            upload_tx: None,
        };
        let event_tx = self.event_tx.clone();
        let id_for_task = build_id.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = build(job).await {
                // Engine itself failed (not target failure). Surface
                // it; build.finished will already have fired with
                // counts including failed targets if relevant.
                let _ = event_tx
                    .send(Event::CommandError {
                        command_id: id_for_task,
                        message: format!("executor error: {e:#}"),
                    })
                    .await;
            }
            let _ = build_done_tx.send(()).await;
        });
        self.running = Some(RunningBuild {
            build_id,
            cancel,
            handle,
        });
    }

    async fn on_build_finished(&mut self, build_done_tx: &mpsc::Sender<()>) {
        // Drop the join handle (build task already exited).
        self.running = None;
        // Start the queued build, if any.
        if let Some(q) = self.queued.take() {
            self.start_build(
                q.build_id,
                q.targets,
                q.fresh,
                q.cancel,
                build_done_tx.clone(),
            );
        }
    }

    async fn cancel_build(&mut self, build_id: &str) -> bool {
        if let Some(r) = &self.running
            && r.build_id == build_id
        {
            r.cancel.cancel();
            return true;
        }
        if let Some(q) = &self.queued
            && q.build_id == build_id
        {
            // Drop from queue with a synthetic finished event.
            let bid = q.build_id.clone();
            self.queued = None;
            let _ = self
                .event_tx
                .send(Event::BuildFinished {
                    id: bid,
                    ok: false,
                    duration_ms: 0,
                    counts: TargetCounts::default(),
                })
                .await;
            return true;
        }
        false
    }

    async fn shutdown(&mut self) {
        if let Some(r) = self.running.take() {
            r.cancel.cancel();
            // Bounded wait; if the build is unresponsive, executor's
            // own SIGKILL escalation eventually fires.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), r.handle).await;
        }
        // Any queued build is dropped silently - no event since the
        // client is going away.
        self.queued = None;
    }

    async fn ack(&self, command_id: Option<String>, build: Option<String>) {
        if let Some(id) = command_id {
            let _ = self
                .event_tx
                .send(Event::CommandAccepted {
                    command_id: id,
                    build,
                })
                .await;
        }
    }

    async fn reject(&self, command_id: Option<String>, reason: String) {
        if let Some(id) = command_id {
            let _ = self
                .event_tx
                .send(Event::CommandRejected {
                    command_id: id,
                    reason,
                })
                .await;
        }
    }
}

async fn emit_catalog(tx: &EventSender, graph: &BuildGraph) {
    use crate::model::Input;
    for (id, spec) in graph.iter() {
        let mut tags: Vec<String> = spec.tags.iter().cloned().collect();
        tags.sort();
        let inputs: Vec<String> = spec
            .inputs
            .iter()
            .map(|i| match i {
                Input::File { glob } => glob.as_str().to_string(),
                Input::Structural { files, .. } => {
                    let joined: Vec<String> = files.iter().map(|g| g.as_str().to_string()).collect();
                    format!("structural:{}", joined.join(","))
                }
            })
            .collect();
        let _ = tx
            .send(Event::TargetDescribed {
                id: id.clone(),
                tags,
                test: spec.test,
                command: spec.command.clone(),
                inputs,
                outputs: spec
                    .outputs
                    .iter()
                    .map(|p| p.as_path().display().to_string())
                    .collect(),
                deps: spec.deps.clone(),
            })
            .await;
    }
}

/// Best-effort stdin reader. Reads JSON-per-line, parses to Command,
/// drops bad lines with a CommandError back-channel? No - for v1 we
/// silently drop unparseable lines and let the writer's
/// protocol.dropped counter cover us. Future: emit `command.rejected`
/// with the raw line.
async fn read_commands(tx: mpsc::Sender<Command>) {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Command>(&line) {
            Ok(cmd) => {
                if tx.send(cmd).await.is_err() {
                    break;
                }
            }
            Err(_) => {
                // Skip bad lines. A future revision could emit a
                // command.rejected event but we don't have a
                // command_id to attach it to.
            }
        }
    }
    // EOF closes the channel and the main loop exits.
}

/// Owns stdout for the session's lifetime. Reads events and writes
/// one JSON object per line. Stops when the channel closes.
async fn write_events(mut rx: mpsc::Receiver<Event>) {
    let mut out = tokio::io::stdout();
    while let Some(ev) = rx.recv().await {
        let mut buf = match serde_json::to_vec(&ev) {
            Ok(b) => b,
            Err(_) => break, // stream is corrupt; bail
        };
        buf.push(b'\n');
        if out.write_all(&buf).await.is_err() {
            break;
        }
        // Flush so the client sees events promptly without us
        // building up a 4 KiB block. Cheap for small JSON.
        if out.flush().await.is_err() {
            break;
        }
    }
}

fn workspace_hint(global: &GlobalFlags) -> Option<String> {
    if let Some(p) = &global.config {
        return Some(p.display().to_string());
    }
    std::env::current_dir().ok().map(|p| p.display().to_string())
}
