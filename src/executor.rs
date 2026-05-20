//! Parallel executor.
//!
//! See TDD-0009 for the three-phase model (key compute → cache lookup →
//! dispatch) and ADR-0009 for the async discipline.

use crate::cache::LocalCache;
use crate::events::{Event, EventSender, TargetCounts};
use crate::graph::BuildGraph;
use crate::model::{CacheKey, TargetId};
use crate::paths::AbsPath;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),

    #[error("cache: {0}")]
    Cache(#[from] crate::cache::CacheError),

    #[error("graph: {0}")]
    Graph(#[from] crate::graph::GraphError),

    #[error("cancelled")]
    Cancelled,
}

#[derive(Debug)]
pub struct BuildJob {
    pub graph: Arc<BuildGraph>,
    pub selection: Vec<TargetId>,
    pub cache: LocalCache,
    pub workspace_root: AbsPath,
    pub output_dir: AbsPath,
    pub parallelism: usize,
    pub fresh: bool,
    pub strict: bool,
    pub show_cached_logs: bool,
    pub events: EventSender,
    pub cancel: CancellationToken,
}

#[derive(Debug, Clone)]
pub struct BuildSummary {
    pub counts: TargetCounts,
    pub duration: Duration,
    pub failed_targets: Vec<TargetId>,
    pub cache_keys: HashMap<TargetId, CacheKey>,
}

/// Run a build. Implementation deferred (TDD-0009).
pub async fn build(_job: BuildJob) -> Result<BuildSummary, ExecutorError> {
    todo!("implement executor per TDD-0009")
}

/// What happened for one target.
#[derive(Debug, Clone)]
pub enum TargetResult {
    Built {
        key: CacheKey,
        duration: Duration,
    },
    CacheHit {
        key: CacheKey,
    },
    RemoteCacheHit {
        key: CacheKey,
    },
    ExternalCacheHit {
        key: CacheKey,
    },
    Failed {
        key: Option<CacheKey>,
        error: String,
    },
    Skipped {
        reason: String,
    },
}

impl TargetResult {
    pub fn is_success(&self) -> bool {
        !matches!(self, TargetResult::Failed { .. } | TargetResult::Skipped { .. })
    }
}

#[allow(dead_code)]
fn _emit_marker(_e: &Event) {}
