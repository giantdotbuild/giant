//! Output renderer - turns the event stream into lines of text.
//!
//! Two visible modes:
//! - `Human` - one line per finished target with colored verb +
//!   right-padded id + dimmed duration; log lines prefixed with
//!   `[target-id]` in a deterministic per-target color.
//! - `Ndjson` - raw event passthrough for porcelains and pipes
//!   (TDD-0004).
//!
//! The live-region / in-place-update design in TDD-0010 is deferred to
//! a future porcelain (`giant-tui`, ADR-0010). v1 is line-streaming
//! only - safe to redirect to a file, no cursor tricks, no frame
//! coalescing.
//!
//! Color is on by default when stdout is a tty and `NO_COLOR` is unset.
//! `--color always|never|auto` overrides; `NO_COLOR=1` always wins
//! against `auto` per the de-facto standard.

use crate::events::{Event, TargetCounts, TargetResultKind};
use crate::model::TargetId;
use anstyle::{AnsiColor, Color, Style};
use std::io::IsTerminal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Human { color: bool },
    Ndjson,
}

impl Mode {
    /// Theme that matches this mode - color for `Human { color: true }`,
    /// plain otherwise. Use this when you need to render outside the
    /// event stream (e.g. an early-exit note).
    pub fn theme(self) -> Theme {
        match self {
            Mode::Human { color: true } => Theme::colored(),
            _ => Theme::plain(),
        }
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    pub fn resolve(self, stdout_is_tty: bool) -> bool {
        match self {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => stdout_is_tty && std::env::var_os("NO_COLOR").is_none(),
        }
    }
}

/// Pick the right mode for a `giant build` / `giant watch` invocation.
/// `ndjson` short-circuits everything else.
pub fn detect_mode(color: ColorChoice, ndjson: bool) -> Mode {
    if ndjson {
        Mode::Ndjson
    } else {
        Mode::Human {
            color: color.resolve(std::io::stdout().is_terminal()),
        }
    }
}

/// Fixed v1 color theme. `enabled = false` → all `paint` calls render
/// the raw text with no escapes.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub enabled: bool,
    built: Style,
    cache: Style,
    remote: Style,
    external: Style,
    skip: Style,
    fail: Style,
    summary_ok: Style,
    summary_fail: Style,
    dim: Style,
    target_palette: [Style; 8],
}

impl Theme {
    pub fn plain() -> Self {
        Self {
            enabled: false,
            built: Style::new(),
            cache: Style::new(),
            remote: Style::new(),
            external: Style::new(),
            skip: Style::new(),
            fail: Style::new(),
            summary_ok: Style::new(),
            summary_fail: Style::new(),
            dim: Style::new(),
            target_palette: [Style::new(); 8],
        }
    }

    pub fn colored() -> Self {
        let fg = |c: AnsiColor| Style::new().fg_color(Some(Color::Ansi(c)));
        // Per-target palette excludes red and green so success/failure
        // colors stay unambiguous against the prefix.
        Self {
            enabled: true,
            built: fg(AnsiColor::Green).bold(),
            cache: fg(AnsiColor::Green),
            remote: fg(AnsiColor::Cyan),
            external: fg(AnsiColor::BrightBlack),
            skip: fg(AnsiColor::BrightBlack),
            fail: fg(AnsiColor::Red).bold(),
            summary_ok: fg(AnsiColor::Green).bold(),
            summary_fail: fg(AnsiColor::Red).bold(),
            dim: fg(AnsiColor::BrightBlack),
            target_palette: [
                fg(AnsiColor::Cyan),
                fg(AnsiColor::Magenta),
                fg(AnsiColor::Blue),
                fg(AnsiColor::Yellow),
                fg(AnsiColor::BrightCyan),
                fg(AnsiColor::BrightMagenta),
                fg(AnsiColor::BrightBlue),
                fg(AnsiColor::BrightYellow),
            ],
        }
    }

    /// Deterministic palette pick for a target id - same id maps to the
    /// same slot across runs so the eye learns it.
    pub fn target_style(&self, id: &TargetId) -> Style {
        let h = fnv1a(id.as_str().as_bytes()) as usize;
        self.target_palette[h % self.target_palette.len()]
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Stateful event-to-line renderer.
///
/// `id_width` is the column width used to right-pad target ids so the
/// duration column lines up. Caller computes it once from the
/// selection before starting the build.
pub struct Renderer {
    mode: Mode,
    theme: Theme,
    id_width: usize,
    quiet: bool,
    failed: Vec<TargetId>,
}

impl Renderer {
    pub fn new(mode: Mode, id_width: usize, quiet: bool) -> Self {
        Self {
            theme: mode.theme(),
            mode,
            id_width,
            quiet,
            failed: Vec::new(),
        }
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Render one event. `None` means "swallow this event" - log lines
    /// in quiet mode, internal events that don't have user-visible
    /// output.
    pub fn render(&mut self, ev: &Event) -> Option<String> {
        match self.mode {
            Mode::Ndjson => Some(serde_json::to_string(ev).ok()? + "\n"),
            Mode::Human { .. } => self.render_human(ev),
        }
    }

    fn render_human(&mut self, ev: &Event) -> Option<String> {
        // Discovery's bootstrap pass runs as its own build under a
        // `bootstrap_*` build id. We swallow its lifecycle and
        // successful-target output so the user sees one summary, not
        // two. Failures still surface - those need to be visible.
        match ev {
            Event::BuildStarted { id, .. } if is_bootstrap(id) => return None,
            Event::BuildFinished { id, .. } if is_bootstrap(id) => return None,
            Event::TargetFinished { build, result, .. }
                if is_bootstrap(build) && !matches!(result, TargetResultKind::Failed) =>
            {
                return None;
            }
            Event::TargetLog { build, .. } if is_bootstrap(build) => return None,
            _ => {}
        }
        match ev {
            Event::BuildStarted { target_ids, .. } => {
                // Lock in the column width now that we know what's
                // actually going to run.
                self.id_width = id_width(target_ids);
                None
            }
            Event::TargetLog { id, line, .. } => {
                if self.quiet {
                    return None;
                }
                Some(self.log_line(id, line))
            }
            Event::TargetFinished {
                id,
                result,
                duration_ms,
                error,
                ..
            } => {
                if matches!(result, TargetResultKind::Failed) {
                    self.failed.push(id.clone());
                }
                if self.quiet && !matches!(result, TargetResultKind::Failed) {
                    return None;
                }
                Some(self.finished_line(id, *result, *duration_ms, error.as_deref()))
            }
            Event::BuildFinished {
                ok,
                duration_ms,
                counts,
                ..
            } => Some(self.summary(*ok, *duration_ms, counts)),
            _ => None,
        }
    }

    fn log_line(&self, id: &TargetId, line: &str) -> String {
        let prefix = paint(
            self.theme.enabled,
            self.theme.target_style(id),
            &format!("[{}]", id.as_str()),
        );
        format!("{prefix} {line}\n")
    }

    fn finished_line(
        &self,
        id: &TargetId,
        result: TargetResultKind,
        ms: u64,
        err: Option<&str>,
    ) -> String {
        let (verb, style) = verb_for(result, &self.theme);
        // Verb padded to VERB_WIDTH visible chars before painting, so
        // ANSI escapes don't shift the column.
        let painted_verb = paint(self.theme.enabled, style, &format!("{verb:<VERB_WIDTH$}"));
        let id_str = id.as_str();
        let id_padded = format!("{id_str:<width$}", width = self.id_width);
        let dur = format_duration(ms);
        let dur_dim = paint(self.theme.enabled, self.theme.dim, &dur);
        match err {
            Some(e) => format!("{painted_verb}  {id_padded}  {dur_dim}  {e}\n"),
            None => format!("{painted_verb}  {id_padded}  {dur_dim}\n"),
        }
    }

    fn summary(&self, ok: bool, ms: u64, counts: &TargetCounts) -> String {
        let head = if ok { "OK  " } else { "FAIL" };
        let head_style = if ok {
            self.theme.summary_ok
        } else {
            self.theme.summary_fail
        };
        let painted_head = paint(self.theme.enabled, head_style, head);
        let dur = format_duration(ms);
        let mut s = String::with_capacity(96);
        s.push('\n');
        s.push_str(&format!(
            "  {painted_head}  {} built · {} cached · {} failed  in {dur}\n",
            counts.built, counts.cache_hit, counts.failed,
        ));
        if !self.failed.is_empty() {
            let names = self
                .failed
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let painted = paint(self.theme.enabled, self.theme.summary_fail, &names);
            s.push_str(&format!("  failed: {painted}\n"));
        }
        s
    }
}

/// Print a one-off informational note (e.g. "no targets affected").
/// Uses a dim `·` marker so it sits visually quieter than verb lines.
fn is_bootstrap(build_id: &str) -> bool {
    build_id.starts_with("bootstrap_")
}

pub fn note(theme: &Theme, msg: &str) -> String {
    let dot = paint(theme.enabled, theme.dim, "·");
    format!("{dot} {msg}\n")
}

/// Width of every status verb, in visible chars. Long enough to fit
/// `"≡ EXTERNAL"` without truncating; all other verbs are padded out.
const VERB_WIDTH: usize = 10;

fn verb_for(r: TargetResultKind, theme: &Theme) -> (&'static str, Style) {
    use TargetResultKind::*;
    match r {
        Built => ("✓ BUILD", theme.built),
        CacheHit => ("✓ CACHE", theme.cache),
        RemoteCacheHit => ("↓ REMOTE", theme.remote),
        ExternalCacheHit => ("≡ EXTERNAL", theme.external),
        Skipped => ("· SKIP", theme.skip),
        Failed => ("✗ FAIL", theme.fail),
    }
}

fn paint(enabled: bool, style: Style, text: &str) -> String {
    if enabled {
        format!("{style}{text}{style:#}")
    } else {
        text.to_string()
    }
}

pub fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{:.1}m", ms as f64 / 60_000.0)
    }
}

/// Computed once from the selection so the duration column lines up
/// across all rendered finish lines.
pub fn id_width<'a, I: IntoIterator<Item = &'a TargetId>>(ids: I) -> usize {
    ids.into_iter()
        .map(|id| id.as_str().len())
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{LogStream, TargetCounts};

    fn ev_finished(id: &str, result: TargetResultKind, ms: u64) -> Event {
        Event::TargetFinished {
            build: "b_test".into(),
            id: TargetId::new(id),
            result,
            duration_ms: ms,
            exit_code: None,
            outputs: vec![],
            error: None,
        }
    }

    fn ev_log(id: &str, line: &str) -> Event {
        Event::TargetLog {
            build: "b_test".into(),
            id: TargetId::new(id),
            stream: LogStream::Stdout,
            line: line.into(),
            truncated: false,
        }
    }

    fn ev_build_finished(ok: bool, ms: u64, built: u32, cached: u32, failed: u32) -> Event {
        Event::BuildFinished {
            id: "b_test".into(),
            ok,
            duration_ms: ms,
            counts: TargetCounts {
                built,
                cache_hit: cached,
                failed,
                skipped: 0,
            },
        }
    }

    #[test]
    fn format_duration_chooses_right_unit() {
        assert_eq!(format_duration(0), "0ms");
        assert_eq!(format_duration(7), "7ms");
        assert_eq!(format_duration(999), "999ms");
        assert_eq!(format_duration(1_000), "1.00s");
        assert_eq!(format_duration(1_240), "1.24s");
        assert_eq!(format_duration(59_999), "60.00s");
        assert_eq!(format_duration(60_000), "1.0m");
        assert_eq!(format_duration(192_000), "3.2m");
    }

    #[test]
    fn target_style_is_deterministic_per_id() {
        let theme = Theme::colored();
        let a1 = theme.target_style(&TargetId::new("go:bin:server"));
        let a2 = theme.target_style(&TargetId::new("go:bin:server"));
        assert_eq!(format!("{a1:?}"), format!("{a2:?}"));
    }

    #[test]
    fn target_style_distributes_across_palette() {
        let theme = Theme::colored();
        // 64 distinct ids should hit at least 4 palette slots - looser
        // than uniform but catches a bug where everything maps to one.
        let ids: Vec<TargetId> = (0..64).map(|i| TargetId::new(format!("t:{i}"))).collect();
        let distinct: std::collections::HashSet<String> = ids
            .iter()
            .map(|id| format!("{:?}", theme.target_style(id)))
            .collect();
        assert!(distinct.len() >= 4, "got {} distinct slots", distinct.len());
    }

    #[test]
    fn color_choice_auto_respects_no_color() {
        // Save/restore env so we don't poison sibling tests in this
        // process (the auto path queries NO_COLOR).
        let prev = std::env::var_os("NO_COLOR");
        // Safety: tests in this module run on one OS thread when
        // serialized via `cargo test -- --test-threads=1`; for the
        // default threaded runner the env is process-global, so we
        // accept some test isolation risk in exchange for not pulling
        // in serial_test.
        unsafe {
            std::env::set_var("NO_COLOR", "1");
        }
        assert!(!ColorChoice::Auto.resolve(true));
        unsafe {
            std::env::remove_var("NO_COLOR");
        }
        assert!(ColorChoice::Auto.resolve(true));
        assert!(!ColorChoice::Auto.resolve(false));
        match prev {
            Some(v) => unsafe { std::env::set_var("NO_COLOR", v) },
            None => unsafe { std::env::remove_var("NO_COLOR") },
        }
    }

    #[test]
    fn color_choice_always_and_never_ignore_tty() {
        assert!(ColorChoice::Always.resolve(false));
        assert!(!ColorChoice::Never.resolve(true));
    }

    #[test]
    fn plain_mode_produces_no_ansi_escapes() {
        let mut r = Renderer::new(Mode::Human { color: false }, 16, false);
        let out = r
            .render(&ev_finished("go:bin:server", TargetResultKind::Built, 1240))
            .unwrap();
        assert!(
            !out.contains('\x1b'),
            "plain mode should not emit ANSI: {out:?}"
        );
        assert!(out.contains("BUILD"));
        assert!(out.contains("go:bin:server"));
        assert!(out.contains("1.24s"));
    }

    #[test]
    fn color_mode_emits_ansi_around_verb() {
        let mut r = Renderer::new(Mode::Human { color: true }, 16, false);
        let out = r
            .render(&ev_finished("go:bin:server", TargetResultKind::Failed, 820))
            .unwrap();
        assert!(out.contains('\x1b'), "color mode should emit ANSI escapes");
        assert!(out.contains("FAIL"));
        assert!(out.contains("820ms"));
    }

    #[test]
    fn log_line_carries_target_prefix() {
        let mut r = Renderer::new(Mode::Human { color: false }, 16, false);
        let out = r
            .render(&ev_log("go:bin:server", "downloading deps"))
            .unwrap();
        assert_eq!(out, "[go:bin:server] downloading deps\n");
    }

    #[test]
    fn ndjson_mode_passes_event_through() {
        let mut r = Renderer::new(Mode::Ndjson, 0, false);
        let out = r
            .render(&ev_finished("a", TargetResultKind::CacheHit, 3))
            .unwrap();
        assert!(out.starts_with('{'));
        assert!(out.contains("\"t\":\"target.finished\""));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn quiet_mode_drops_success_finishes_and_logs() {
        let mut r = Renderer::new(Mode::Human { color: false }, 16, true);
        assert!(r.render(&ev_log("a", "noisy stdout")).is_none());
        assert!(
            r.render(&ev_finished("a", TargetResultKind::Built, 100))
                .is_none()
        );
        assert!(
            r.render(&ev_finished("a", TargetResultKind::CacheHit, 2))
                .is_none()
        );
        let fail = r
            .render(&ev_finished("b", TargetResultKind::Failed, 50))
            .unwrap();
        assert!(fail.contains("FAIL"));
    }

    #[test]
    fn summary_includes_failed_list_when_present() {
        let mut r = Renderer::new(Mode::Human { color: false }, 16, false);
        let _ = r.render(&ev_finished("go:bin:client", TargetResultKind::Failed, 800));
        let _ = r.render(&ev_finished("go:bin:server", TargetResultKind::Failed, 900));
        let summary = r.render(&ev_build_finished(false, 1500, 0, 0, 2)).unwrap();
        assert!(summary.contains("FAIL"));
        assert!(summary.contains("0 built"));
        assert!(summary.contains("2 failed"));
        assert!(summary.contains("1.50s"));
        assert!(summary.contains("go:bin:client"));
        assert!(summary.contains("go:bin:server"));
    }

    #[test]
    fn summary_omits_failed_list_on_success() {
        let mut r = Renderer::new(Mode::Human { color: false }, 16, false);
        let summary = r.render(&ev_build_finished(true, 320, 3, 2, 0)).unwrap();
        assert!(summary.contains("OK"));
        assert!(!summary.contains("failed:"));
    }

    #[test]
    fn id_width_computes_max_len() {
        let ids = vec![
            TargetId::new("a"),
            TargetId::new("go:bin:server"),
            TargetId::new("xx"),
        ];
        assert_eq!(super::id_width(&ids), "go:bin:server".len());
        assert_eq!(super::id_width::<&[TargetId]>(&[]), 0);
    }

    #[test]
    fn note_uses_dim_marker() {
        let theme = Theme::plain();
        assert_eq!(
            note(&theme, "no targets affected"),
            "· no targets affected\n"
        );
    }
}
