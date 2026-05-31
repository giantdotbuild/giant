//! Controlling-terminal foreground handoff for task commands.
//!
//! A command spawned in its own process group (so a shutdown signal can
//! reach its whole subtree - see [`crate::signals`]) is, by default, a
//! *background* group: if it reads the terminal - `sudo` or `ssh` reading
//! `/dev/tty` for a password, a confirmation prompt, a REPL - the kernel
//! stops it with SIGTTIN. We avoid that the way a shell does for a
//! foreground job: give the command's group the terminal for the duration
//! of its run, then take it back.
//!
//! No-op when there's no controlling terminal (CI, pipes) or when we don't
//! currently own the terminal (giant-task was backgrounded with `&`) - so
//! we never steal the terminal out from under the user's shell.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;

/// RAII guard: makes a child's process group the terminal's foreground on
/// [`Foreground::grab`], and restores ours on drop - however the command
/// ends (exit, timeout, signal).
pub struct Foreground {
    tty: File,
    restore_to: libc::pid_t,
}

impl Foreground {
    /// Make `child_pgid` the terminal's foreground group. Returns `None`
    /// when the handoff doesn't apply: no controlling terminal, or we
    /// aren't the foreground group right now.
    pub fn grab(child_pgid: libc::pid_t) -> Option<Self> {
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        let fd = tty.as_raw_fd();

        // Only take the terminal if we currently own it; otherwise we'd be
        // yanking it from whatever's actually in the foreground.
        let ours = unsafe { libc::getpgrp() };
        if unsafe { libc::tcgetpgrp(fd) } != ours {
            return None;
        }

        // Once the child is foreground we're a background group: the
        // reclaim `tcsetpgrp` on drop, and any log line we print
        // meanwhile, would raise SIGTTOU and stop us. Ignore it, exactly
        // as a job-control shell does for itself.
        unsafe { libc::signal(libc::SIGTTOU, libc::SIG_IGN) };
        unsafe { libc::tcsetpgrp(fd, child_pgid) };
        Some(Self {
            tty,
            restore_to: ours,
        })
    }
}

impl Drop for Foreground {
    fn drop(&mut self) {
        unsafe { libc::tcsetpgrp(self.tty.as_raw_fd(), self.restore_to) };
    }
}
