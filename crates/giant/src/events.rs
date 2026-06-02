//! NDJSON event protocol.
//!
//! See TDD-0004 for the full schema and command channel. This module
//! defines the in-engine `Event` enum that gets serialized to the wire.

use crate::model::TargetId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Event {
    #[serde(rename = "engine.hello")]
    EngineHello {
        version: String,
        protocol: u32,
        workspace: String,
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

    // ----- Session-mode events (TDD-0014) ----------------------------
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

/// Best-effort sender that drops log events first under backpressure (TDD-0004).
pub type EventSender = tokio::sync::mpsc::Sender<Event>;

/// Unused right now; placeholder so lib.rs re-export resolves.
#[allow(dead_code)]
pub fn _ev_module_marker() -> HashMap<String, ()> {
    HashMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_hello_serializes_with_t_tag() {
        let ev = Event::EngineHello {
            version: "0.1.0".into(),
            protocol: 1,
            workspace: "/tmp/proj".into(),
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
