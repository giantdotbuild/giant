//! Giant - build orchestration with content-addressed caching.
//!
//! The engine is language-agnostic: targets are `inputs → command → outputs`.
//! Config is static `giant.yaml`/`giant.json`; producing it (discovery,
//! matrices) is an offline generator's job, not the engine's (ADR-0024).
//!
//! See `docs/adr/` and `docs/tdd/` for the design.

pub mod cache;
pub mod cli;
pub mod client;
pub mod commands;
pub mod config;
pub mod events;
pub mod executor;
pub mod explain;
pub mod git;
pub mod graph;
pub mod model;
pub mod paths;
#[cfg(feature = "remote")]
pub mod remote;
pub mod renderer;
pub mod selection;
pub mod types;
pub mod watcher;

// Re-export the most-used types at the crate root.
pub use cache::LocalCache;
// The session-query client read-query porcelains use to render engine-computed
// data over the protocol (ADR-0034) instead of recomputing it.
pub use client::query_session;
// The workspace-load entry point porcelains link against (ADR-0034): scan
// config, build the graph, open the cache.
pub use cli::prep::{Prepared, prepare, resolve_cache_dir};
pub use config::Config;
pub use events::{Event, EventSender};
pub use executor::{BuildJob, BuildSummary, build};
pub use graph::BuildGraph;
pub use model::{CacheKey, ContentHash, TargetId, TargetSpec};
pub use paths::{AbsPath, OutputPath, WsRelPath};
