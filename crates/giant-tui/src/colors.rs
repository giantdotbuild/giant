//! Per-target color hashing (FNV-1a over the target id) and a small
//! status palette. Same scheme the core renderer uses so the
//! `[name]` log prefix is the same color across `giant build` and
//! `giant tui`.

use crate::state::TargetStatus;
use ratatui::style::{Color, Modifier, Style};

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

const TARGET_PALETTE: &[Color] = &[
    Color::Cyan,
    Color::Magenta,
    Color::Blue,
    Color::Yellow,
    Color::LightCyan,
    Color::LightMagenta,
    Color::LightBlue,
    Color::LightYellow,
];

pub fn target_color(id: &str) -> Color {
    let mut h = FNV_OFFSET;
    for b in id.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    TARGET_PALETTE[(h as usize) % TARGET_PALETTE.len()]
}

pub fn status_icon(status: TargetStatus) -> &'static str {
    match status {
        TargetStatus::Queued => "·",
        TargetStatus::Running => "⏵",
        TargetStatus::Built | TargetStatus::Cached => "✓",
        TargetStatus::Remote => "↓",
        TargetStatus::External => "≡",
        TargetStatus::Skipped => "·",
        TargetStatus::Failed => "✗",
    }
}

pub fn status_label(status: TargetStatus) -> &'static str {
    match status {
        TargetStatus::Queued => "queued",
        TargetStatus::Running => "running",
        TargetStatus::Built => "built",
        TargetStatus::Cached => "cache",
        TargetStatus::Remote => "remote",
        TargetStatus::External => "external",
        TargetStatus::Skipped => "skip",
        TargetStatus::Failed => "FAIL",
    }
}

pub fn status_style(status: TargetStatus) -> Style {
    let base = Style::default();
    match status {
        TargetStatus::Queued | TargetStatus::Skipped => base.fg(Color::DarkGray),
        TargetStatus::Running => base.fg(Color::Yellow).add_modifier(Modifier::BOLD),
        TargetStatus::Built => base.fg(Color::Green).add_modifier(Modifier::BOLD),
        TargetStatus::Cached => base.fg(Color::Green),
        TargetStatus::Remote => base.fg(Color::Cyan),
        TargetStatus::External => base.fg(Color::DarkGray),
        TargetStatus::Failed => base.fg(Color::Red).add_modifier(Modifier::BOLD),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_id_always_hashes_to_same_color() {
        assert_eq!(target_color("go:bin:server"), target_color("go:bin:server"));
    }

    #[test]
    fn different_ids_likely_get_different_colors() {
        assert_ne!(target_color("a"), target_color("z"));
    }
}
