//! Task schema. Owned by this porcelain, deliberately separate from
//! core's `giant::Config`. Same `giant.yaml` file, different reader.
//!
//! Only the `tasks:` block (and the workspace name, for sanity) is
//! consulted; every other field core defines is silently ignored.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level shape we read from giant.yaml. Other fields (targets,
/// include, cache, etc.) belong to core; we don't look at them.
#[derive(Debug, Deserialize)]
pub struct TopLevel {
    #[serde(default)]
    pub workspace: WorkspaceStub,
    #[serde(default)]
    pub tasks: IndexMap<String, TaskSpec>,
    #[serde(default)]
    pub services: IndexMap<String, ServiceSpec>,
}

#[derive(Debug, Default, Deserialize)]
pub struct WorkspaceStub {
    #[serde(default)]
    pub name: String,
}

/// One task definition. The full lifecycle:
///
/// 1. Build `deps` via `giant build` (target ids).
/// 2. Start `services` in parallel; wait for each `ready` probe.
/// 3. Run `needs` (other tasks) in declared order.
/// 4. Run this task's `command`.
/// 5. Always run `finally` (other tasks) in declared order, even on
///    failure or signal.
/// 6. Stop `services` in parallel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    /// Shell command (`sh -c "<command>"`). Required.
    pub command: String,

    /// One-line description for `giant-task list`.
    #[serde(default)]
    pub description: Option<String>,

    /// Target IDs to build before this task's command runs. Each is
    /// forwarded to `giant build <id>` as a subprocess.
    #[serde(default)]
    pub deps: Vec<String>,

    /// Other task names to run-to-completion before this task's
    /// command. Sequential, in declared order.
    #[serde(default)]
    pub needs: Vec<String>,

    /// Service names to start (parallel) before this task's command,
    /// and stop (parallel) after it exits. The task waits for each
    /// service's `ready` probe to pass before proceeding.
    #[serde(default)]
    pub services: Vec<String>,

    /// Other task names to run after this task's `command` (success
    /// or failure or signal). Sequential, in declared order. Useful
    /// for cleanup steps.
    #[serde(default)]
    pub finally: Vec<String>,

    /// Named arguments. The CLI binds them via `--arg key=value` and
    /// they're exported as `GIANT_ARG_<KEY>=<value>` env vars before
    /// running the command.
    #[serde(default)]
    pub args: IndexMap<String, TaskArg>,

    /// Extra environment variables. Merged on top of inherited env.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Working directory, relative to the workspace root. Default:
    /// workspace root.
    #[serde(default)]
    pub cwd: Option<String>,

    /// Seconds before the command is killed. None = no timeout.
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// Optional input globs the task is sensitive to. Only consulted by
    /// `giant-task --watch <name>`: file events under these paths
    /// retrigger the task. Same glob syntax as core target inputs.
    /// Empty = `giant-task --watch` falls back to watching the
    /// workspace root, with the cache + state dirs excluded.
    #[serde(default)]
    pub inputs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskArg {
    /// Default value when the user doesn't pass `--arg <name>=...`.
    #[serde(default)]
    pub default: Option<String>,
    /// Constrained value set. If non-empty, `default` must be in the
    /// list and any user-supplied value must match.
    #[serde(default)]
    pub choices: Option<Vec<String>>,
    /// One-line description for `giant-task <name> --help`.
    #[serde(default)]
    pub description: Option<String>,
}

/// One long-lived process started for the duration of a task. Started
/// in parallel with sibling services, then waited on via the `ready`
/// probe before the task command runs. Stopped on task exit (signal
/// escalation: SIGINT then SIGTERM then SIGKILL).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpec {
    /// Shell command (`sh -c "<command>"`). Required.
    pub command: String,

    /// One-line description for diagnostics.
    #[serde(default)]
    pub description: Option<String>,

    /// Target IDs to build before this service starts. Same shape as
    /// task `deps:`. Forwarded to `giant build`.
    #[serde(default)]
    pub deps: Vec<String>,

    /// Optional readiness probe. If absent, the service is considered
    /// ready as soon as it starts (i.e., the task command runs
    /// immediately). For real network services, prefer a probe.
    #[serde(default)]
    pub ready: Option<ReadyProbe>,

    /// Extra environment variables.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Working directory, workspace-relative. Default: workspace root.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// How to tell whether a service is ready. v1 supports only `command`
/// - a shell snippet run periodically until it exits 0, or
/// `timeout_secs` elapses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadyProbe {
    /// Shell command. Exit 0 = ready.
    pub command: String,
    /// Poll interval. Default: 1 second.
    #[serde(default = "default_period")]
    pub period_secs: u64,
    /// Hard ceiling on the wait. Default: 30 seconds.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_period() -> u64 {
    1
}

fn default_timeout() -> u64 {
    30
}
