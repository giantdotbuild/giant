//! `giant session` - persistent engine over stdio (TDD-0014).
//!
//! Lifecycle:
//!   1. Refuse if stdout is a TTY (would corrupt the protocol).
//!   2. Load config + build the graph once.
//!   3. Emit `engine.hello` + `target.described` × N + `engine.ready`.
//!   4. Read commands on stdin (one JSON object per line), dispatch
//!      to the engine, multiplex events back on stdout.
//!   5. On EOF or `{"c":"shutdown"}`: cancel in-flight work, drain
//!      events, exit.
//!
//! Commands: `build`, `cancel`, `watch.start`/`watch.stop`,
//! `affected.subscribe`/`unsubscribe`, `watch.subscribe`/`unsubscribe`,
//! `config.reload`, and `shutdown`. An always-on watcher also triggers a
//! reload on a `giant.yaml` / `giant.json` change, so the catalog stays
//! live without a restart (TDD-0014).

use crate::cache::LocalCache;
use crate::cli::prep;
use crate::cli::{GlobalFlags, SilentExit};
use crate::commands::Command;
use crate::events::{
    Event, EventSender, ExplainCacheHit, ExplainDep, ExplainEnv, ExplainInput, ExplainOutput,
    LogStream, ShutdownReason, TargetCounts, TargetStatus,
};
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

/// Read/query capabilities this engine advertises in `engine.hello`
/// (ADR-0033). Extend as queries are added.
const CAPABILITIES: &[&str] = &["query.status", "logs.get", "query.explain"];

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
            protocol: 2,
            workspace: workspace_hint(global).unwrap_or_default(),
            capabilities: CAPABILITIES.iter().map(|s| (*s).to_string()).collect(),
        })
        .await;

    let parallelism = prep::num_cpus_estimate();
    let prepared = match prep::prepare(global.config.as_deref()).await {
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

    let mut state = SessionState::new(
        prepared,
        event_tx.clone(),
        // The session never forces fresh globally; clients pass `fresh` per
        // build via the protocol (ADR-0034 dropped the giant-level --fresh).
        false,
        global.config.clone(),
        parallelism,
    );

    // Drive stdin → command channel.
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(64);
    tokio::spawn(read_commands(cmd_tx));

    // Always-on config watcher: a `giant.yaml` / `giant.json` edit
    // triggers a reload (rebuild graph + re-emit catalog). The handle
    // must outlive the loop; dropping it stops the OS watch.
    let (reload_tx, mut reload_rx) = mpsc::channel::<()>(8);
    let _config_watch = spawn_config_watcher(
        config_watch_dirs(&state.workspace_root, &state.graph),
        reload_tx,
    );

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
            Some(()) = reload_rx.recv() => {
                state.reload().await;
            }
        }
    }

    // Drain: cancel anything still running, wait briefly. Then drop the
    // state so its `event_tx` clone is released - otherwise the writer
    // task never sees the channel close and we'd hang here.
    state.shutdown().await;
    drop(state);
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

/// Run one build through the engine and wait for it to finish - the
/// in-process adapter behind `giant build` / `test` (TDD-0021). The same
/// `Command::Build` → `start_build` path the stdio session uses; events
/// flow to `event_tx` so the caller can render them, and the remote-cache
/// uploader is opened and drained here. Pass/fail is read off the event
/// stream by the caller (the renderer captures `build.finished`).
pub async fn run_one_build(
    prepared: prep::Prepared,
    event_tx: EventSender,
    config_path: Option<std::path::PathBuf>,
    parallelism: usize,
    selection: Vec<TargetId>,
    fresh: bool,
    sandbox: Option<crate::executor::SandboxPolicy>,
) -> anyhow::Result<()> {
    // No-op without the `remote` feature (returns all-None).
    let (remote, upload_tx, upload_handle) = prep::open_remote(&prepared.config)?;

    let state = SessionState::new(prepared, event_tx, fresh, config_path, parallelism)
        .with_sandbox(sandbox);
    #[cfg(feature = "remote")]
    let mut state = state.with_remote(remote, upload_tx.clone());
    #[cfg(not(feature = "remote"))]
    let mut state = {
        let _ = (remote, &upload_tx);
        state
    };

    let (build_done_tx, mut build_done_rx) = mpsc::channel::<()>(8);
    state
        .handle_command(
            Command::Build {
                command_id: None,
                targets: selection,
                fresh,
            },
            &build_done_tx,
        )
        .await;
    // Exactly one build; wait for it, then tear down.
    let _ = build_done_rx.recv().await;
    state.shutdown().await;
    drop(state);

    #[cfg(feature = "remote")]
    {
        drop(upload_tx);
        if let Some(h) = upload_handle {
            let _ = h.await;
        }
    }
    #[cfg(not(feature = "remote"))]
    let _ = (upload_tx, upload_handle);
    Ok(())
}

/// Run a watch session through the engine - `build --watch` /
/// `test --watch` (TDD-0021). Dispatches `watch.start`, lets the caller
/// render the event stream off `event_tx`, and runs until Ctrl-C, then
/// stops the watch and drains. Watch rebuilds deliberately do **not**
/// upload to the remote cache - rapid local iteration shouldn't pollute
/// the shared cache; the one-shot `giant build` still uploads.
pub async fn run_watch_command(
    prepared: prep::Prepared,
    event_tx: EventSender,
    config_path: Option<std::path::PathBuf>,
    parallelism: usize,
    selection: Vec<TargetId>,
    fresh: bool,
    sandbox: Option<crate::executor::SandboxPolicy>,
) -> anyhow::Result<()> {
    let mut state = SessionState::new(prepared, event_tx, fresh, config_path, parallelism)
        .with_sandbox(sandbox);

    // Ctrl-C → cancel the watch.
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        ctrlc::set_handler(move || cancel.cancel()).ok();
    }

    // `watch.start` spawns the loop; it doesn't use the build-done signal.
    let (build_done_tx, _build_done_rx) = mpsc::channel::<()>(8);
    state
        .handle_command(
            Command::WatchStart {
                command_id: None,
                targets: selection,
            },
            &build_done_tx,
        )
        .await;

    cancel.cancelled().await;

    state
        .handle_command(Command::WatchStop { command_id: None }, &build_done_tx)
        .await;
    state.shutdown().await;
    drop(state);
    Ok(())
}

/// Carries the engine state across commands. One per session.
struct SessionState {
    graph: Arc<BuildGraph>,
    cache: LocalCache,
    workspace_root: AbsPath,
    /// Resolved absolute cache directory - the watcher must exclude it
    /// so cache writes don't trigger rebuild storms.
    cache_root: AbsPath,
    /// Workspace-relative state directory (`config.state.dir`), excluded
    /// from the watchers alongside the cache.
    state_dir: std::path::PathBuf,
    log_capture: crate::executor::LogCapture,
    fresh_default: bool,
    event_tx: EventSender,
    /// At most one build runs at a time in v1. A second `build`
    /// command while another is in flight is queued (only the most
    /// recent kept; older queued builds are dropped with a synthetic
    /// `build.finished`).
    running: Option<RunningBuild>,
    queued: Option<QueuedBuild>,
    /// Active watch session, if any. Mutually exclusive with regular
    /// builds in v1 - `build` while watching is rejected, and
    /// `watch.start` while a build is in flight is rejected.
    watch: Option<WatchSession>,
    /// Active affected subscription, if any. Independent of builds /
    /// watch - clients (the TUI) keep this alive while filtering by
    /// affected, and it auto-recomputes on file changes.
    affected: Option<AffectedSubscription>,
    /// Active notify-only watch subscription, if any (`watch.subscribe`).
    /// Independent of everything else - porcelains (giant-task --watch)
    /// keep it alive to learn when a scoped change happens.
    changes: Option<ChangeSubscription>,
    next_build_seq: u64,
    /// Config path + parallelism, kept so `config.reload` can re-run
    /// `prep::prepare` exactly as session startup did.
    config_path: Option<std::path::PathBuf>,
    parallelism: usize,
    /// Sandbox policy for the build path. `None` unless `--sandbox` is on;
    /// set by the one-shot CLI helpers via `with_sandbox` (ADR-0030).
    sandbox: Option<crate::executor::SandboxPolicy>,
    /// Remote-cache handles for the build path. The stdio session leaves
    /// these `None` today; the in-process CLI adapter sets them so
    /// `giant build` keeps using the remote cache (TDD-0021).
    #[cfg(feature = "remote")]
    remote: Option<crate::remote::RemoteCache>,
    #[cfg(feature = "remote")]
    upload_tx: Option<tokio::sync::mpsc::Sender<crate::remote::UploadJob>>,
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

struct WatchSession {
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
    /// The client's selection, kept so a config reload can restart the
    /// watch against the rebuilt graph.
    selection: Vec<TargetId>,
}

struct AffectedSubscription {
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
    /// Git baseline, kept so a reload can restart the subscription.
    base: String,
}

struct ChangeSubscription {
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
    /// Scope (targets + globs), kept so a reload can restart it.
    targets: Vec<TargetId>,
    globs: Vec<String>,
}

impl SessionState {
    fn new(
        prepared: prep::Prepared,
        event_tx: EventSender,
        fresh_default: bool,
        config_path: Option<std::path::PathBuf>,
        parallelism: usize,
    ) -> Self {
        let cache_root = prep::resolve_cache_dir(&prepared.config.cache.dir)
            .map(AbsPath::new)
            .unwrap_or_else(|_| prepared.workspace_root.clone());
        let log_capture = crate::executor::LogCapture::from_cache_config(&prepared.config.cache);
        let state_dir = std::path::PathBuf::from(&prepared.config.state.dir);
        Self {
            graph: Arc::new(prepared.graph),
            cache: prepared.cache,
            workspace_root: prepared.workspace_root,
            cache_root,
            state_dir,
            log_capture,
            fresh_default,
            event_tx,
            running: None,
            queued: None,
            watch: None,
            affected: None,
            changes: None,
            next_build_seq: 0,
            config_path,
            parallelism,
            sandbox: None,
            #[cfg(feature = "remote")]
            remote: None,
            #[cfg(feature = "remote")]
            upload_tx: None,
        }
    }

    /// Attach remote-cache handles to the build path (the in-process CLI
    /// adapter; the stdio session leaves them unset).
    #[cfg(feature = "remote")]
    fn with_remote(
        mut self,
        remote: Option<crate::remote::RemoteCache>,
        upload_tx: Option<tokio::sync::mpsc::Sender<crate::remote::UploadJob>>,
    ) -> Self {
        self.remote = remote;
        self.upload_tx = upload_tx;
        self
    }

    /// Attach a sandbox policy to the build path (one-shot `giant build` /
    /// `giant test --sandbox`). Unset = run commands directly (ADR-0030).
    fn with_sandbox(mut self, sandbox: Option<crate::executor::SandboxPolicy>) -> Self {
        self.sandbox = sandbox;
        self
    }

    async fn handle_command(&mut self, cmd: Command, build_done_tx: &mpsc::Sender<()>) -> bool {
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
                if self.watch.is_some() {
                    self.reject(
                        command_id,
                        "watch is active - send `watch.stop` first".into(),
                    )
                    .await;
                    return false;
                }
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
            Command::WatchStart {
                command_id,
                targets,
            } => {
                if self.watch.is_some() {
                    self.reject(command_id, "watch already active".into()).await;
                    return false;
                }
                if self.running.is_some() || self.queued.is_some() {
                    self.reject(
                        command_id,
                        "build in flight - try again when it finishes".into(),
                    )
                    .await;
                    return false;
                }
                if let Some(reason) = self.validate_targets(&targets) {
                    self.reject(command_id, reason).await;
                    return false;
                }
                self.start_watch(targets);
                self.ack(command_id, None).await;
            }
            Command::WatchStop { command_id } => {
                if let Some(w) = self.watch.take() {
                    w.cancel.cancel();
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), w.handle).await;
                }
                self.ack(command_id, None).await;
            }
            Command::ConfigReload { command_id } => {
                self.reload().await;
                self.ack(command_id, None).await;
            }
            Command::AffectedSubscribe { command_id, base } => {
                // Replace any existing subscription. A re-subscribe
                // with the same base is the natural "force-refresh"
                // primitive - the new task computes immediately.
                if let Some(prev) = self.affected.take() {
                    prev.cancel.cancel();
                    let _ =
                        tokio::time::timeout(std::time::Duration::from_secs(1), prev.handle).await;
                }
                self.start_affected(base);
                self.ack(command_id, None).await;
            }
            Command::AffectedUnsubscribe { command_id } => {
                if let Some(a) = self.affected.take() {
                    a.cancel.cancel();
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), a.handle).await;
                }
                self.ack(command_id, None).await;
            }
            Command::WatchSubscribe {
                command_id,
                targets,
                globs,
            } => {
                // A subscribe that names an unknown target is a config
                // error worth surfacing (vs. silently never firing).
                if let Some(reason) = self.validate_subscribe(&targets, &globs) {
                    self.reject(command_id, reason).await;
                    return false;
                }
                // Replace any existing subscription, like affected.
                if let Some(prev) = self.changes.take() {
                    prev.cancel.cancel();
                    let _ =
                        tokio::time::timeout(std::time::Duration::from_secs(1), prev.handle).await;
                }
                self.start_changes(targets, globs);
                self.ack(command_id, None).await;
            }
            Command::WatchUnsubscribe { command_id } => {
                if let Some(c) = self.changes.take() {
                    c.cancel.cancel();
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), c.handle).await;
                }
                self.ack(command_id, None).await;
            }
            Command::QueryStatus {
                command_id,
                targets,
            } => {
                self.query_status(command_id, targets).await;
            }
            Command::LogsGet {
                command_id,
                target,
                follow,
                key,
            } => {
                self.logs_get(command_id, target, follow, key).await;
            }
            Command::QueryExplain { command_id, target } => {
                self.query_explain(command_id, target).await;
            }
        }
        false
    }

    /// Answer `query.explain` (ADR-0033): the structured cache-key breakdown for
    /// a target, plus whether it is currently cached. Reuses the same
    /// `breakdown_for_target` walk `giant explain` uses.
    async fn query_explain(&self, command_id: Option<String>, target: TargetId) {
        if let Some(reason) = self.validate_targets(std::slice::from_ref(&target)) {
            self.reject(command_id, reason).await;
            return;
        }
        let mut memo = std::collections::BTreeMap::new();
        let (key, breakdown, _) = match crate::explain::breakdown_for_target(
            &self.graph,
            &self.cache,
            &self.workspace_root,
            &target,
            &mut memo,
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                self.reject(command_id, format!("cannot compute cache key: {e:#}"))
                    .await;
                return;
            }
        };
        let ac = self.cache.get_ac(&key).await.ok().flatten();
        let cached = ac.is_some();
        let cache_hit = ac.map(|entry| ExplainCacheHit {
            built_at: entry.built_at,
            duration_ms: entry.duration_ms,
            exit_code: entry.exit_code,
            outputs: entry
                .outputs
                .into_iter()
                .map(|o| ExplainOutput {
                    path: o.path,
                    hash: o.content_hash,
                    size: o.size,
                    mode: o.mode,
                })
                .collect(),
            outputs_content_hash: entry.outputs_content_hash,
        });

        let file_inputs = breakdown
            .file_inputs
            .iter()
            .map(|f| ExplainInput {
                path: f.rel_path.clone(),
                hash: f.content_hash.to_hex(),
                size: f.size,
            })
            .collect();
        let deps = breakdown
            .dep_outputs
            .iter()
            .map(|(id, h)| ExplainDep {
                id: id.clone(),
                output_hash: h.to_hex(),
            })
            .collect();
        let mut env: Vec<ExplainEnv> = breakdown
            .user_env
            .iter()
            .map(|(k, v)| ExplainEnv {
                key: k.clone(),
                value: v.clone(),
                built_in: false,
            })
            .collect();
        env.extend(breakdown.built_in_env.iter().map(|(k, v)| ExplainEnv {
            key: k.clone(),
            value: v.clone(),
            built_in: true,
        }));

        let _ = self
            .event_tx
            .send(Event::QueryExplained {
                command_id,
                target,
                key: key.to_hex(),
                cached,
                command: breakdown.command,
                cwd: breakdown.cwd,
                file_inputs,
                deps,
                env,
                cache_hit,
            })
            .await;
    }

    /// Answer `logs.get` (ADR-0033): replay a target's captured logs from its
    /// last cached build as `logs.line` events, then `logs.end`. Reuses the
    /// cache-key walk + the AC entry's stdout/stderr blobs (the same data
    /// `giant logs` reads). `follow` (live tail of a running target) is not yet
    /// implemented; this always replays the persisted logs.
    async fn logs_get(
        &self,
        command_id: Option<String>,
        target: TargetId,
        _follow: bool,
        key: Option<String>,
    ) {
        if let Some(reason) = self.validate_targets(std::slice::from_ref(&target)) {
            self.reject(command_id, reason).await;
            return;
        }
        let key = match key {
            Some(hex) => match crate::model::ContentHash::from_hex(&hex) {
                Some(h) => crate::model::CacheKey::new(h),
                None => {
                    self.reject(command_id, format!("malformed cache key: {hex}"))
                        .await;
                    return;
                }
            },
            None => {
                let mut memo = std::collections::BTreeMap::new();
                match crate::explain::walk_target(
                    &self.graph,
                    &self.cache,
                    &self.workspace_root,
                    &target,
                    &mut memo,
                )
                .await
                {
                    Ok((key, _)) => key,
                    Err(e) => {
                        self.reject(command_id, format!("cannot compute cache key: {e:#}"))
                            .await;
                        return;
                    }
                }
            }
        };

        if let Some(entry) = self.cache.get_ac(&key).await.ok().flatten() {
            self.replay_blob(
                &command_id,
                &target,
                entry.stdout_blob.as_deref(),
                LogStream::Stdout,
            )
            .await;
            self.replay_blob(
                &command_id,
                &target,
                entry.stderr_blob.as_deref(),
                LogStream::Stderr,
            )
            .await;
        }

        let _ = self
            .event_tx
            .send(Event::LogsEnd { command_id, target })
            .await;
    }

    /// Read a captured log blob (CAS hex) and emit each line as a `logs.line`.
    async fn replay_blob(
        &self,
        command_id: &Option<String>,
        target: &TargetId,
        hex: Option<&str>,
        stream: LogStream,
    ) {
        let Some(hex) = hex else { return };
        let Some(hash) = crate::model::ContentHash::from_hex(hex) else {
            return;
        };
        let Ok(Some(bytes)) = self.cache.get_cas(&hash).await else {
            return;
        };
        for line in String::from_utf8_lossy(&bytes).lines() {
            let _ = self
                .event_tx
                .send(Event::LogsLine {
                    command_id: command_id.clone(),
                    target: target.clone(),
                    stream,
                    line: line.to_string(),
                })
                .await;
        }
    }

    /// Answer `query.status` (ADR-0033): per-target cache state. Recomputes each
    /// target's cache key (the same walk `giant explain` uses) and consults the
    /// action cache. Read-only; safe to run alongside or between builds.
    async fn query_status(&self, command_id: Option<String>, targets: Vec<TargetId>) {
        if !targets.is_empty()
            && let Some(reason) = self.validate_targets(&targets)
        {
            self.reject(command_id, reason).await;
            return;
        }
        let ids: Vec<TargetId> = if targets.is_empty() {
            let mut v: Vec<TargetId> = self.graph.iter().map(|(id, _)| id.clone()).collect();
            v.sort();
            v
        } else {
            targets
        };

        let mut memo = std::collections::BTreeMap::new();
        let mut out = Vec::with_capacity(ids.len());
        for id in &ids {
            let status = match crate::explain::walk_target(
                &self.graph,
                &self.cache,
                &self.workspace_root,
                id,
                &mut memo,
            )
            .await
            {
                Ok((key, _)) => {
                    let entry = self.cache.get_ac(&key).await.ok().flatten();
                    let (state, last_duration_ms) = match entry {
                        Some(e) => ("cached", Some(e.duration_ms)),
                        None => ("stale", None),
                    };
                    TargetStatus {
                        id: id.clone(),
                        state: state.to_string(),
                        key: key.to_hex(),
                        last_duration_ms,
                    }
                }
                // A key we cannot compute (e.g. a missing declared input) reads
                // as stale with no key, rather than failing the whole query.
                Err(_) => TargetStatus {
                    id: id.clone(),
                    state: "stale".to_string(),
                    key: String::new(),
                    last_duration_ms: None,
                },
            };
            out.push(status);
        }

        let _ = self
            .event_tx
            .send(Event::QueryStatus {
                command_id,
                targets: out,
            })
            .await;
    }

    fn start_affected(&mut self, base: String) {
        let cancel = CancellationToken::new();
        let ctx = AffectedCtx {
            graph: self.graph.clone(),
            workspace_root: self.workspace_root.clone(),
            cache_root: self.cache_root.clone(),
            state_dir: self.state_dir.clone(),
            event_tx: self.event_tx.clone(),
        };
        let cancel_for_task = cancel.clone();
        let base_for_loop = base.clone();
        let handle = tokio::spawn(async move {
            affected_loop(ctx, base_for_loop, cancel_for_task).await;
        });
        self.affected = Some(AffectedSubscription {
            cancel,
            handle,
            base,
        });
    }

    fn start_changes(&mut self, targets: Vec<TargetId>, globs: Vec<String>) {
        let cancel = CancellationToken::new();
        let ctx = ChangeCtx {
            graph: self.graph.clone(),
            workspace_root: self.workspace_root.clone(),
            cache_root: self.cache_root.clone(),
            state_dir: self.state_dir.clone(),
            event_tx: self.event_tx.clone(),
        };
        let cancel_for_task = cancel.clone();
        let (targets_for_loop, globs_for_loop) = (targets.clone(), globs.clone());
        let handle = tokio::spawn(async move {
            watch_subscribe_loop(ctx, targets_for_loop, globs_for_loop, cancel_for_task).await;
        });
        self.changes = Some(ChangeSubscription {
            cancel,
            handle,
            targets,
            globs,
        });
    }

    fn start_watch(&mut self, selection: Vec<TargetId>) {
        let cancel = CancellationToken::new();
        let ctx = WatchCtx {
            graph: self.graph.clone(),
            cache: self.cache.clone(),
            workspace_root: self.workspace_root.clone(),
            cache_root: self.cache_root.clone(),
            state_dir: self.state_dir.clone(),
            parallelism: self.parallelism,
            log_capture: self.log_capture,
            fresh: self.fresh_default,
            sandbox: self.sandbox.clone(),
            event_tx: self.event_tx.clone(),
        };
        let cancel_for_task = cancel.clone();
        let selection_for_loop = selection.clone();
        let handle = tokio::spawn(async move {
            watch_loop(ctx, selection_for_loop, cancel_for_task).await;
        });
        self.watch = Some(WatchSession {
            cancel,
            handle,
            selection,
        });
    }

    /// Re-load config, rebuild the graph, re-emit the catalog, and
    /// restart any active subscriptions against the new graph. Triggered
    /// by `config.reload` or a `giant.yaml` / `giant.json` change. A
    /// build in flight keeps its own graph `Arc`, so reloading is safe
    /// to do alongside it (deviating from TDD-0014's "queue until the
    /// build finishes" - simpler, and the running build is unaffected).
    async fn reload(&mut self) {
        let _ = self.event_tx.send(Event::CatalogInvalidating).await;

        // Snapshot active subscriptions' params, then tear them down -
        // their loops hold the old graph and must be restarted.
        let watch_sel = self.watch.take().map(|w| {
            w.cancel.cancel();
            w.handle.abort();
            w.selection
        });
        let affected_base = self.affected.take().map(|a| {
            a.cancel.cancel();
            a.handle.abort();
            a.base
        });
        let changes_scope = self.changes.take().map(|c| {
            c.cancel.cancel();
            c.handle.abort();
            (c.targets, c.globs)
        });

        match prep::prepare(self.config_path.as_deref()).await {
            Ok(p) => {
                self.cache_root = prep::resolve_cache_dir(&p.config.cache.dir)
                    .map(AbsPath::new)
                    .unwrap_or_else(|_| p.workspace_root.clone());
                self.state_dir = std::path::PathBuf::from(&p.config.state.dir);
                self.log_capture = crate::executor::LogCapture::from_cache_config(&p.config.cache);
                self.workspace_root = p.workspace_root;
                self.cache = p.cache;
                self.graph = Arc::new(p.graph);
            }
            Err(e) => {
                // Keep the previous graph; tell the client and still emit
                // a `catalog.ready` so it isn't stuck on `invalidating`.
                let _ = self
                    .event_tx
                    .send(Event::CommandError {
                        command_id: "config.reload".into(),
                        message: format!("reload failed, keeping previous config: {e:#}"),
                    })
                    .await;
            }
        }

        emit_catalog(&self.event_tx, &self.graph).await;
        let _ = self.event_tx.send(Event::CatalogReady).await;

        // Restart subscriptions against the (possibly new) graph, dropping
        // named targets that no longer exist.
        if let Some(sel) = watch_sel {
            let alive: Vec<TargetId> = sel
                .into_iter()
                .filter(|id| self.graph.get(id).is_some())
                .collect();
            if !alive.is_empty() {
                self.start_watch(alive);
            }
        }
        if let Some(base) = affected_base {
            self.start_affected(base);
        }
        if let Some((targets, globs)) = changes_scope {
            let alive: Vec<TargetId> = targets
                .into_iter()
                .filter(|id| self.graph.get(id).is_some())
                .collect();
            self.start_changes(alive, globs);
        }
    }

    fn next_build_id(&mut self) -> String {
        self.next_build_seq += 1;
        format!("b_{:04x}", self.next_build_seq)
    }

    /// The first target id not in the graph, as a rejection reason.
    fn unknown_target(&self, targets: &[TargetId]) -> Option<String> {
        targets
            .iter()
            .find(|id| self.graph.get(id).is_none())
            .map(|id| format!("unknown target: {id}"))
    }

    fn validate_targets(&self, targets: &[TargetId]) -> Option<String> {
        if targets.is_empty() {
            return Some("build command has empty target list".into());
        }
        self.unknown_target(targets)
    }

    /// A watch subscription may name zero targets (whole-workspace), but
    /// any target it does name must exist, and any glob must compile.
    fn validate_subscribe(&self, targets: &[TargetId], globs: &[String]) -> Option<String> {
        if let Some(reason) = self.unknown_target(targets) {
            return Some(reason);
        }
        for g in globs {
            if let Err(e) = glob::Pattern::new(g) {
                return Some(format!("bad glob {g:?}: {e}"));
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
            parallelism: self.parallelism,
            fresh,
            events: self.event_tx.clone(),
            cancel: cancel.clone(),
            build_id: build_id.clone(),
            log_capture: self.log_capture,
            sandbox: self.sandbox.clone(),
            #[cfg(feature = "remote")]
            remote: self.remote.clone(),
            #[cfg(feature = "remote")]
            upload_tx: self.upload_tx.clone(),
        };
        let event_tx = self.event_tx.clone();
        let id_for_task = build_id.clone();
        let failures_path = prep::last_failures_path(
            self.workspace_root.as_path(),
            &self.state_dir.to_string_lossy(),
        );
        let handle = tokio::spawn(async move {
            match build(job).await {
                Ok(summary) => {
                    // Record the failed set (empty = clean) for `failed-last`.
                    prep::write_last_failures(&failures_path, &summary.failed_targets);
                }
                Err(e) => {
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
        if let Some(a) = self.affected.take() {
            a.cancel.cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), a.handle).await;
        }
        if let Some(w) = self.watch.take() {
            w.cancel.cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), w.handle).await;
        }
        if let Some(c) = self.changes.take() {
            c.cancel.cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), c.handle).await;
        }
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

/// Spawn the always-on config watcher. Watches the workspace and, on a
/// debounced `giant.yaml` / `giant.json` change, sends a reload signal.
/// Returns the watcher handle (keep it alive); `None` if it couldn't
/// start - reload then works only via the explicit `config.reload`
/// command.
/// Directories to watch for config edits: the workspace root plus every
/// package directory (where a `giant.<infix>.yaml` lives). A non-recursive
/// watch over this small fixed set is all the config watcher needs - it only
/// reacts to `giant.yaml`/`giant.json` changes - and it avoids recursively
/// registering OS watches over the whole workspace (e.g. a multi-GB `.devenv`).
fn config_watch_dirs(root: &AbsPath, graph: &BuildGraph) -> Vec<std::path::PathBuf> {
    let mut dirs: std::collections::BTreeSet<std::path::PathBuf> =
        std::collections::BTreeSet::new();
    dirs.insert(root.as_path().to_path_buf());
    for (id, _) in graph.iter() {
        let pkg = id.split().0;
        if !pkg.is_empty() {
            dirs.insert(root.as_path().join(pkg));
        }
    }
    dirs.into_iter().collect()
}

fn spawn_config_watcher(
    dirs: Vec<std::path::PathBuf>,
    reload_tx: mpsc::Sender<()>,
) -> Option<crate::watcher::WatcherHandle> {
    let (handle, mut rx) = crate::watcher::spawn_dirs(&dirs, false, Vec::new()).ok()?;
    tokio::spawn(async move {
        // Never-cancelled: the task ends when `rx` closes (handle dropped
        // at session shutdown).
        let cancel = CancellationToken::new();
        let mut deb = super::watch::Debouncer::new(
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(800),
        );
        while let Some(batch) = deb.next_batch(&mut rx, &cancel).await {
            let config_changed = batch.iter().any(|p| {
                matches!(
                    p.file_name().and_then(|n| n.to_str()),
                    Some("giant.yaml" | "giant.json")
                )
            });
            if config_changed && reload_tx.send(()).await.is_err() {
                break; // session gone
            }
        }
    });
    Some(handle)
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
    std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
}

/// Bundle of state the watch loop needs. Wrapper because clippy
/// complains about 8 positional arguments otherwise - and the
/// structure clusters "everything the engine shares" vs the
/// per-watch selection/cancellation.
struct WatchCtx {
    graph: Arc<BuildGraph>,
    cache: LocalCache,
    workspace_root: AbsPath,
    cache_root: AbsPath,
    state_dir: std::path::PathBuf,
    parallelism: usize,
    log_capture: crate::executor::LogCapture,
    fresh: bool,
    sandbox: Option<crate::executor::SandboxPolicy>,
    event_tx: EventSender,
}

/// The file-watching loop for `watch.start`. Runs the initial build,
/// then spins on debounced file events, running affected rebuilds
/// until cancelled. All events flow back through `event_tx` so the
/// porcelain sees normal `build.started` / `build.finished` cycles
/// with distinct `b_w_<n>` ids.
async fn watch_loop(ctx: WatchCtx, selection: Vec<TargetId>, cancel: CancellationToken) {
    let WatchCtx {
        graph,
        cache,
        workspace_root,
        cache_root,
        state_dir,
        parallelism,
        log_capture,
        fresh,
        sandbox,
        event_tx,
    } = ctx;
    // One build per change cycle. Watch rebuilds pass no remote handles -
    // rapid local iteration shouldn't push to the shared cache (the
    // one-shot `start_build` does upload). Everything else mirrors the
    // session's configured build.
    let watch_job = |build_id: String, selection: Vec<TargetId>| BuildJob {
        graph: graph.clone(),
        selection,
        cache: cache.clone(),
        workspace_root: workspace_root.clone(),
        parallelism,
        fresh,
        events: event_tx.clone(),
        cancel: cancel.clone(),
        build_id,
        log_capture,
        sandbox: sandbox.clone(),
        #[cfg(feature = "remote")]
        remote: None,
        #[cfg(feature = "remote")]
        upload_tx: None,
    };
    let mut cycle: u64 = 1;
    let _ = build(watch_job(format!("b_w_{cycle:04x}"), selection.clone())).await;

    // Watcher excludes: anything we write (cache, declared outputs)
    // plus .git / .giant to keep noise out.
    let excludes =
        super::watch::standard_excludes(&workspace_root, &cache_root, &state_dir, &graph);
    let (_w_handle, mut rx) = match crate::watcher::spawn(workspace_root.as_path(), excludes) {
        Ok(p) => p,
        Err(e) => {
            let _ = event_tx
                .send(Event::CommandError {
                    command_id: "watch".into(),
                    message: format!("could not start file watcher: {e}"),
                })
                .await;
            return;
        }
    };

    // Debounce + affected-filter are the shared watch mechanics
    // (`next_affected_cycle`); only the per-cycle build differs from the
    // CLI's `build --watch`.
    let mut debouncer = super::watch::Debouncer::new(
        std::time::Duration::from_millis(100),
        std::time::Duration::from_millis(500),
    );
    while let Some(to_build) = super::watch::next_affected_cycle(
        &graph,
        &mut rx,
        &mut debouncer,
        &selection,
        &workspace_root,
        &cancel,
    )
    .await
    {
        // Announce what this change cycle affects (empty = a real change
        // touched nothing in the selection) so consumers - the CLI tty
        // renderer, a TUI - can show it. Empty cycles run no build.
        let _ = event_tx
            .send(Event::WatchAffected {
                target_ids: to_build.clone(),
            })
            .await;
        if to_build.is_empty() {
            continue;
        }
        cycle += 1;
        let _ = build(watch_job(format!("b_w_{cycle:04x}"), to_build)).await;
    }
}

/// State shared with the affected subscription task. Mirrors
/// `WatchCtx` - same workspace + cache_root excludes - but without
/// the executor plumbing since we only compute the set, not build.
struct AffectedCtx {
    graph: Arc<BuildGraph>,
    workspace_root: AbsPath,
    cache_root: AbsPath,
    state_dir: std::path::PathBuf,
    event_tx: EventSender,
}

/// Long-running task that owns one affected subscription.
///
/// 1. Compute the affected set against `base`, emit one
///    `affected.changed` (or `affected.error` if git fails).
/// 2. Spin up a file watcher with the same excludes as `watch.start`.
/// 3. On every debounced batch, recompute and re-emit *only when the
///    set actually changed* (saves the client's renderer from
///    spurious redraws).
async fn affected_loop(ctx: AffectedCtx, base: String, cancel: CancellationToken) {
    let AffectedCtx {
        graph,
        workspace_root,
        cache_root,
        state_dir,
        event_tx,
    } = ctx;

    let mut last: Option<Vec<TargetId>> = match compute_affected(&graph, &workspace_root, &base) {
        Ok(ids) => {
            let _ = event_tx
                .send(Event::AffectedChanged {
                    base: base.clone(),
                    target_ids: ids.clone(),
                })
                .await;
            Some(ids)
        }
        Err(e) => {
            let _ = event_tx
                .send(Event::AffectedError {
                    base: base.clone(),
                    message: e,
                })
                .await;
            None
        }
    };

    // Same exclusions as watch_loop - declared outputs, .git, .giant,
    // cache root - so the engine's own writes don't loop the watcher.
    let excludes =
        super::watch::standard_excludes(&workspace_root, &cache_root, &state_dir, &graph);
    let (_w_handle, mut rx) = match crate::watcher::spawn(workspace_root.as_path(), excludes) {
        Ok(p) => p,
        Err(e) => {
            let _ = event_tx
                .send(Event::AffectedError {
                    base: base.clone(),
                    message: format!("file watcher failed: {e}"),
                })
                .await;
            // Wait for cancellation rather than busy-looping. A
            // missing watcher means no refreshes; the snapshot above
            // is the user's only datapoint until they re-subscribe.
            cancel.cancelled().await;
            return;
        }
    };

    let mut debouncer = super::watch::Debouncer::new(
        std::time::Duration::from_millis(200),
        std::time::Duration::from_millis(800),
    );

    loop {
        if cancel.is_cancelled() {
            break;
        }
        let batch = match debouncer.next_batch(&mut rx, &cancel).await {
            Some(b) if !b.is_empty() => b,
            Some(_) => continue,
            None => break,
        };
        // Drop the batch contents - we recompute the *whole* set
        // from git on each tick. The batch is just our trigger.
        drop(batch);

        match compute_affected(&graph, &workspace_root, &base) {
            Ok(ids) => {
                if last.as_ref() != Some(&ids) {
                    let _ = event_tx
                        .send(Event::AffectedChanged {
                            base: base.clone(),
                            target_ids: ids.clone(),
                        })
                        .await;
                    last = Some(ids);
                }
            }
            Err(e) => {
                let _ = event_tx
                    .send(Event::AffectedError {
                        base: base.clone(),
                        message: e,
                    })
                    .await;
                last = None;
            }
        }
    }
}

/// Compute "affected since <base>" in-process. Mirrors
/// `cli::affected::execute` minus the I/O - git diff for changed
/// paths, then the graph intersection. Returns user-facing error
/// strings (the session emits them as `affected.error.message`).
fn compute_affected(
    graph: &BuildGraph,
    workspace_root: &AbsPath,
    base: &str,
) -> std::result::Result<Vec<TargetId>, String> {
    let changed = crate::git::affected_files_since(workspace_root.as_path(), base)
        .map_err(|e| format!("git diff against {base}: {e}"))?;
    let changed_refs: Vec<&std::path::Path> = changed.iter().map(|p| p.as_path()).collect();
    let affected = crate::selection::affected_targets(graph, &changed_refs);
    let mut out: Vec<TargetId> = affected.into_iter().collect();
    out.sort();
    Ok(out)
}

/// State for a notify-only `watch.subscribe`. Like `AffectedCtx` (same
/// excludes, no executor plumbing), but scoped by targets+globs instead
/// of a git base, and it emits `watch.changed` rather than computing an
/// affected set.
struct ChangeCtx {
    graph: Arc<BuildGraph>,
    workspace_root: AbsPath,
    cache_root: AbsPath,
    state_dir: std::path::PathBuf,
    event_tx: EventSender,
}

/// Long-running task owning one `watch.subscribe`. Watches the workspace
/// (same exclusions as the affected loop) and emits `watch.changed` on
/// each debounced batch relevant to `targets ∪ globs`. Never builds.
async fn watch_subscribe_loop(
    ctx: ChangeCtx,
    targets: Vec<TargetId>,
    globs: Vec<String>,
    cancel: CancellationToken,
) {
    let ChangeCtx {
        graph,
        workspace_root,
        cache_root,
        state_dir,
        event_tx,
    } = ctx;
    let requested: std::collections::HashSet<TargetId> = targets.into_iter().collect();
    // Globs were validated at subscribe; recompile, dropping any that
    // somehow don't (belt and braces - a bad one just never matches).
    let matchers: Vec<glob::Pattern> = globs
        .iter()
        .filter_map(|g| glob::Pattern::new(g).ok())
        .collect();

    // Same exclusions as `affected_loop` - declared outputs, .git,
    // .giant, cache root - so the engine's own writes don't loop us.
    let excludes =
        super::watch::standard_excludes(&workspace_root, &cache_root, &state_dir, &graph);
    let (_w_handle, mut rx) = match crate::watcher::spawn(workspace_root.as_path(), excludes) {
        Ok(p) => p,
        Err(_) => {
            // No watcher → no notifications possible. Hold until
            // cancelled rather than busy-looping. (Notify-only: there is
            // nothing to report and no snapshot to fall back on.)
            cancel.cancelled().await;
            return;
        }
    };

    let mut debouncer = super::watch::Debouncer::new(
        std::time::Duration::from_millis(200),
        std::time::Duration::from_millis(800),
    );

    loop {
        if cancel.is_cancelled() {
            break;
        }
        let batch = match debouncer.next_batch(&mut rx, &cancel).await {
            Some(b) if !b.is_empty() => b,
            Some(_) => continue,
            None => break,
        };
        if let Some(paths) = relevant(&graph, &requested, &matchers, &batch, &workspace_root) {
            let _ = event_tx.send(Event::WatchChanged { paths }).await;
        }
    }
}

/// Decide whether a debounced `batch` is relevant to a subscription
/// scope, returning the in-scope paths to report (workspace-relative,
/// sorted) or `None` to stay silent.
///
/// - Empty `requested` and `matchers` = whole-workspace: any non-empty
///   batch is relevant.
/// - Otherwise relevant when a changed path feeds one of the requested
///   targets - via `affected_targets`, so a change to a *transitive*
///   dependency counts - or matches a glob.
///
/// v1 reports the whole batch on a hit (the paths are advisory; the
/// client's signal is the event). See TDD-0019 for per-path attribution.
fn relevant(
    graph: &BuildGraph,
    requested: &std::collections::HashSet<TargetId>,
    matchers: &[glob::Pattern],
    batch: &[std::path::PathBuf],
    workspace_root: &AbsPath,
) -> Option<Vec<String>> {
    if batch.is_empty() {
        return None;
    }
    // The watcher emits absolute paths; target input globs and our glob
    // matchers are workspace-relative, so strip the root first.
    let rel: Vec<std::path::PathBuf> = batch
        .iter()
        .map(|p| {
            p.strip_prefix(workspace_root.as_path())
                .unwrap_or(p)
                .to_path_buf()
        })
        .collect();
    // Decide relevance before paying for the (sorted, owned) payload.
    // `target_hit` is a full-graph scan, so let a cheap glob match
    // short-circuit it.
    let glob_hit = || {
        rel.iter()
            .any(|p| matchers.iter().any(|m| m.matches(&p.to_string_lossy())))
    };
    let target_hit = || {
        let refs: Vec<&std::path::Path> = rel.iter().map(|p| p.as_path()).collect();
        !requested.is_disjoint(&crate::selection::affected_targets(graph, &refs))
    };
    let whole_workspace = requested.is_empty() && matchers.is_empty();
    if !(whole_workspace || glob_hit() || target_hit()) {
        return None;
    }

    let mut paths: Vec<String> = rel
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    paths.sort();
    Some(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Input, TargetSpec};
    use crate::paths::{OutputPath, WsRelPath};
    use crate::types::GlobPattern;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn spec(id: &str, deps: &[&str], outputs: &[&str], inputs: &[&str]) -> TargetSpec {
        let name = id.rsplit([':', '/']).next().unwrap_or(id);
        TargetSpec {
            name: name.to_string(),
            id: TargetId::new(id),
            inputs: inputs
                .iter()
                .map(|g| Input::File {
                    glob: GlobPattern::new(*g).unwrap(),
                })
                .collect(),
            outputs_raw: Vec::new(),
            outputs: outputs
                .iter()
                .map(|o| OutputPath::new(*o).unwrap())
                .collect(),
            deps: deps.iter().map(|d| TargetId::new(*d)).collect(),
            command: "true".into(),
            cwd_raw: None,
            cwd: WsRelPath::default(),
            env: Default::default(),
            cache: Some(true),
            remote_cache: true,
            network: false,
            sandbox: true,
            exists: None,
            timeout_secs: None,
            test: false,
            tags: Default::default(),
            label: None,
            prune_dirs: Vec::new(),
        }
    }

    fn graph_with(specs: Vec<TargetSpec>) -> BuildGraph {
        let mut g = BuildGraph::new();
        for s in specs {
            g.add_target(s).unwrap();
        }
        g.build_edges_and_validate().unwrap();
        g
    }

    fn ws() -> AbsPath {
        AbsPath::new("/ws")
    }

    // The watcher emits absolute paths; relevance strips the root.
    fn abs(p: &str) -> PathBuf {
        PathBuf::from("/ws").join(p)
    }

    #[test]
    fn empty_batch_is_silent() {
        let g = graph_with(vec![]);
        assert_eq!(relevant(&g, &HashSet::new(), &[], &[], &ws()), None);
    }

    #[test]
    fn empty_scope_watches_whole_workspace() {
        let g = graph_with(vec![]);
        let batch = [abs("anything.txt")];
        assert_eq!(
            relevant(&g, &HashSet::new(), &[], &batch, &ws()),
            Some(vec!["anything.txt".to_string()]),
        );
    }

    #[test]
    fn glob_hit_reports_workspace_relative_paths() {
        let g = graph_with(vec![]);
        let m = [glob::Pattern::new("tests/e2e/**/*.go").unwrap()];
        let batch = [abs("tests/e2e/login_test.go")];
        assert_eq!(
            relevant(&g, &HashSet::new(), &m, &batch, &ws()),
            Some(vec!["tests/e2e/login_test.go".to_string()]),
        );
    }

    #[test]
    fn glob_miss_with_no_targets_is_silent() {
        let g = graph_with(vec![]);
        let m = [glob::Pattern::new("tests/**/*.go").unwrap()];
        let batch = [abs("src/main.rs")];
        assert_eq!(relevant(&g, &HashSet::new(), &m, &batch, &ws()), None);
    }

    #[test]
    fn direct_target_hit_is_relevant() {
        let g = graph_with(vec![spec("go:bin", &[], &["bin/server"], &["cmd/**/*.go"])]);
        let req: HashSet<TargetId> = [TargetId::new("go:bin")].into();
        let batch = [abs("cmd/server/main.go")];
        assert!(relevant(&g, &req, &[], &batch, &ws()).is_some());
    }

    #[test]
    fn transitive_dep_change_triggers_requested_target() {
        // go:bin depends on go:lib; a change to go:lib's source must count
        // - the whole reason for this feature.
        let g = graph_with(vec![
            spec("go:lib", &[], &["lib.a"], &["internal/**/*.go"]),
            spec("go:bin", &["go:lib"], &["bin/server"], &["cmd/**/*.go"]),
        ]);
        let req: HashSet<TargetId> = [TargetId::new("go:bin")].into();
        let batch = [abs("internal/store/db.go")];
        assert!(
            relevant(&g, &req, &[], &batch, &ws()).is_some(),
            "a change to a transitive dependency must be relevant",
        );
    }

    #[test]
    fn unrelated_change_is_silent() {
        let g = graph_with(vec![spec("go:bin", &[], &["bin/server"], &["cmd/**/*.go"])]);
        let req: HashSet<TargetId> = [TargetId::new("go:bin")].into();
        let batch = [abs("docs/readme.md")];
        assert_eq!(relevant(&g, &req, &[], &batch, &ws()), None);
    }
}
