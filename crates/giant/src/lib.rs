//! Giant - build orchestration with content-addressed caching.
//!
//! The engine is language-agnostic: targets are `inputs → command → outputs`.
//! Config is static `giant.yaml`/`giant.json`; producing it (discovery,
//! matrices) is an offline generator's job, not the engine's.
//!
//! See `docs/ARCHITECTURE.md` for the design.

pub mod cache;
pub mod cli;
pub mod config;
pub mod executor;
pub mod explain;
pub mod fmt;
pub mod git;
pub mod graph;
pub mod model;
pub mod paths;
#[cfg(feature = "remote")]
pub mod remote;
pub mod selection;
pub mod types;
pub mod watcher;
pub mod worktree;

// The wire protocol lives in giant-protocol now; re-export its modules
// so `giant::commands` / `giant::events` / `giant::client` stay stable for
// engine-linking porcelains and internal `crate::` paths keep resolving.
pub use giant_protocol::{client, commands, events};

// Re-export the most-used types at the crate root.
pub use cache::LocalCache;
// The session-query client read-query porcelains use to render engine-computed
// data over the protocol instead of recomputing it.
pub use giant_protocol::query_session;
// The workspace-load entry point porcelains link against: scan
// config, build the graph, open the cache.
pub use cli::prep::{
    Prepared, last_failures_path, num_cpus_estimate, prepare, read_last_failures, resolve_cache_dir,
};
// The in-process build adapter the `giant-build` porcelain drives:
// one build / a watch loop, plus sandbox-policy resolution.
pub use cli::resolve_sandbox;
pub use cli::session::{BuildOptions, run_one_build, run_watch_command};
pub use config::Config;
pub use executor::{BuildJob, BuildSummary, SandboxPolicy, build};
pub use fmt::format_duration;
pub use giant_protocol::{Event, EventSender, PROTOCOL_VERSION};
pub use graph::BuildGraph;
pub use model::{CacheKey, ContentHash, TargetId, TargetSpec};
pub use paths::{AbsPath, OutputPath, WsRelPath};
