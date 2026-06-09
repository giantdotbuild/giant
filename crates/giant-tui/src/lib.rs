//! `giant-tui` - interactive target browser + build runner.
//!
//! Spawns one `giant session` subprocess for the TUI's lifetime
//! All catalog data, build progress, and (eventually)
//! watch cycles flow over the same NDJSON channel.
//!
//! `lib.rs` exposes the testable pieces - state machine, key handler,
//! layout - so the bin's tokio loop stays small.

pub mod colors;
pub mod keys;
pub mod state;
pub mod ui;
