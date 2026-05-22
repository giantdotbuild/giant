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

    /// Re-read the workspace config + re-run discovery, emit a fresh
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
            | Command::AffectedUnsubscribe { command_id, .. } => command_id.as_deref(),
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
