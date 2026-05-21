//! Output rendering for giant-task.
//!
//! Style-matches `giant build` - dim `·` notes for one-off lines,
//! colored verbs reserved for the running phase. Auto-detects tty.

use crate::config::TaskConfig;
use anstyle::{AnsiColor, Color, Style};
use giant::events::TargetCounts;
use std::io::IsTerminal;

fn enabled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn paint(s: Style, text: &str) -> String {
    if enabled() {
        format!("{s}{text}{s:#}")
    } else {
        text.to_string()
    }
}

fn dim() -> Style {
    Style::new().fg_color(Some(Color::Ansi(AnsiColor::BrightBlack)))
}

fn accent() -> Style {
    Style::new()
        .fg_color(Some(Color::Ansi(AnsiColor::Green)))
        .bold()
}

fn fail() -> Style {
    Style::new()
        .fg_color(Some(Color::Ansi(AnsiColor::Red)))
        .bold()
}

fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{:.1}m", ms as f64 / 60_000.0)
    }
}

/// One-off informational line (mirrors `renderer::note` in core).
pub fn note(msg: &str) {
    let dot = paint(dim(), "·");
    println!("{dot} {msg}");
}

/// Two-line "running task" header. Sits above the task command's
/// inherited stdout/stderr.
pub fn running(name: &str) {
    let arrow = paint(accent(), "▶");
    let name_s = paint(accent(), name);
    println!("{arrow} {name_s}");
}

/// One-line "deps built OK" summary. Mirrors the shape of `giant
/// build`'s summary but collapsed onto a single line - the user
/// doesn't need to see every cache hit when they're really running a
/// task, just whether it worked.
pub fn deps_ok(counts: &TargetCounts, duration_ms: u64) {
    let check = paint(accent(), "✓");
    let dur = paint(dim(), &format_duration(duration_ms));
    let line = format!(
        "{} {} built · {} cached  in {}",
        check, counts.built, counts.cache_hit, dur
    );
    println!("  {line}");
}

/// One-line "deps failed" summary. Concrete counts so the user can
/// tell at a glance how big the failure was.
pub fn deps_fail(counts: &TargetCounts, duration_ms: u64) {
    let x = paint(fail(), "✗");
    let count_s = paint(fail(), &format!("{} failed", counts.failed));
    let dur = paint(dim(), &format_duration(duration_ms));
    let line = format!(
        "{} {} · {} built · {} cached  in {}",
        x, count_s, counts.built, counts.cache_hit, dur
    );
    println!("  {line}");
}

/// Header for the per-failure replay block. Mirrors a one-line
/// '--- last N lines of stderr for <id> ---' editor banner.
pub fn failure_header(id: &str) {
    let arrow = paint(fail(), "✗");
    let id_s = paint(fail(), id);
    println!("\n{arrow} {id_s}");
}

/// Print the available tasks with descriptions.
pub fn list(cfg: &TaskConfig) {
    if cfg.tasks.is_empty() {
        note("no tasks defined in giant.yaml");
        return;
    }
    let header = paint(accent(), "tasks");
    let workspace = paint(dim(), &format!("({})", cfg.workspace_name));
    println!("{header} {workspace}");

    let width = cfg.tasks.keys().map(|k| k.len()).max().unwrap_or(0);

    for (name, spec) in &cfg.tasks {
        let name_s = paint(accent(), name);
        let desc = spec.description.as_deref().unwrap_or("");
        println!(
            "  {name_s:<padded$}  {desc}",
            padded = width + accent_padding()
        );
    }
}

/// When we paint the task name in `accent` (green+bold), the ANSI
/// escape sequence adds invisible bytes that break `{:<width}` padding.
/// This is the extra byte count we have to add to the width spec so
/// columns line up under colour. With colour off it's 0.
fn accent_padding() -> usize {
    if enabled() {
        // `\x1b[1;32m` (7 bytes) + `\x1b[0m` (4 bytes) = 11.
        11
    } else {
        0
    }
}
