//! ratatui draw functions. Two layouts: browser (catalog list) and
//! build view (target list + recent logs).

use crate::colors::{status_icon, status_label, status_style, target_color};
use crate::state::{
    CatalogEntry, LogLine, Mode, Screen, State, StatusFilter, TargetStatus, TargetView,
};
use giant::events::LogStream;
use giant::model::TargetId;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

pub const HEADER_HEIGHT: u16 = 3;
pub const FOOTER_HEIGHT: u16 = 8;

pub fn draw(frame: &mut Frame, state: &State) {
    let area = frame.area();
    if area.height < 10 || area.width < 60 {
        draw_collapsed(frame, area, state);
        return;
    }
    match state.screen {
        Screen::Loading => draw_loading(frame, area, state),
        Screen::Browser => draw_browser(frame, area, state),
        Screen::Building | Screen::BuildFinished => draw_build_view(frame, area, state),
    }

    if state.mode == Mode::Search {
        draw_search_bar(frame, area, state);
    }
    if state.mode == Mode::Help {
        draw_help_overlay(frame, area);
    }
    if let Some(err) = &state.last_error {
        draw_error_banner(frame, area, err);
    }
    if state.quitting {
        draw_quitting_overlay(frame, area);
    }
}

fn draw_quitting_overlay(frame: &mut Frame, area: Rect) {
    let w: u16 = 30.min(area.width.saturating_sub(2));
    let h: u16 = 3.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    let para = Paragraph::new(Line::from(Span::styled(
        " quitting…",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(para, rect);
}

// ============================================================
// Loading
// ============================================================

fn draw_loading(frame: &mut Frame, area: Rect, state: &State) {
    let msg = if let Some(err) = &state.catalog_error {
        format!("catalog load failed: {err}")
    } else if state.catalog.is_empty() {
        "starting engine session…".into()
    } else {
        format!("loading catalog… ({} targets so far)", state.catalog.len())
    };
    let para = Paragraph::new(Line::from(Span::styled(
        msg,
        Style::default().add_modifier(Modifier::BOLD),
    )))
    .block(Block::default().borders(Borders::ALL).title(" giant tui "));
    frame.render_widget(para, area);
}

// ============================================================
// Browser
// ============================================================

fn draw_browser(frame: &mut Frame, area: Rect, state: &State) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(HEADER_HEIGHT),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);
    draw_browser_header(frame, chunks[0], state);
    draw_catalog_list(frame, chunks[1], state);
    draw_browser_footer(frame, chunks[2], state);
}

fn draw_browser_header(frame: &mut Frame, area: Rect, state: &State) {
    let total = state.catalog.len();
    let visible = state.visible_count();
    let title = if visible == total {
        format!(" giant - {total} targets ")
    } else {
        format!(" giant - {visible} of {total} visible ")
    };
    let mut spans: Vec<Span> = vec![Span::styled(
        title,
        Style::default().add_modifier(Modifier::BOLD),
    )];
    for chip in filter_chips(state) {
        spans.push(Span::raw(" "));
        spans.push(chip);
    }
    let para = Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::ALL));
    frame.render_widget(para, area);
}

fn draw_catalog_list(frame: &mut Frame, area: Rect, state: &State) {
    let rows: Vec<Line> = state
        .filtered_catalog()
        .iter()
        .skip(state.scroll_offset)
        .map(|(id, entry)| catalog_row(id, entry))
        .collect();
    let body = if rows.is_empty() && !state.filters.search.is_empty() {
        vec![Line::from(Span::styled(
            "  no targets match the current filters",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        rows
    };
    let para = Paragraph::new(body).wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn catalog_row<'a>(id: &'a TargetId, entry: &'a CatalogEntry) -> Line<'a> {
    let dot = Span::styled("  · ", Style::default().fg(Color::DarkGray));
    let id_span = Span::raw(format!("{:<48}", truncate(id.as_str(), 48)));
    let mut tags: Vec<&str> = entry.tags.iter().map(|s| s.as_str()).collect();
    tags.sort();
    let tags_text = if tags.is_empty() {
        String::new()
    } else {
        format!("{:<20}", truncate(&tags.join(","), 20))
    };
    let tag_span = Span::styled(tags_text, Style::default().fg(Color::Magenta));
    let test_span = if entry.test {
        Span::styled(
            " test ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("      ")
    };
    let deps = if entry.deps.is_empty() {
        Span::raw(String::new())
    } else {
        Span::styled(
            format!(
                "  {} dep{}",
                entry.deps.len(),
                if entry.deps.len() == 1 { "" } else { "s" }
            ),
            Style::default().fg(Color::DarkGray),
        )
    };
    Line::from(vec![dot, id_span, tag_span, test_span, deps])
}

fn draw_browser_footer(frame: &mut Frame, area: Rect, state: &State) {
    let hint = state
        .filtered_catalog()
        .first()
        .map(|(_, e)| {
            let one_line: String = e.command.lines().next().unwrap_or("").to_string();
            format!(" first target: {} ", truncate(&one_line, 80))
        })
        .unwrap_or_else(|| " (no target selected) ".into());
    let para = Paragraph::new(Line::from(Span::styled(
        hint,
        Style::default().fg(Color::DarkGray),
    )))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Enter build · / search · t tag · T test · Tab status · c clear · ? help "),
    );
    frame.render_widget(para, area);
}

// ============================================================
// Build view
// ============================================================

fn draw_build_view(frame: &mut Frame, area: Rect, state: &State) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(HEADER_HEIGHT),
            Constraint::Min(0),
            Constraint::Length(FOOTER_HEIGHT),
        ])
        .split(area);
    draw_build_header(frame, chunks[0], state);
    draw_build_target_list(frame, chunks[1], state);
    draw_recent_logs(frame, chunks[2], state);
}

fn draw_build_header(frame: &mut Frame, area: Rect, state: &State) {
    let title = if let (Some(counts), Some(dur)) = (&state.final_summary, state.final_duration_ms) {
        let ok = state.final_ok.unwrap_or(false) && !state.has_failures();
        let verb = if ok { "OK" } else { "FAIL" };
        format!(
            " {verb} · {} built · {} cached · {} failed · {} ",
            counts.built,
            counts.cache_hit,
            counts.failed,
            format_duration_ms(dur)
        )
    } else {
        let elapsed_ms = state
            .started_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let total = state.build_target_count();
        format!(
            " giant - building {} of {} targets · {} ",
            state.running_count(),
            total,
            format_duration_ms(elapsed_ms)
        )
    };
    let style = match (state.final_ok, state.has_failures()) {
        (Some(true), false) => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        (Some(false), _) | (_, true) => {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
        _ => Style::default().add_modifier(Modifier::BOLD),
    };
    let mut spans: Vec<Span> = vec![Span::styled(title, style)];
    for chip in filter_chips(state) {
        spans.push(Span::raw(" "));
        spans.push(chip);
    }
    let para = Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::ALL));
    frame.render_widget(para, area);
}

fn draw_build_target_list(frame: &mut Frame, area: Rect, state: &State) {
    let rows: Vec<Line> = state
        .sorted_build_targets()
        .iter()
        .skip(state.scroll_offset)
        .map(|(id, v)| target_row(id, v))
        .collect();
    let para = Paragraph::new(rows).wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn target_row<'a>(id: &'a TargetId, v: &'a TargetView) -> Line<'a> {
    let icon = Span::styled(
        format!("  {} ", status_icon(v.status)),
        status_style(v.status),
    );
    let id_span = Span::raw(format!("{:<40}", truncate(id.as_str(), 40)));
    let label = Span::styled(
        format!("{:<8}", status_label(v.status)),
        status_style(v.status),
    );
    let dur = match (v.status, v.duration_ms, v.started_at) {
        (TargetStatus::Running, _, Some(t)) => Span::raw(format!(
            "  {}",
            format_duration_ms(t.elapsed().as_millis() as u64)
        )),
        (_, Some(ms), _) if v.status.is_terminal() => {
            Span::raw(format!("  {}", format_duration_ms(ms)))
        }
        _ => Span::raw(String::new()),
    };
    Line::from(vec![icon, id_span, label, dur])
}

fn draw_recent_logs(frame: &mut Frame, area: Rect, state: &State) {
    let height = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = state
        .recent_logs
        .iter()
        .rev()
        .take(height)
        .rev()
        .map(log_row)
        .collect();
    let title = if state.has_failures() && state.final_ok.is_some() {
        " recent (failures above) "
    } else {
        " recent "
    };
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);

    let hint = match state.screen {
        Screen::BuildFinished => " any key → browser · q quit ",
        _ => " Esc stop build · q quit · ? help ",
    };
    let hint_x = area.x + area.width.saturating_sub(hint.len() as u16 + 1);
    let hint_y = area.y + area.height.saturating_sub(1);
    let hint_area = Rect {
        x: hint_x,
        y: hint_y,
        width: hint.len() as u16,
        height: 1,
    };
    if hint_area.width <= area.width && hint_area.x >= area.x {
        let p = Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray)));
        frame.render_widget(p, hint_area);
    }
}

fn log_row(l: &LogLine) -> Line<'_> {
    let stream_color = match l.stream {
        LogStream::Stdout => Color::Reset,
        LogStream::Stderr => Color::Red,
    };
    let prefix = Span::styled(
        format!("[{}] ", l.target.as_str()),
        Style::default().fg(target_color(l.target.as_str())),
    );
    let body = Span::styled(l.line.clone(), Style::default().fg(stream_color));
    Line::from(vec![prefix, body])
}

// ============================================================
// Chips + overlays
// ============================================================

fn filter_chips(state: &State) -> Vec<Span<'static>> {
    let mut chips = Vec::new();
    let chip_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    if !state.filters.search.is_empty() {
        chips.push(Span::styled(
            format!(" /{} ", state.filters.search),
            chip_style,
        ));
    }
    if let Some(tag) = &state.filters.tag {
        chips.push(Span::styled(format!(" #{tag} "), chip_style));
    }
    if state.filters.status != StatusFilter::All
        && matches!(state.screen, Screen::Building | Screen::BuildFinished)
    {
        chips.push(Span::styled(
            format!(" {} ", state.filters.status.label()),
            chip_style,
        ));
    }
    if state.filters.test_only {
        chips.push(Span::styled(
            " tests-only ".to_string(),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    chips
}

fn draw_search_bar(frame: &mut Frame, area: Rect, state: &State) {
    let h: u16 = 3;
    let w = area.width.saturating_sub(4).min(60);
    let x = area.x + 2;
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(FOOTER_HEIGHT + h + 1));
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    let body = Line::from(vec![
        Span::styled("/", Style::default().fg(Color::Yellow)),
        Span::raw(state.filters.search.clone()),
        Span::styled(
            "▏",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::SLOW_BLINK),
        ),
    ]);
    let para = Paragraph::new(body).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" search (Enter commit · Esc clear) ")
            .style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(para, rect);
}

fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    let w = 60.min(area.width.saturating_sub(2));
    let h = 18.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    let lines = vec![
        Line::from(""),
        Line::from("  Browser:"),
        Line::from("    Enter / b    build the current filter selection"),
        Line::from("    /            search target ids (substring, or"),
        Line::from("                 glob: bin:*, docker:**, etc.)"),
        Line::from("    t            cycle through known tags"),
        Line::from("    T            toggle test-only filter"),
        Line::from("    Tab / f      cycle status filter (build screens)"),
        Line::from("    c            clear all filters"),
        Line::from("    j/k g/G PgUp/PgDn   scroll"),
        Line::from(""),
        Line::from("  Build:"),
        Line::from("    Esc / Ctrl-C    stop build, return to browser"),
        Line::from(""),
        Line::from("    q / Ctrl-C  (when no build) quit"),
        Line::from("    ?           this help - any key dismisses"),
    ];
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" keys ")
            .style(Style::default().fg(Color::White)),
    );
    frame.render_widget(para, rect);
}

fn draw_error_banner(frame: &mut Frame, area: Rect, msg: &str) {
    // One line at the bottom of the screen.
    let h: u16 = 1;
    let y = area.y + area.height.saturating_sub(h);
    let rect = Rect {
        x: area.x,
        y,
        width: area.width,
        height: h,
    };
    let text = format!(" ! {} ", truncate(msg, area.width.saturating_sub(4) as usize));
    let para = Paragraph::new(Span::styled(
        text,
        Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(para, rect);
}

fn draw_collapsed(frame: &mut Frame, area: Rect, state: &State) {
    let summary = match state.screen {
        Screen::Browser | Screen::Loading => format!("giant - {} targets", state.catalog.len()),
        _ => format!(
            "giant - {} / {} targets",
            state.running_count(),
            state.build_target_count()
        ),
    };
    let para = Paragraph::new(summary);
    frame.render_widget(para, area);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

pub fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let s = ms / 1000;
        format!("{}m{}s", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use giant::events::Event;

    #[test]
    fn duration_formats_at_each_unit_boundary() {
        assert_eq!(format_duration_ms(0), "0ms");
        assert_eq!(format_duration_ms(999), "999ms");
        assert_eq!(format_duration_ms(1000), "1.0s");
        assert_eq!(format_duration_ms(60_000), "1m0s");
        assert_eq!(format_duration_ms(125_000), "2m5s");
    }

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn truncate_adds_ellipsis_when_over() {
        assert_eq!(truncate("0123456789abcdef", 10), "012345678…");
    }

    fn render_to_string(state: &State) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| draw(f, state)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area().height)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn loading_screen_message() {
        let state = State::default();
        let dump = render_to_string(&state);
        assert!(dump.contains("giant tui"));
        assert!(dump.contains("starting"));
    }

    #[test]
    fn browser_screen_lists_catalog() {
        let mut state = State::default();
        state.apply(Event::TargetDescribed {
            id: giant::model::TargetId::new("go:bin:server"),
            tags: vec!["release".into()],
            test: false,
            command: "go build".into(),
            inputs: vec![],
            outputs: vec![],
            deps: vec![giant::model::TargetId::new("proto:api")],
        });
        state.apply(Event::EngineReady);
        let dump = render_to_string(&state);
        assert!(dump.contains("go:bin:server"));
        assert!(dump.contains("release"));
    }

    #[test]
    fn build_view_renders_targets() {
        let mut state = State {
            screen: Screen::Building,
            ..State::default()
        };
        state.apply(Event::BuildStarted {
            id: "b_1".into(),
            selection: vec![],
            target_ids: vec![
                giant::model::TargetId::new("a"),
                giant::model::TargetId::new("b"),
            ],
            parallelism: 2,
        });
        let dump = render_to_string(&state);
        assert!(dump.contains("building"));
        assert!(dump.contains("a"));
        assert!(dump.contains("b"));
    }
}
