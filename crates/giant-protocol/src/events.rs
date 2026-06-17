//! NDJSON event protocol.
//!
//! for the full schema and command channel. This module
//! defines the in-engine `Event` enum that gets serialized to the wire.

use crate::TargetId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Event {
    #[serde(rename = "engine.hello")]
    EngineHello {
        version: String,
        protocol: u32,
        workspace: String,
        /// Read/query capabilities (protocol 2). Absent or empty
        /// means a protocol-1 engine: structural catalog + build events only,
        /// no pull queries.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },

    #[serde(rename = "engine.shutdown")]
    EngineShutdown {
        reason: ShutdownReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    #[serde(rename = "config.loaded")]
    ConfigLoaded {
        workspace_name: String,
        target_count: usize,
    },

    #[serde(rename = "config.error")]
    ConfigError {
        file: String,
        line: Option<u32>,
        column: Option<u32>,
        message: String,
    },

    #[serde(rename = "build.started")]
    BuildStarted {
        id: String,
        selection: Vec<String>,
        target_ids: Vec<TargetId>,
        parallelism: usize,
    },

    #[serde(rename = "build.finished")]
    BuildFinished {
        id: String,
        ok: bool,
        duration_ms: u64,
        counts: TargetCounts,
    },

    #[serde(rename = "target.queued")]
    TargetQueued {
        build: String,
        id: TargetId,
        deps: Vec<TargetId>,
    },

    #[serde(rename = "target.started")]
    TargetStarted {
        build: String,
        id: TargetId,
        cache_key: String,
        command: String,
    },

    #[serde(rename = "target.log")]
    TargetLog {
        build: String,
        id: TargetId,
        stream: LogStream,
        line: String,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },

    #[serde(rename = "target.finished")]
    TargetFinished {
        build: String,
        id: TargetId,
        result: TargetResultKind,
        duration_ms: u64,
        exit_code: Option<i32>,
        outputs: Vec<String>,
        error: Option<String>,
    },

    #[serde(rename = "watch.started")]
    WatchStarted { filter: Option<String> },

    #[serde(rename = "watch.batch")]
    WatchBatch {
        paths: Vec<String>,
        more: usize,
        config_changed: bool,
    },

    #[serde(rename = "watch.affected")]
    WatchAffected { target_ids: Vec<TargetId> },

    #[serde(rename = "watch.state")]
    WatchState { state: WatchStateKind },

    #[serde(rename = "watch.stopped")]
    WatchStopped,

    /// A debounced batch touched a `watch.subscribe` scope. `paths` are
    /// the in-scope changed paths (workspace-relative), advisory - the
    /// client's signal is the event itself. Notify-only: no build runs.
    #[serde(rename = "watch.changed")]
    WatchChanged { paths: Vec<String> },

    /// Describes one discovered package: its workspace-relative directory
    /// (`""` for the root) and the workspace-relative path to its primary
    /// config. Emitted in the catalog stream before the targets, including
    /// packages that contribute no targets (a tasks-only directory), so
    /// porcelains learn the package layout without re-walking the tree.
    #[serde(rename = "package.described")]
    PackageDescribed { package: String, config: String },

    /// Describes one target in the merged graph. Emitted by
    /// `giant list --events ndjson` and (when it lands) by the
    /// `query.catalog` channel of a long-running engine. Exporting
    /// the merged graph is a separate concern from build events; this
    /// event is *only* used in catalog streams, never during a build.
    #[serde(rename = "target.described")]
    TargetDescribed {
        id: TargetId,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        test: bool,
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inputs: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        outputs: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        deps: Vec<TargetId>,
    },

    #[serde(rename = "protocol.dropped")]
    ProtocolDropped {
        count: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<TargetId>,
    },

    // ----- Session-mode events ----------------------------
    //
    // Emitted after the initial catalog stream finishes, indicating the
    // session is now ready to accept commands on stdin.
    #[serde(rename = "engine.ready")]
    EngineReady,

    /// Engine acknowledges a command and (if the command starts a
    /// build) reports the assigned build id.
    #[serde(rename = "command.accepted")]
    CommandAccepted {
        command_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<String>,
    },

    /// Engine refuses a command - bad schema, conflicting state,
    /// unknown target, etc. The build never starts.
    #[serde(rename = "command.rejected")]
    CommandRejected { command_id: String, reason: String },

    /// Engine accepted a command but it failed during execution.
    /// Different from `build.finished { ok: false }` (which is a
    /// targeted-failure event from the executor); this is for command-
    /// level failures like "config couldn't load."
    #[serde(rename = "command.error")]
    CommandError { command_id: String, message: String },

    /// Catalog is about to be invalidated and re-emitted (config
    /// changed). Porcelains should hold off on rendering until
    /// `catalog.ready` arrives.
    #[serde(rename = "catalog.invalidating")]
    CatalogInvalidating,

    /// Catalog re-emission is complete. The most recent
    /// `target.described` events form the current catalog.
    #[serde(rename = "catalog.ready")]
    CatalogReady,

    /// The "affected since <base>" set was (re)computed. Fired once
    /// per `affected.subscribe` (the initial snapshot), then again
    /// whenever a file change causes the set to change. The session
    /// keeps a file watcher pinned for the lifetime of the
    /// subscription; clients just render the latest `target_ids`.
    #[serde(rename = "affected.changed")]
    AffectedChanged {
        base: String,
        target_ids: Vec<TargetId>,
    },

    /// `affected.subscribe` couldn't compute. Most often a bad git
    /// ref, but covers any failure from `git::affected_files_since`.
    /// The subscription stays alive; a follow-up file change might
    /// succeed (e.g. user `git fetch`-es the missing ref).
    #[serde(rename = "affected.error")]
    AffectedError { base: String, message: String },

    /// Reply to `query.status`. Per-target cache state, correlated
    /// by `command_id`.
    #[serde(rename = "query.status")]
    QueryStatus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<String>,
        targets: Vec<TargetStatus>,
    },

    /// One captured log line replayed for `logs.get`, correlated by
    /// `command_id`. Distinct from `target.log`, which is live build output.
    #[serde(rename = "logs.line")]
    LogsLine {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<String>,
        target: TargetId,
        stream: LogStream,
        line: String,
    },

    /// End of a `logs.get` replay (always sent, even when there were no logs).
    #[serde(rename = "logs.end")]
    LogsEnd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<String>,
        target: TargetId,
    },

    /// Reply to `query.explain`: what feeds a target's cache key,
    /// and whether it is currently cached. The structured form of `giant
    /// explain`, for an inline "why did this run / why cached" view.
    #[serde(rename = "query.explained")]
    QueryExplained {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<String>,
        target: TargetId,
        key: String,
        cached: bool,
        command: String,
        cwd: String,
        file_inputs: Vec<ExplainInput>,
        deps: Vec<ExplainDep>,
        env: Vec<ExplainEnv>,
        /// Present when `cached` - the cached action's metadata and outputs, so a
        /// renderer can show "HIT (built X, Yms, exit Z)" and the output list
        /// without a second round-trip. Absent on a miss.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_hit: Option<ExplainCacheHit>,
    },
}

/// The cached action behind a `query.explained` hit: metadata plus the produced
/// outputs. Mirrors the action-cache entry, trimmed to what `giant explain`
/// renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainCacheHit {
    pub built_at: String,
    pub duration_ms: u64,
    pub exit_code: i32,
    pub outputs: Vec<ExplainOutput>,
    /// The aggregate hash of all outputs - the value a dependent target folds
    /// into its own cache key.
    pub outputs_content_hash: String,
}

/// One produced output in a `query.explained` cache hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainOutput {
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub mode: String,
}

/// A file input contributing to a target's cache key (`query.explained`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainInput {
    pub path: String,
    pub hash: String,
    pub size: u64,
}

/// A dep's output hash contributing to a target's cache key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainDep {
    pub id: TargetId,
    pub output_hash: String,
}

/// An environment variable contributing to a target's cache key. `built_in`
/// marks engine-provided vars (vs the target's declared `env`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainEnv {
    pub key: String,
    pub value: String,
    pub built_in: bool,
}

/// One target's cache state in a `query.status` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetStatus {
    pub id: TargetId,
    /// `cached` (the action cache has an entry at the current key) or `stale`
    /// (no entry at the current key - never built, or built at a different key).
    pub state: String,
    pub key: String,
    /// Duration of the build behind the cached entry, when `state == cached`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_duration_ms: Option<u64>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    Graceful,
    Signal,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetResultKind {
    Built,
    CacheHit,
    RemoteCacheHit,
    ExternalCacheHit,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchStateKind {
    Idle,
    Building,
    BuildingWithPending,
    ReloadingConfig,
    ConfigError,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetCounts {
    pub built: u32,
    pub cache_hit: u32,
    pub failed: u32,
    pub skipped: u32,
}

/// Best-effort sender that drops log events first under backpressure.
pub type EventSender = tokio::sync::mpsc::Sender<Event>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_hello_serializes_with_t_tag() {
        let ev = Event::EngineHello {
            version: "0.1.0".into(),
            protocol: 1,
            workspace: "/tmp/proj".into(),
            capabilities: Vec::new(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"t\":\"engine.hello\""));
        assert!(s.contains("\"version\":\"0.1.0\""));
    }

    #[test]
    fn target_described_round_trips_through_json() {
        let ev = Event::TargetDescribed {
            id: TargetId::new("go:bin:server"),
            tags: vec!["release".into(), "smoke".into()],
            test: false,
            command: "go build -o bin/server".into(),
            inputs: vec!["src/**/*.go".into()],
            outputs: vec!["bin/server".into()],
            deps: vec![TargetId::new("proto:api")],
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"t\":\"target.described\""));
        assert!(s.contains("\"id\":\"go:bin:server\""));
        // test:false should be elided.
        assert!(!s.contains("\"test\""), "test:false should be skipped");
        let back: Event = serde_json::from_str(&s).unwrap();
        match back {
            Event::TargetDescribed { id, tags, deps, .. } => {
                assert_eq!(id.as_str(), "go:bin:server");
                assert_eq!(tags, vec!["release", "smoke"]);
                assert_eq!(deps.len(), 1);
            }
            _ => panic!("wrong variant after round-trip"),
        }
    }

    #[test]
    fn engine_hello_serializes_capabilities() {
        let ev = Event::EngineHello {
            version: "0.2.0".into(),
            protocol: 2,
            workspace: "/tmp/p".into(),
            capabilities: vec!["query.status".into()],
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"protocol\":2"));
        assert!(s.contains("\"capabilities\":[\"query.status\"]"));
    }

    #[test]
    fn query_status_round_trips() {
        let ev = Event::QueryStatus {
            command_id: Some("q1".into()),
            targets: vec![TargetStatus {
                id: TargetId::new("//:a"),
                state: "cached".into(),
                key: "7a3f".into(),
                last_duration_ms: Some(342),
            }],
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"t\":\"query.status\""));
        assert!(s.contains("\"command_id\":\"q1\""));
        let back: Event = serde_json::from_str(&s).unwrap();
        match back {
            Event::QueryStatus { targets, .. } => {
                assert_eq!(targets.len(), 1);
                assert_eq!(targets[0].state, "cached");
                assert_eq!(targets[0].last_duration_ms, Some(342));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn logs_line_and_end_round_trip() {
        let line = Event::LogsLine {
            command_id: Some("q6".into()),
            target: TargetId::new("//:a"),
            stream: LogStream::Stdout,
            line: "writing bin/server".into(),
        };
        let s = serde_json::to_string(&line).unwrap();
        assert!(s.contains("\"t\":\"logs.line\""));
        assert!(s.contains("\"stream\":\"stdout\""));
        assert!(s.contains("\"command_id\":\"q6\""));

        let end = Event::LogsEnd {
            command_id: Some("q6".into()),
            target: TargetId::new("//:a"),
        };
        let s2 = serde_json::to_string(&end).unwrap();
        assert!(s2.contains("\"t\":\"logs.end\""));
        // both deserialize back.
        assert!(matches!(
            serde_json::from_str::<Event>(&s).unwrap(),
            Event::LogsLine { .. }
        ));
        assert!(matches!(
            serde_json::from_str::<Event>(&s2).unwrap(),
            Event::LogsEnd { .. }
        ));
    }

    #[test]
    fn query_explained_round_trips() {
        let ev = Event::QueryExplained {
            command_id: Some("e1".into()),
            target: TargetId::new("//:a"),
            key: "7a3f".into(),
            cached: true,
            command: "go build".into(),
            cwd: "".into(),
            file_inputs: vec![ExplainInput {
                path: "src/main.go".into(),
                hash: "abcd".into(),
                size: 12,
            }],
            deps: vec![ExplainDep {
                id: TargetId::new("//:lib"),
                output_hash: "ef01".into(),
            }],
            env: vec![ExplainEnv {
                key: "PATH".into(),
                value: "/bin".into(),
                built_in: true,
            }],
            cache_hit: Some(ExplainCacheHit {
                built_at: "2026-06-08T00:00:00Z".into(),
                duration_ms: 42,
                exit_code: 0,
                outputs: vec![ExplainOutput {
                    path: "bin/server".into(),
                    hash: "c0ffee".into(),
                    size: 1024,
                    mode: "100755".into(),
                }],
                outputs_content_hash: "deadbeef".into(),
            }),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"t\":\"query.explained\""));
        assert!(s.contains("\"cached\":true"));
        match serde_json::from_str::<Event>(&s).unwrap() {
            Event::QueryExplained {
                command,
                file_inputs,
                deps,
                env,
                cache_hit,
                ..
            } => {
                assert_eq!(command, "go build");
                assert_eq!(file_inputs.len(), 1);
                assert_eq!(deps[0].id.as_str(), "//:lib");
                assert!(env[0].built_in);
                let hit = cache_hit.expect("cache_hit present");
                assert_eq!(hit.duration_ms, 42);
                assert_eq!(hit.outputs[0].path, "bin/server");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn target_log_truncated_flag_omitted_when_false() {
        let ev = Event::TargetLog {
            build: "b_1".into(),
            id: TargetId::new("foo"),
            stream: LogStream::Stdout,
            line: "hello".into(),
            truncated: false,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(
            !s.contains("truncated"),
            "truncated:false should be skipped"
        );
    }
}
