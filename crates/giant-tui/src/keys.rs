//! Key event → Action mapping. Screen + mode aware.

use crate::state::{Mode, Screen, State};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// State mutated locally; redraw needed.
    Redraw,
    /// Quit the TUI (closes the session).
    Quit,
    /// Send {"c":"cancel", build: …} for the in-flight build.
    /// No-op in browser screens.
    CancelChild,
    /// Send {"c":"build", targets: <current selection>}.
    StartBuild,
    /// Send {"c":"watch.start", targets: <current selection>}.
    StartWatch,
    /// Send {"c":"watch.stop"} to exit watch mode.
    StopWatch,
    /// Key consumed but no UI effect.
    Ignore,
}

pub fn handle(state: &mut State, key: KeyEvent) -> Action {
    // Ctrl-C is screen-sensitive: cancels a running build, quits
    // everywhere else. The terminal doesn't translate it to SIGINT in
    // raw mode; the TUI decides what it means.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return match state.screen {
            Screen::Building => Action::CancelChild,
            Screen::Watching => Action::StopWatch,
            _ => Action::Quit,
        };
    }
    match state.mode {
        Mode::Help => return handle_help(state, key),
        Mode::Search => return handle_search(state, key),
        Mode::TagPicker => return handle_tag_picker(state, key),
        Mode::Normal => {}
    }
    // Any non-Ctrl-C input clears the last-error banner.
    state.last_error = None;
    match state.screen {
        Screen::Loading | Screen::Browser => handle_browser(state, key),
        Screen::Building | Screen::Watching => handle_running_build(state, key),
        Screen::BuildFinished => handle_finished(state, key),
    }
}

fn handle_browser(state: &mut State, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
        KeyCode::Enter | KeyCode::Char('b') => Action::StartBuild,
        KeyCode::Char('w') => Action::StartWatch,
        KeyCode::Char('/') => {
            state.mode = Mode::Search;
            state.filters.search.clear();
            state.scroll_offset = 0;
            Action::Redraw
        }
        KeyCode::Char('?') => {
            state.mode = Mode::Help;
            Action::Redraw
        }
        KeyCode::Char('t') => {
            state.open_tag_picker();
            Action::Redraw
        }
        KeyCode::Char('T') => {
            state.toggle_test_only();
            Action::Redraw
        }
        KeyCode::Tab | KeyCode::Char('f') => {
            state.cycle_status();
            Action::Redraw
        }
        KeyCode::Char('c') => {
            state.clear_filters();
            Action::Redraw
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.scroll_up(1);
            Action::Redraw
        }
        KeyCode::Char('j') | KeyCode::Down => {
            state.scroll_down(1);
            Action::Redraw
        }
        KeyCode::Char('g') => {
            state.scroll_top();
            Action::Redraw
        }
        KeyCode::Char('G') => {
            state.scroll_bottom();
            Action::Redraw
        }
        KeyCode::PageUp => {
            state.scroll_up(10);
            Action::Redraw
        }
        KeyCode::PageDown => {
            state.scroll_down(10);
            Action::Redraw
        }
        _ => Action::Ignore,
    }
}

fn handle_running_build(state: &mut State, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
        // Esc semantics:
        // - In Watching: stop the watch (it has no single build to
        //   cancel; watch.stop ends the whole loop).
        // - In Building: cancel the in-flight build.
        // - In BuildFinished: handled in handle_finished before we
        //   get here (it returns to browser).
        KeyCode::Esc => {
            if state.screen == Screen::Watching {
                Action::StopWatch
            } else {
                Action::CancelChild
            }
        }
        KeyCode::Char('?') => {
            state.mode = Mode::Help;
            Action::Redraw
        }
        KeyCode::Tab | KeyCode::Char('f') => {
            state.cycle_status();
            Action::Redraw
        }
        KeyCode::Char('c') => {
            state.clear_filters();
            Action::Redraw
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.scroll_up(1);
            Action::Redraw
        }
        KeyCode::Char('j') | KeyCode::Down => {
            state.scroll_down(1);
            Action::Redraw
        }
        KeyCode::Char('g') => {
            state.scroll_top();
            Action::Redraw
        }
        KeyCode::Char('G') => {
            state.scroll_bottom();
            Action::Redraw
        }
        KeyCode::PageUp => {
            state.scroll_up(10);
            Action::Redraw
        }
        KeyCode::PageDown => {
            state.scroll_down(10);
            Action::Redraw
        }
        _ => Action::Ignore,
    }
}

fn handle_finished(state: &mut State, key: KeyEvent) -> Action {
    // The build is done but the user is still browsing the result -
    // scrolling, looking at logs per-target. Only explicit
    // dismissal keys return to the catalog. Everything else is
    // handled the same way as during a running build, so the cursor
    // moves, filter chips work, etc.
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('b') => {
            state.return_to_browser();
            Action::Redraw
        }
        _ => handle_running_build(state, key),
    }
}

fn handle_help(state: &mut State, _key: KeyEvent) -> Action {
    state.mode = Mode::Normal;
    Action::Redraw
}

fn handle_tag_picker(state: &mut State, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('t') | KeyCode::Char('q') => {
            state.close_tag_picker();
            Action::Redraw
        }
        KeyCode::Char(' ') | KeyCode::Char('i') => {
            state.cycle_tag_at_cursor();
            Action::Redraw
        }
        KeyCode::Char('c') => {
            state.filters.tag_include.clear();
            state.filters.tag_exclude.clear();
            Action::Redraw
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.move_tag_cursor(-1);
            Action::Redraw
        }
        KeyCode::Char('j') | KeyCode::Down => {
            state.move_tag_cursor(1);
            Action::Redraw
        }
        _ => Action::Ignore,
    }
}

fn handle_search(state: &mut State, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            state.filters.search.clear();
            state.mode = Mode::Normal;
            state.scroll_offset = 0;
            Action::Redraw
        }
        KeyCode::Enter => {
            state.mode = Mode::Normal;
            Action::Redraw
        }
        KeyCode::Backspace => {
            state.filters.search.pop();
            state.scroll_offset = 0;
            Action::Redraw
        }
        KeyCode::Char(c) => {
            if !c.is_control() && !key.modifiers.contains(KeyModifiers::CONTROL) {
                state.filters.search.push(c);
                state.scroll_offset = 0;
                Action::Redraw
            } else {
                Action::Ignore
            }
        }
        _ => Action::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn browser() -> State {
        State {
            screen: Screen::Browser,
            ..State::default()
        }
    }

    #[test]
    fn browser_enter_returns_start_build() {
        let mut s = browser();
        assert_eq!(handle(&mut s, key(KeyCode::Enter)), Action::StartBuild);
    }

    #[test]
    fn browser_b_also_starts_build() {
        let mut s = browser();
        assert_eq!(handle(&mut s, key(KeyCode::Char('b'))), Action::StartBuild);
    }

    #[test]
    fn browser_q_quits() {
        let mut s = browser();
        assert_eq!(handle(&mut s, key(KeyCode::Char('q'))), Action::Quit);
    }

    #[test]
    fn ctrl_c_cancels_only_during_a_running_build() {
        let mut s = State {
            screen: Screen::Building,
            ..State::default()
        };
        assert_eq!(handle(&mut s, ctrl('c')), Action::CancelChild);
    }

    #[test]
    fn ctrl_c_quits_when_no_build_is_running() {
        for screen in [Screen::Loading, Screen::Browser, Screen::BuildFinished] {
            let mut s = State {
                screen,
                ..State::default()
            };
            assert_eq!(handle(&mut s, ctrl('c')), Action::Quit);
        }
    }

    #[test]
    fn building_esc_cancels() {
        let mut s = State {
            screen: Screen::Building,
            ..State::default()
        };
        assert_eq!(handle(&mut s, key(KeyCode::Esc)), Action::CancelChild);
    }

    #[test]
    fn building_enter_does_not_start_another_build() {
        let mut s = State {
            screen: Screen::Building,
            ..State::default()
        };
        assert_eq!(handle(&mut s, key(KeyCode::Enter)), Action::Ignore);
    }

    #[test]
    fn finished_q_quits() {
        let mut s = State {
            screen: Screen::BuildFinished,
            ..State::default()
        };
        assert_eq!(handle(&mut s, key(KeyCode::Char('q'))), Action::Quit);
    }

    #[test]
    fn finished_esc_or_enter_returns_to_browser() {
        for k in [KeyCode::Esc, KeyCode::Enter] {
            let mut s = State {
                screen: Screen::BuildFinished,
                ..State::default()
            };
            assert_eq!(handle(&mut s, key(k)), Action::Redraw);
            assert_eq!(s.screen, Screen::Browser);
        }
    }

    #[test]
    fn finished_jk_moves_cursor_does_not_dismiss() {
        use giant::events::{Event, TargetCounts};
        use giant::model::TargetId;
        let mut s = State {
            screen: Screen::BuildFinished,
            ..State::default()
        };
        s.apply(Event::BuildStarted {
            id: "b_1".into(),
            selection: vec![],
            target_ids: vec![TargetId::new("a"), TargetId::new("b")],
            parallelism: 1,
        });
        s.apply(Event::BuildFinished {
            id: "b_1".into(),
            ok: true,
            duration_ms: 1,
            counts: TargetCounts::default(),
        });
        s.screen = Screen::BuildFinished; // BuildFinished after the events
        let initial_cursor = s.build_cursor;
        handle(&mut s, key(KeyCode::Char('j')));
        assert_eq!(s.screen, Screen::BuildFinished, "must stay in build view");
        assert_eq!(s.build_cursor, initial_cursor + 1);
    }

    #[test]
    fn finished_random_key_is_a_noop_not_dismiss() {
        let mut s = State {
            screen: Screen::BuildFinished,
            ..State::default()
        };
        handle(&mut s, key(KeyCode::Char('x')));
        assert_eq!(s.screen, Screen::BuildFinished);
    }

    #[test]
    fn search_mode_typing_appends_to_filter() {
        let mut s = State {
            screen: Screen::Browser,
            mode: Mode::Search,
            ..State::default()
        };
        handle(&mut s, key(KeyCode::Char('g')));
        handle(&mut s, key(KeyCode::Char('o')));
        assert_eq!(s.filters.search, "go");
    }

    #[test]
    fn search_mode_esc_clears_and_exits() {
        let mut s = State {
            screen: Screen::Browser,
            mode: Mode::Search,
            ..State::default()
        };
        s.filters.search = "go:".into();
        handle(&mut s, key(KeyCode::Esc));
        assert_eq!(s.mode, Mode::Normal);
        assert!(s.filters.search.is_empty());
    }

    #[test]
    fn help_mode_any_key_dismisses() {
        let mut s = State {
            screen: Screen::Browser,
            mode: Mode::Help,
            ..State::default()
        };
        handle(&mut s, key(KeyCode::Char('x')));
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn keypress_clears_last_error() {
        let mut s = browser();
        s.last_error = Some("oops".into());
        handle(&mut s, key(KeyCode::Char('j')));
        assert!(s.last_error.is_none());
    }
}
