//! Dep-phase orchestration.
//!
//! Spawns `giant build --events ndjson <deps...>` and consumes the
//! NDJSON event stream. The default UI is compact - one summary line
//! per phase, full output suppressed unless something fails or
//! `--verbose` is set. On failure, the failing target's captured
//! stderr is replayed so the user can see what went wrong without
//! scrolling through a flood of cache-hit lines from successful
//! siblings.
//!
//! Falls back to the inherited-stdio behaviour automatically if
//! `giant` is missing from PATH (porcelain shouldn't crash because of
//! that - it reports cleanly).

use crate::render;
use giant::events::{Event, LogStream, TargetCounts, TargetResultKind};
use giant::model::TargetId;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

const GIANT_BIN_ENV: &str = "GIANT_TASK_BUILD_BIN";

#[derive(Debug, thiserror::Error)]
pub enum DepsError {
    #[error("could not spawn `giant build`: {0}")]
    Spawn(#[from] std::io::Error),
}

/// Run `giant build <deps...>` and surface a compact summary. Returns
/// the subprocess exit code on success-or-clean-failure, errors only
/// if we couldn't spawn at all.
pub async fn build(
    deps: &[String],
    workspace_root: &Path,
    verbose: bool,
) -> Result<i32, DepsError> {
    let bin = std::env::var_os(GIANT_BIN_ENV).unwrap_or_else(|| OsString::from("giant"));

    render::note(&format!("building {} dep(s)", deps.len()));

    let mut child = Command::new(&bin)
        .args(["build", "--events", "ndjson", "--color", "never"])
        .args(deps)
        .current_dir(workspace_root)
        // Capture stdout for NDJSON; stderr stays inherited so the
        // user still sees tracing warnings etc. that come from the
        // engine outside the event stream.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .expect("stdout was just piped - it's there");
    let mut lines = BufReader::new(stdout).lines();

    let mut state = State::default();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let event: Event = match serde_json::from_str(&line) {
            Ok(ev) => ev,
            Err(_) => {
                // Non-JSON line (shouldn't happen, but tolerate it
                // instead of bricking the porcelain). Echo through if
                // verbose so the user can see what slipped past us.
                if verbose {
                    println!("{line}");
                }
                continue;
            }
        };
        state.consume(event, verbose);
    }

    let status = child.wait().await?;
    let code = status.code().unwrap_or(1);

    state.flush_failures();
    state.flush_summary(code);

    Ok(code)
}

/// Accumulator for one build run. Per-target stderr buffers are kept
/// only for targets that haven't finished yet; on success we drop
/// them, on failure we keep them for replay.
#[derive(Default)]
struct State {
    /// Counts from the BuildFinished event, if it arrived.
    counts: Option<TargetCounts>,
    /// Duration from the BuildFinished event, if it arrived.
    duration_ms: Option<u64>,
    /// Per-target captured stderr+stdout. Indexed by id. Dropped on
    /// successful TargetFinished; kept on failure.
    captured: HashMap<TargetId, Vec<String>>,
    /// IDs that finished as Failed, in completion order.
    failures: Vec<TargetId>,
    /// True if we've already printed a "building …" line for a target
    /// (so verbose mode can prefix log output properly).
    bootstrap: bool,
}

impl State {
    fn consume(&mut self, ev: Event, verbose: bool) {
        // Bootstrap (discovery) events are part of `giant build`'s
        // pipeline. The renderer in core filters them; we do the same
        // here so the user sees one summary per phase, not two.
        if event_is_bootstrap(&ev) {
            return;
        }

        match ev {
            Event::TargetLog {
                id, stream, line, ..
            } => {
                let entry = self.captured.entry(id.clone()).or_default();
                let tagged = match stream {
                    LogStream::Stdout => line.clone(),
                    LogStream::Stderr => line.clone(),
                };
                entry.push(tagged);
                if verbose {
                    println!("[{id}] {line}");
                }
            }
            Event::TargetFinished { id, result, .. } => {
                match result {
                    TargetResultKind::Failed => {
                        // Keep the buffer for failure replay.
                        self.failures.push(id);
                    }
                    _ => {
                        // Drop the buffer; we don't need it.
                        self.captured.remove(&id);
                    }
                }
            }
            Event::BuildFinished {
                duration_ms,
                counts,
                ..
            } => {
                self.counts = Some(counts);
                self.duration_ms = Some(duration_ms);
                self.bootstrap = true;
            }
            _ => {}
        }
    }

    fn flush_failures(&mut self) {
        if self.failures.is_empty() {
            return;
        }
        // For each failing target, replay the captured output so the
        // user sees what went wrong. Cap at ~50 lines per target so a
        // chatty failure doesn't flood the terminal.
        const MAX_LINES_PER_TARGET: usize = 50;
        for id in &self.failures {
            render::failure_header(id.as_str());
            if let Some(lines) = self.captured.get(id) {
                let total = lines.len();
                let start = total.saturating_sub(MAX_LINES_PER_TARGET);
                if start > 0 {
                    render::note(&format!("… ({} earlier lines elided)", start));
                }
                for line in &lines[start..] {
                    println!("  {line}");
                }
            } else {
                render::note("(no output captured)");
            }
        }
    }

    fn flush_summary(&mut self, exit_code: i32) {
        // If BuildFinished never arrived (e.g. giant crashed mid-run),
        // fall back to a generic line.
        let Some(counts) = &self.counts else {
            if exit_code != 0 {
                render::note(&format!("giant build exited {exit_code}"));
            }
            return;
        };
        let duration = self.duration_ms.unwrap_or(0);
        if exit_code == 0 {
            render::deps_ok(counts, duration);
        } else {
            render::deps_fail(counts, duration);
        }
    }
}

/// Bootstrap events come from discovery - they have a build id that
/// starts with `bootstrap_` (TDD-0003). Core's renderer filters them
/// the same way.
fn event_is_bootstrap(ev: &Event) -> bool {
    match ev {
        Event::BuildStarted { id, .. } | Event::BuildFinished { id, .. } => {
            id.starts_with("bootstrap_")
        }
        Event::TargetFinished { build, .. } | Event::TargetLog { build, .. } => {
            build.starts_with("bootstrap_")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use giant::events::TargetCounts;
    use giant::model::TargetId;

    #[test]
    fn drops_buffer_for_successful_target() {
        let mut s = State::default();
        s.consume(
            Event::TargetLog {
                build: "b_1".into(),
                id: TargetId::new("foo"),
                stream: LogStream::Stdout,
                line: "compiling".into(),
                truncated: false,
            },
            false,
        );
        assert!(s.captured.contains_key(&TargetId::new("foo")));
        s.consume(
            Event::TargetFinished {
                build: "b_1".into(),
                id: TargetId::new("foo"),
                result: TargetResultKind::Built,
                duration_ms: 12,
                exit_code: Some(0),
                outputs: vec![],
                error: None,
            },
            false,
        );
        // Successful → buffer discarded.
        assert!(!s.captured.contains_key(&TargetId::new("foo")));
        assert!(s.failures.is_empty());
    }

    #[test]
    fn keeps_buffer_for_failed_target() {
        let mut s = State::default();
        for line in ["go: downloading", "internal/foo.go:3: error: oops"] {
            s.consume(
                Event::TargetLog {
                    build: "b_1".into(),
                    id: TargetId::new("foo"),
                    stream: LogStream::Stderr,
                    line: line.into(),
                    truncated: false,
                },
                false,
            );
        }
        s.consume(
            Event::TargetFinished {
                build: "b_1".into(),
                id: TargetId::new("foo"),
                result: TargetResultKind::Failed,
                duration_ms: 12,
                exit_code: Some(1),
                outputs: vec![],
                error: Some("exit code 1".into()),
            },
            false,
        );
        assert_eq!(s.failures, vec![TargetId::new("foo")]);
        // Buffer retained for replay.
        assert_eq!(s.captured.get(&TargetId::new("foo")).unwrap().len(), 2);
    }

    #[test]
    fn bootstrap_events_are_filtered() {
        let mut s = State::default();
        s.consume(
            Event::TargetLog {
                build: "bootstrap_abc".into(),
                id: TargetId::new("discover:go"),
                stream: LogStream::Stdout,
                line: "noise".into(),
                truncated: false,
            },
            false,
        );
        s.consume(
            Event::TargetFinished {
                build: "bootstrap_abc".into(),
                id: TargetId::new("discover:go"),
                result: TargetResultKind::Failed,
                duration_ms: 0,
                exit_code: None,
                outputs: vec![],
                error: None,
            },
            false,
        );
        // Discovery events should be invisible to this layer.
        assert!(s.captured.is_empty());
        assert!(s.failures.is_empty());
    }

    #[test]
    fn build_finished_populates_counts() {
        let mut s = State::default();
        s.consume(
            Event::BuildFinished {
                id: "b_1".into(),
                ok: true,
                duration_ms: 320,
                counts: TargetCounts {
                    built: 2,
                    cache_hit: 5,
                    failed: 0,
                    skipped: 0,
                },
            },
            false,
        );
        assert!(s.counts.is_some());
        assert_eq!(s.duration_ms, Some(320));
    }
}
