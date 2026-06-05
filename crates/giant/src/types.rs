//! Small shared types.
//!
//! `GlobPattern` lives in the `giant-schema` crate (it is part of the wire
//! `Input` form) and is re-exported here so existing `crate::types::GlobPattern`
//! paths keep resolving.

pub use giant_schema::GlobPattern;
