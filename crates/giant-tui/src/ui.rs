//! ratatui draw functions. Two layouts: browser (catalog list) and
//! build view (target list + recent logs).

use crate::colors::{status_icon, status_label, status_style};
use crate::state::{
    CatalogEntry, Focus, LogLine, Mode, Screen, State, StatusFilter, TagState, TargetStatus,
    TargetView,
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
        Screen::Building | Screen::Watching | Screen::BuildFinished => {
            draw_build_view(frame, area, state)
        }
        Screen::Logs => draw_logs(frame, area, state),
    }

    if state.mode == Mode::Search {
        draw_search_bar(frame, area, state);
    }
    if state.mode == Mode::Help {
        draw_help_overlay(frame, area);
    }
    if state.mode == Mode::TagPicker {
        draw_tag_picker(frame, area, state);
    }
    if state.mode == Mode::AffectedPrompt {
        draw_affected_prompt(frame, area, state);
    }
    if state.mode == Mode::Explain {
        draw_explain_overlay(frame, area, state);
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
    let catalog = state.filtered_catalog();
    // `scroll_offset` is the cursor; window so it stays visible and highlight it.
    let height = area.height as usize;
    let cursor = state.scroll_offset.min(catalog.len().saturating_sub(1));
    let start = if cursor >= height {
        cursor - height + 1
    } else {
        0
    };
    let rows: Vec<Line> = catalog
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(i, (id, entry))| {
            catalog_row(id, entry, state.cache_status.get(*id).copied(), i == cursor)
        })
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

fn catalog_row<'a>(
    id: &'a TargetId,
    entry: &'a CatalogEntry,
    cached: Option<bool>,
    selected: bool,
) -> Line<'a> {
    // Cache state glyph (ADR-0033 query.status): filled green = cached, hollow
    // yellow = stale, dim dot = not yet known.
    let (glyph, color) = match cached {
        Some(true) => ("●", Color::Green),
        Some(false) => ("○", Color::Yellow),
        None => ("·", Color::DarkGray),
    };
    let caret = if selected { "▸" } else { " " };
    let dot = Span::styled(format!(" {caret}{glyph} "), Style::default().fg(color));
    let id_style = if selected {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let id_span = Span::styled(format!("{:<48}", truncate(id.as_str(), 48)), id_style);
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
    .block(Block::default().borders(Borders::ALL).title(
        " Enter build · / search · t tag · T test · A affected · R refresh · c clear · ? help ",
    ));
    frame.render_widget(para, area);
}

// ============================================================
// Build view
// ============================================================

fn draw_build_view(frame: &mut Frame, area: Rect, state: &State) {
    let log_h = log_pane_height(area, state);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(HEADER_HEIGHT),
            Constraint::Min(0),
            Constraint::Length(log_h),
        ])
        .split(area);
    draw_build_header(frame, chunks[0], state);
    draw_build_target_list(frame, chunks[1], state);
    draw_recent_logs(frame, chunks[2], state);
}

/// Effective height for the log pane. Honors the user's resize input
/// (state.log_pane_rows) but clamps to the area so the target list
/// never disappears entirely. Min 4 keeps the title bar + 2 lines
/// visible; max is area.height - header - 5.
fn log_pane_height(area: Rect, state: &State) -> u16 {
    let max = area
        .height
        .saturating_sub(HEADER_HEIGHT)
        .saturating_sub(5)
        .max(4);
    state.log_pane_rows.unwrap_or(FOOTER_HEIGHT).clamp(4, max)
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
        let verb = if state.screen == Screen::Watching {
            "rebuilding"
        } else {
            "building"
        };
        let c = state.live_counts();
        format!(
            " giant - {verb} {}/{} · {} built · {} cached{} · {} ",
            c.running,
            total,
            c.built,
            c.cached,
            if c.failed > 0 {
                format!(" · {} failed", c.failed)
            } else {
                String::new()
            },
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
    if state.screen == Screen::Watching {
        // High-contrast badge so it's obvious watch mode is on. Sits
        // right after the verb so the eye picks it up first.
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            " ◉ WATCHING ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    for chip in filter_chips(state) {
        spans.push(Span::raw(" "));
        spans.push(chip);
    }
    let para = Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::ALL));
    frame.render_widget(para, area);
}

fn draw_build_target_list(frame: &mut Frame, area: Rect, state: &State) {
    let targets = state.sorted_build_targets();
    let height = area.height as usize;
    let cursor = state.build_cursor.min(targets.len().saturating_sub(1));
    let offset = compute_build_list_offset(cursor, targets.len(), height);
    let rows: Vec<Line> = targets
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, (id, v))| target_row(id, v, i == cursor))
        .collect();
    let para = Paragraph::new(rows).wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

/// Compute which row index the build-list viewport should start at,
/// given the cursor position, total target count, and viewport
/// height. Keeps the cursor on-screen with a one-row top/bottom
/// margin where possible - moving down only scrolls when the cursor
/// hits the bottom, and vice-versa.
pub fn compute_build_list_offset(cursor: usize, total: usize, height: usize) -> usize {
    if total == 0 || height == 0 {
        return 0;
    }
    let max_offset = total.saturating_sub(height);
    // Center bias: keep the cursor away from the top/bottom edges
    // when there's room. Cursor sits roughly 1/3 down from the top.
    let preferred_top = cursor.saturating_sub(height / 3);
    preferred_top.min(max_offset)
}

fn target_row<'a>(id: &'a TargetId, v: &'a TargetView, selected: bool) -> Line<'a> {
    let cursor = if selected {
        Span::styled(
            "▶ ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    };
    let icon = Span::styled(
        format!("{} ", status_icon(v.status)),
        status_style(v.status),
    );
    let id_style = if selected {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let id_span = Span::styled(format!("{:<40}", truncate(id.as_str(), 40)), id_style);
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
    Line::from(vec![cursor, icon, id_span, label, dur])
}

/// Filter `logs` by the active log search and take the visible window honoring
/// scroll-back. Returns the visible lines and the filtered count (for titles).
/// Shared by the build view's log pane and the full-screen log viewer; filtering
/// happens against the full buffer so a match is found even outside the window.
fn log_window<'a>(state: &State, logs: &'a [LogLine], height: usize) -> (Vec<&'a LogLine>, usize) {
    let q = state.log_search.to_lowercase();
    let filtered: Vec<&LogLine> = if q.is_empty() {
        logs.iter().collect()
    } else {
        logs.iter()
            .filter(|l| l.line.to_lowercase().contains(&q))
            .collect()
    };
    let count = filtered.len();
    let max_back = count.saturating_sub(height);
    let back = state.log_scroll_back.min(max_back);
    let end = count.saturating_sub(back);
    let start = end.saturating_sub(height);
    (filtered[start..end].to_vec(), count)
}

/// Full-screen log viewer for one target (`Screen::Logs`), fed by a `logs.get`
/// replay (ADR-0033). Search reuses `Mode::LogSearch`; j/k scroll; Esc returns.
fn draw_logs(frame: &mut Frame, area: Rect, state: &State) {
    let height = area.height.saturating_sub(2) as usize;
    let logs = state.log_view_logs();
    let (window, filtered_count) = log_window(state, logs, height);
    let mut lines: Vec<Line> = Vec::with_capacity(window.len());
    for l in &window {
        lines.extend(log_lines(l));
    }
    let target = state
        .log_view_target
        .as_ref()
        .map(|t| t.as_str())
        .unwrap_or("");
    let title = if state.mode == Mode::LogSearch {
        format!(
            " logs · {target} · /{}_ ({filtered_count} of {} lines) ",
            state.log_search,
            logs.len()
        )
    } else if !state.log_search.is_empty() {
        format!(
            " logs · {target} · /{} ({filtered_count} of {} lines) · Esc clears ",
            state.log_search,
            logs.len()
        )
    } else if logs.is_empty() {
        format!(" logs · {target} · no captured output · Esc browser ")
    } else {
        format!(
            " logs · {target} · {} lines · Esc browser · / search · q quit ",
            logs.len()
        )
    };
    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(title),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn draw_recent_logs(frame: &mut Frame, area: Rect, state: &State) {
    let height = area.height.saturating_sub(2) as usize;
    let logs = state.selected_target_logs();
    let (window, filtered_count) = log_window(state, logs, height);
    let mut lines: Vec<Line> = Vec::with_capacity(window.len());
    for l in &window {
        lines.extend(log_lines(l));
    }
    let base_title = match state.selected_build_target() {
        Some(id) if !logs.is_empty() => format!(" logs - {} ", id.as_str()),
        Some(id) => format!(" logs - {} (no output yet) ", id.as_str()),
        None => " logs ".to_string(),
    };
    let title = if state.mode == Mode::LogSearch {
        format!(
            " logs - /{}_ ({} of {} lines) ",
            state.log_search,
            filtered_count,
            logs.len()
        )
    } else if !state.log_search.is_empty() {
        format!(
            " logs - /{} ({} of {} lines) · Esc clears ",
            state.log_search,
            filtered_count,
            logs.len()
        )
    } else {
        base_title
    };
    let border_style = if state.focus == Focus::Log {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(title),
        )
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

/// Render one captured log line. Parses ANSI escape sequences so
/// colored cargo / npm / docker output survives. Returns one or more
/// `Line` values (rare - ANSI lines with embedded newlines).
///
/// Stderr lines get a red default style; stdout lines render whatever
/// styles the ANSI parser produced (or no style at all).
fn log_lines(l: &LogLine) -> Vec<Line<'static>> {
    use ansi_to_tui::IntoText;
    let parsed = l.line.into_text().unwrap_or_else(|_| {
        let style = match l.stream {
            LogStream::Stdout => Style::default(),
            LogStream::Stderr => Style::default().fg(Color::Red),
        };
        ratatui::text::Text::from(Span::styled(l.line.clone(), style))
    });
    parsed.lines.into_iter().collect()
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
    let mut include: Vec<&str> = state
        .filters
        .tag_include
        .iter()
        .map(|s| s.as_str())
        .collect();
    include.sort();
    for tag in include {
        chips.push(Span::styled(format!(" +{tag} "), chip_style));
    }
    let exclude_style = Style::default()
        .fg(Color::White)
        .bg(Color::Red)
        .add_modifier(Modifier::BOLD);
    let mut exclude: Vec<&str> = state
        .filters
        .tag_exclude
        .iter()
        .map(|s| s.as_str())
        .collect();
    exclude.sort();
    for tag in exclude {
        chips.push(Span::styled(format!(" -{tag} "), exclude_style));
    }
    if state.filters.status != StatusFilter::All
        && matches!(
            state.screen,
            Screen::Building | Screen::Watching | Screen::BuildFinished
        )
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
    if let Some(aff) = &state.affected {
        let label = if aff.refreshing {
            format!(" affected:{}… ", aff.base)
        } else if let Some(err) = &aff.last_error {
            format!(" affected:{} (error: {}) ", aff.base, truncate(err, 30))
        } else {
            format!(" affected:{} ({}) ", aff.base, aff.ids.len())
        };
        chips.push(Span::styled(
            label,
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
        ));
    }
    chips
}

fn draw_affected_prompt(frame: &mut Frame, area: Rect, state: &State) {
    let w: u16 = 60.min(area.width.saturating_sub(4));
    let h: u16 = 5;
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
        Line::from(Span::raw(" git ref to diff against (e.g. main, HEAD~1):")),
        Line::from(Span::styled(
            format!("  › {}", state.affected_input),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            " Enter to accept · Esc to cancel ",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" affected since… "),
    );
    frame.render_widget(para, rect);
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

fn draw_tag_picker(frame: &mut Frame, area: Rect, state: &State) {
    let tags = state.known_tags();
    let w: u16 = 40.min(area.width.saturating_sub(2));
    let max_h = (tags.len() as u16 + 4).min(area.height.saturating_sub(2));
    let h: u16 = max_h.max(5);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);

    let mut lines: Vec<Line> = Vec::with_capacity(tags.len() + 2);
    lines.push(Line::from(Span::styled(
        " space: toggle  j/k: move  c: clear  Esc/Enter: close ",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(""));
    for (idx, tag) in tags.iter().enumerate() {
        let state_marker = match state.tag_picker_state(tag) {
            TagState::Neutral => Span::styled(" · ", Style::default().fg(Color::DarkGray)),
            TagState::Include => Span::styled(
                " + ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            TagState::Exclude => Span::styled(
                " - ",
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ),
        };
        let cursor = if idx == state.tag_picker_cursor {
            Span::styled(
                "▶ ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("  ")
        };
        let tag_style = if idx == state.tag_picker_cursor {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            cursor,
            state_marker,
            Span::styled(tag.clone(), tag_style),
        ]));
    }
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" tags ")
            .style(Style::default().fg(Color::White)),
    );
    frame.render_widget(para, rect);
}

fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    let w = 60.min(area.width.saturating_sub(2));
    let h = 30.min(area.height.saturating_sub(2));
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
        Line::from("    j/k          move the selection cursor"),
        Line::from("    l            view the selected target's logs"),
        Line::from("    e            explain the selected target's cache key"),
        Line::from("    t            open tag picker (multi-select +/-)"),
        Line::from("    T            toggle test-only filter"),
        Line::from("    Tab / f      cycle status filter (build screens)"),
        Line::from("    c            clear all filters"),
        Line::from("    A            affected since <ref> (auto-refreshes"),
        Line::from("                 on file change)"),
        Line::from("    R            re-run affected refresh"),
        Line::from("    j/k g/G PgUp/PgDn   scroll"),
        Line::from(""),
        Line::from("  Build:"),
        Line::from("    Tab          switch focus (target list ↔ log pane)"),
        Line::from("    j/k g/G PgUp/PgDn   scroll the focused pane"),
        Line::from("    Ctrl-↑ / Ctrl-↓    shrink / grow the log pane"),
        Line::from("    /            substring-filter the log pane"),
        Line::from("                 (Esc clears, Enter commits)"),
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

/// "Why did this run / why cached" overlay (ADR-0033 query.explain). Shows the
/// cache key, cache state, and what feeds the key. Renders a loading line until
/// the `query.explained` reply arrives.
fn draw_explain_overlay(frame: &mut Frame, area: Rect, state: &State) {
    let w = 76.min(area.width.saturating_sub(2));
    let h = 28.min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);

    let body_rows = h.saturating_sub(2) as usize;
    let lines: Vec<Line> = match &state.explain {
        None => vec![Line::from(""), Line::from("  computing cache key…")],
        Some(ex) => {
            let mut v = vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  {}", ex.target.as_str()),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(format!("  key:   {}", ex.key)),
                Line::from(Span::styled(
                    if ex.cached {
                        "  state: cached (action-cache hit at this key)".to_string()
                    } else {
                        "  state: stale (would rebuild)".to_string()
                    },
                    Style::default().fg(if ex.cached {
                        Color::Green
                    } else {
                        Color::Yellow
                    }),
                )),
                Line::from(""),
                Line::from(format!(
                    "  command: {}",
                    truncate(&ex.command, w as usize - 12)
                )),
                Line::from(format!(
                    "  cwd:     {}",
                    if ex.cwd.is_empty() {
                        "(workspace root)"
                    } else {
                        &ex.cwd
                    }
                )),
                Line::from(""),
                Line::from(format!("  file inputs ({}):", ex.file_inputs.len())),
            ];
            for f in &ex.file_inputs {
                v.push(Line::from(Span::styled(
                    format!(
                        "    {}  {}",
                        &f.hash.chars().take(12).collect::<String>(),
                        f.path
                    ),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            if !ex.deps.is_empty() {
                v.push(Line::from(""));
                v.push(Line::from(format!("  deps ({}):", ex.deps.len())));
                for d in &ex.deps {
                    v.push(Line::from(Span::styled(
                        format!("    {}", d.id.as_str()),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            if !ex.env.is_empty() {
                v.push(Line::from(""));
                v.push(Line::from(format!("  env ({}):", ex.env.len())));
                for e in &ex.env {
                    let suffix = if e.built_in { "  (built-in)" } else { "" };
                    v.push(Line::from(Span::styled(
                        format!("    {}={}{}", e.key, truncate(&e.value, 40), suffix),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            // Trim to the box so a huge input list does not overflow.
            v.truncate(body_rows);
            v
        }
    };
    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" explain · any key dismisses ")
                .style(Style::default().fg(Color::White)),
        )
        .wrap(Wrap { trim: false });
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
    let text = format!(
        " ! {} ",
        truncate(msg, area.width.saturating_sub(4) as usize)
    );
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
    fn compute_offset_keeps_cursor_in_view_at_top() {
        // Cursor near top of list, plenty of space → no scroll.
        assert_eq!(compute_build_list_offset(0, 100, 10), 0);
        assert_eq!(compute_build_list_offset(2, 100, 10), 0);
    }

    #[test]
    fn compute_offset_scrolls_when_cursor_goes_past_top_third() {
        // Cursor at 5 with height 10 → preferred top is 5 - 3 = 2.
        assert_eq!(compute_build_list_offset(5, 100, 10), 2);
        // Cursor at 50 → preferred top 50 - 3 = 47.
        assert_eq!(compute_build_list_offset(50, 100, 10), 47);
    }

    #[test]
    fn compute_offset_clamps_to_max_when_near_bottom() {
        // Total 20, height 10 → max_offset = 10. Cursor at 19 should
        // still leave the cursor visible; offset = 10.
        assert_eq!(compute_build_list_offset(19, 20, 10), 10);
        assert_eq!(compute_build_list_offset(15, 20, 10), 10);
    }

    #[test]
    fn compute_offset_handles_empty_and_short_lists() {
        assert_eq!(compute_build_list_offset(0, 0, 10), 0);
        assert_eq!(compute_build_list_offset(2, 5, 10), 0);
    }

    #[test]
    fn watching_screen_shows_watching_badge_in_header() {
        let mut state = State {
            screen: Screen::Watching,
            ..State::default()
        };
        state.apply(Event::BuildStarted {
            id: "b_w_0001".into(),
            selection: vec![],
            target_ids: vec![giant::model::TargetId::new("a")],
            parallelism: 1,
        });
        let dump = render_to_string(&state);
        assert!(
            dump.contains("WATCHING"),
            "expected WATCHING badge in:\n{dump}"
        );
        assert!(
            dump.contains("rebuilding"),
            "expected 'rebuilding' verb in:\n{dump}"
        );
    }

    #[test]
    fn building_screen_does_not_show_watching_badge() {
        let mut state = State {
            screen: Screen::Building,
            ..State::default()
        };
        state.apply(Event::BuildStarted {
            id: "b_1".into(),
            selection: vec![],
            target_ids: vec![giant::model::TargetId::new("a")],
            parallelism: 1,
        });
        let dump = render_to_string(&state);
        assert!(!dump.contains("WATCHING"));
        assert!(dump.contains("building"));
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
