//! Giant's wire protocol: the `Command` / `Event` NDJSON types the engine speaks
//! over `giant session` (ADR-0003), the `TargetId` vocabulary they share, and a
//! reference [`client`] that drives a session subprocess.
//!
//! This crate carries no engine logic - it is the contract a porcelain links to
//! render engine-computed data without compiling the engine itself (ADR-0034).

pub mod client;
pub mod commands;
pub mod events;

pub use client::query_session;
pub use commands::Command;
pub use events::{Event, EventSender};

use serde::{Deserialize, Serialize};

/// Path-derived target label `//<package>:<name>` (TDD-0001, ADR-0024).
///
/// The package is the workspace-relative directory of the target's
/// `giant.yaml`; the root package is empty, so a root target is
/// `//:name`. The engine treats the whole string as opaque past
/// construction.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TargetId(String);

impl TargetId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Build the label for `name` in `package` (a workspace-relative dir,
    /// `""` for the root package): `//<package>:<name>`.
    pub fn label(package: &str, name: &str) -> Self {
        Self(format!("//{package}:{name}"))
    }

    /// Split a `//<package>:<name>` label into its package (may be empty
    /// or contain `/`) and name parts. The inverse of `label`.
    pub fn split(&self) -> (&str, &str) {
        let body = self.0.strip_prefix("//").unwrap_or(&self.0);
        body.rsplit_once(':').unwrap_or((body, ""))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TargetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for TargetId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for TargetId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TargetId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for TargetId {
    fn from(s: String) -> Self {
        Self(s)
    }
}
