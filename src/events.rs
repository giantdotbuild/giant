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

    #[serde(rename = "discovery.merged")]
    DiscoveryMerged {
        build: String,
        id: TargetId,
        added_targets: Vec<TargetId>,
    },

    #[serde(rename = "protocol.dropped")]
    ProtocolDropped {
        count: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<TargetId>,
    },
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
