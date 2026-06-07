//! Session-mode command channel.
//!
//! The engine's stdio protocol (TDD-0014) accepts JSON commands on
//! stdin. Each command is one JSON object per line, with a `c` field
//! naming the type. Commands optionally carry a `command_id` that the
//! engine echoes back on `command.accepted` / `command.rejected` so
//! the client can correlate the response with the request.
//!
//! See TDD-0004 §Command types for the full wire schema.

use crate::model::TargetId;
use serde::{Deserialize, Serialize};

/// One command from a session client. Tagged on the `c` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "c", rename_all = "snake_case")]
pub enum Command {
    /// Start a build of the given target ids. The selection is
    /// explicit ids; the engine doesn't re-evaluate selection
    /// language at this layer (the client did that).
    #[serde(rename = "build")]
    Build {
        #[serde(default)]
        command_id: Option<String>,
        targets: Vec<TargetId>,
        #[serde(default)]
        fresh: bool,
    },

    /// Cancel an in-flight or queued build by id.
    #[serde(rename = "cancel")]
    Cancel {
        #[serde(default)]
        command_id: Option<String>,
        build: String,
    },

    /// Start a watch loop on the given target ids. File changes
    /// affecting the selection's deps trigger rebuilds.
    #[serde(rename = "watch.start")]
    WatchStart {
        #[serde(default)]
        command_id: Option<String>,
        targets: Vec<TargetId>,
    },

    /// Stop the active watch loop. No-op if none is active.
    #[serde(rename = "watch.stop")]
    WatchStop {
        #[serde(default)]
        command_id: Option<String>,
    },

    /// Re-read the workspace config, rebuild the graph, emit a fresh
    /// catalog stream. Triggered explicitly or by a giant.yaml file
    /// change (engine internal).
    #[serde(rename = "config.reload")]
    ConfigReload {
        #[serde(default)]
        command_id: Option<String>,
    },

    /// Graceful shutdown. Engine drains in-flight builds and exits.
    /// Equivalent to closing stdin.
    #[serde(rename = "shutdown")]
    Shutdown {
        #[serde(default)]
        command_id: Option<String>,
    },

    /// Subscribe to the "affected since <base>" target set. The
    /// engine computes the set immediately, emits one
    /// `affected.changed` event, and keeps a file watcher pinned so
    /// it can re-emit whenever the set actually changes.
    ///
    /// At most one subscription is active per session. A second
    /// `affected.subscribe` replaces the first; the old watcher is
    /// torn down.
    #[serde(rename = "affected.subscribe")]
    AffectedSubscribe {
        #[serde(default)]
        command_id: Option<String>,
        base: String,
    },

    /// End the active affected subscription, if any. No-op if none.
    #[serde(rename = "affected.unsubscribe")]
    AffectedUnsubscribe {
        #[serde(default)]
        command_id: Option<String>,
    },

    /// Notify-only watch. The engine watches the inputs of `targets`
    /// (graph-expanded, transitively) plus `globs`, and emits
    /// `watch.changed` on each relevant batch - it never builds. Empty
    /// `targets` and `globs` watches the whole workspace.
    ///
    /// At most one is active per session; a second `watch.subscribe`
    /// replaces the first. Independent of builds and `watch.start`.
    #[serde(rename = "watch.subscribe")]
    WatchSubscribe {
        #[serde(default)]
        command_id: Option<String>,
        #[serde(default)]
        targets: Vec<TargetId>,
        #[serde(default)]
        globs: Vec<String>,
    },

    /// End the active watch subscription, if any. No-op if none.
    #[serde(rename = "watch.unsubscribe")]
    WatchUnsubscribe {
        #[serde(default)]
        command_id: Option<String>,
    },

    /// Read query: per-target cache state (ADR-0033). Computes each target's
    /// cache key and consults the action cache. Empty `targets` means every
    /// target (may be slow on a large graph). Answered with `query.status`.
    #[serde(rename = "query.status")]
    QueryStatus {
        #[serde(default)]
        command_id: Option<String>,
        #[serde(default)]
        targets: Vec<TargetId>,
    },

    /// Read query: replay a target's captured logs from the last cached build
    /// (ADR-0033). Streams `logs.line` events then `logs.end`. `follow` (live
    /// tail of a running target) is not yet implemented; it replays regardless.
    #[serde(rename = "logs.get")]
    LogsGet {
        #[serde(default)]
        command_id: Option<String>,
        target: TargetId,
        #[serde(default)]
        follow: bool,
    },

    /// Read query: what feeds a target's cache key, and whether it is cached
    /// (ADR-0033). Answered with `query.explained` - the structured form of
    /// `giant explain`.
    #[serde(rename = "query.explain")]
    QueryExplain {
        #[serde(default)]
        command_id: Option<String>,
        target: TargetId,
    },
}

impl Command {
    /// The `command_id` carried by this command, if any.
    pub fn command_id(&self) -> Option<&str> {
        match self {
            Command::Build { command_id, .. }
            | Command::Cancel { command_id, .. }
            | Command::WatchStart { command_id, .. }
            | Command::WatchStop { command_id, .. }
            | Command::ConfigReload { command_id, .. }
            | Command::Shutdown { command_id, .. }
            | Command::AffectedSubscribe { command_id, .. }
            | Command::AffectedUnsubscribe { command_id, .. }
            | Command::WatchSubscribe { command_id, .. }
            | Command::WatchUnsubscribe { command_id, .. }
            | Command::QueryStatus { command_id, .. }
            | Command::LogsGet { command_id, .. }
            | Command::QueryExplain { command_id, .. } => command_id.as_deref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_command_parses_with_target_list() {
        let raw = r#"{"c":"build","command_id":"c_1","targets":["go:bin:server","docker:api"]}"#;
        let cmd: Command = serde_json::from_str(raw).unwrap();
        match cmd {
            Command::Build {
                command_id,
                targets,
                fresh,
            } => {
                assert_eq!(command_id.as_deref(), Some("c_1"));
                assert_eq!(targets.len(), 2);
                assert_eq!(targets[0].as_str(), "go:bin:server");
                assert!(!fresh);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn build_command_fresh_defaults_to_false() {
        let raw = r#"{"c":"build","targets":["a"]}"#;
        let cmd: Command = serde_json::from_str(raw).unwrap();
        match cmd {
            Command::Build { fresh, .. } => assert!(!fresh),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn cancel_command_parses() {
        let raw = r#"{"c":"cancel","build":"b_1234"}"#;
        let cmd: Command = serde_json::from_str(raw).unwrap();
        assert!(matches!(cmd, Command::Cancel { ref build, .. } if build == "b_1234"));
    }

    #[test]
    fn watch_start_command_parses() {
        let raw = r#"{"c":"watch.start","targets":["go:bin:server"]}"#;
        let cmd: Command = serde_json::from_str(raw).unwrap();
        assert!(matches!(cmd, Command::WatchStart { ref targets, .. } if targets.len() == 1));
    }

    #[test]
    fn shutdown_command_parses_without_id() {
        let raw = r#"{"c":"shutdown"}"#;
        let cmd: Command = serde_json::from_str(raw).unwrap();
        assert!(matches!(cmd, Command::Shutdown { .. }));
    }

    #[test]
    fn watch_subscribe_parses_with_targets_and_globs() {
        let raw = r#"{"c":"watch.subscribe","command_id":"w1","targets":["go:bin:server"],"globs":["tests/e2e/**/*.go"]}"#;
        let cmd: Command = serde_json::from_str(raw).unwrap();
        match cmd {
            Command::WatchSubscribe {
                command_id,
                targets,
                globs,
            } => {
                assert_eq!(command_id.as_deref(), Some("w1"));
                assert_eq!(targets, vec![TargetId::new("go:bin:server")]);
                assert_eq!(globs, vec!["tests/e2e/**/*.go".to_string()]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn watch_subscribe_defaults_to_whole_workspace() {
        // No targets, no globs → empty vecs (the whole-workspace case).
        let cmd: Command = serde_json::from_str(r#"{"c":"watch.subscribe"}"#).unwrap();
        match cmd {
            Command::WatchSubscribe { targets, globs, .. } => {
                assert!(targets.is_empty() && globs.is_empty());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn query_status_parses_with_targets() {
        let raw = r#"{"c":"query.status","command_id":"q1","targets":["//:a","//:b"]}"#;
        let cmd: Command = serde_json::from_str(raw).unwrap();
        match cmd {
            Command::QueryStatus {
                command_id,
                targets,
            } => {
                assert_eq!(command_id.as_deref(), Some("q1"));
                assert_eq!(targets.len(), 2);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn query_status_defaults_to_all_targets() {
        let cmd: Command = serde_json::from_str(r#"{"c":"query.status"}"#).unwrap();
        match cmd {
            Command::QueryStatus { targets, .. } => assert!(targets.is_empty()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn logs_get_parses_with_follow() {
        let raw = r#"{"c":"logs.get","command_id":"q6","target":"//:a","follow":true}"#;
        let cmd: Command = serde_json::from_str(raw).unwrap();
        match cmd {
            Command::LogsGet {
                command_id,
                target,
                follow,
            } => {
                assert_eq!(command_id.as_deref(), Some("q6"));
                assert_eq!(target.as_str(), "//:a");
                assert!(follow);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn logs_get_follow_defaults_false() {
        let cmd: Command = serde_json::from_str(r#"{"c":"logs.get","target":"//:a"}"#).unwrap();
        match cmd {
            Command::LogsGet { follow, .. } => assert!(!follow),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn query_explain_parses() {
        let cmd: Command =
            serde_json::from_str(r#"{"c":"query.explain","command_id":"e1","target":"//:a"}"#)
                .unwrap();
        match cmd {
            Command::QueryExplain { command_id, target } => {
                assert_eq!(command_id.as_deref(), Some("e1"));
                assert_eq!(target.as_str(), "//:a");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn unknown_command_type_fails_to_parse() {
        let raw = r#"{"c":"explode","yield":42}"#;
        assert!(serde_json::from_str::<Command>(raw).is_err());
    }

    #[test]
    fn command_id_accessor_works_for_each_variant() {
        let cmds: Vec<Command> = vec![
            serde_json::from_str(r#"{"c":"build","command_id":"a","targets":[]}"#).unwrap(),
            serde_json::from_str(r#"{"c":"cancel","command_id":"b","build":"b1"}"#).unwrap(),
            serde_json::from_str(r#"{"c":"shutdown","command_id":"c"}"#).unwrap(),
        ];
        let ids: Vec<&str> = cmds.iter().filter_map(|c| c.command_id()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }
}
