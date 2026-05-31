//! Graceful-shutdown signal handling for giant-task.
//!
//! In run mode the engine used to rely on the terminal delivering SIGINT
//! to the whole foreground process group: the command died, and the
//! lifecycle fell through to `finally` + service teardown. A signal sent
//! straight to giant-task (`pkill -INT`, `systemctl stop`, a parent
//! supervisor) bypassed that - with no handler installed the process took
//! the default disposition and died before any cleanup ran.
//!
//! `Shutdown` closes that gap. Constructing it installs SIGINT/SIGTERM
//! handlers; the first signal is latched and any later ones are swallowed
//! so cleanup completes instead of being cut short. The command runner
//! forwards the signal to the command's own process group, and both run
//! and supervise mode always reach the teardown path.

use std::time::Duration;
use tokio::process::Child;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

/// A latching SIGINT/SIGTERM listener. Cheap to share: `recv` can be
/// awaited from several places, and every caller observes the same first
/// signal.
pub struct Shutdown {
    rx: watch::Receiver<Option<i32>>,
}

impl Shutdown {
    /// Install the handlers and start latching. The listener task lives
    /// for the rest of the process, swallowing further signals so a hung
    /// shutdown can't be made worse by an impatient second Ctrl-C cutting
    /// teardown short.
    pub fn install() -> std::io::Result<Self> {
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        let (tx, rx) = watch::channel(None);
        tokio::spawn(async move {
            loop {
                let sig = tokio::select! {
                    _ = sigint.recv() => libc::SIGINT,
                    _ = sigterm.recv() => libc::SIGTERM,
                };
                // Latch the first signal; keep looping to absorb the rest.
                tx.send_if_modified(|cur| {
                    if cur.is_none() {
                        *cur = Some(sig);
                        true
                    } else {
                        false
                    }
                });
            }
        });
        Ok(Self { rx })
    }

    /// Resolve once a shutdown signal has arrived, yielding its number. If
    /// one already fired, returns immediately.
    pub async fn recv(&self) -> i32 {
        match self.rx.clone().wait_for(Option::is_some).await {
            Ok(latched) => latched.expect("waited for Some"),
            // The sender outlives the process, so the channel never closes;
            // if it somehow did, never fire rather than report a false signal.
            Err(_) => std::future::pending().await,
        }
    }
}

/// Terminate a running child: send `signum`, wait briefly, then SIGKILL if
/// it's still alive. Reaps the child either way. With `as_group`, signals
/// the child's whole process group (it must have been spawned with
/// `process_group(0)`); otherwise just the child pid.
pub async fn terminate(child: &mut Child, signum: i32, as_group: bool) {
    const GRACE: Duration = Duration::from_secs(3);
    let Some(pid) = child.id() else {
        // Already reaped - nothing to signal.
        return;
    };
    let target = if as_group { -(pid as i32) } else { pid as i32 };
    // SAFETY: `kill` is async-signal-safe and takes plain integers.
    unsafe { libc::kill(target, signum) };
    if tokio::time::timeout(GRACE, child.wait()).await.is_err() {
        unsafe { libc::kill(target, libc::SIGKILL) };
        let _ = child.wait().await;
    }
}
