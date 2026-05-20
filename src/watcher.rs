//! File watcher.
//!
//! See TDD-0008 for the watch-mode design. This module owns the notify
//! integration and the debouncer.

use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum WatcherError {
    #[error("notify: {0}")]
    Notify(#[from] notify::Error),

    #[error("workspace root not absolute: {0:?}")]
    BadRoot(std::path::PathBuf),
}

#[derive(Debug, Clone)]
pub struct WatchConfig {
    pub workspace_root: PathBuf,
    pub quiet_window: Duration,
    pub max_delay: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            workspace_root: PathBuf::from("."),
            quiet_window: Duration::from_millis(100),
            max_delay: Duration::from_millis(500),
        }
    }
}

/// One coalesced batch of file events.
#[derive(Debug, Clone, Default)]
pub struct WatchBatch {
    pub paths: Vec<PathBuf>,
}

/// Spawn a watcher. Returns a receiver that yields debounced batches.
pub async fn spawn(
    _cfg: WatchConfig,
) -> Result<mpsc::Receiver<WatchBatch>, WatcherError> {
    todo!("TDD-0008: notify -> debouncer -> mpsc<WatchBatch>")
}
