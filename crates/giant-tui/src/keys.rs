//! Key event → Action mapping. Screen + mode aware.

use crate::state::{Focus, Mode, Screen, State};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use giant::model::TargetId;

#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// User submitted a base ref in the affected prompt - main loop
    /// should kick off a background `giant affected` computation
    /// against the given base and spin up the workspace file watcher.
    RefreshAffected { base: String },
    /// Re-run `giant affected` against the existing base (manual
    /// refresh keystroke).
    RefreshAffectedAgain,
    /// Drop the affected filter; tear down the file watcher.
    ClearAffected,
    /// Open the log viewer for a target: send `logs.get` for it (ADR-0033).
    /// The state transition to `Screen::Logs` is already done.
    ViewLogs(TargetId),
    /// Open the explain overlay for a target: send `query.explain` (ADR-0033).
    /// The overlay (`Mode::Explain`) is already showing.
    Explain(TargetId),
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
        Mode::AffectedPrompt => return handle_affected_prompt(state, key),
        Mode::LogSearch => return handle_log_search(state, key),
        Mode::Explain => return handle_explain(state, key),
        Mode::Normal => {}
    }
    // Any non-Ctrl-C input clears the last-error banner.
    state.last_error = None;
    match state.screen {
        Screen::Loading | Screen::Browser => handle_browser(state, key),
        Screen::Building | Screen::Watching => handle_running_build(state, key),
        Screen::BuildFinished => handle_finished(state, key),
        Screen::Logs => handle_logs(state, key),
    }
}

/// Keys for the full-screen log viewer (`Screen::Logs`). Esc returns to the
/// browser; `/` searches within the logs (reusing `Mode::LogSearch`); j/k
/// scroll; q quits.
fn handle_logs(state: &mut State, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
        KeyCode::Esc => {
            state.log_view_target = None;
            state.log_search.clear();
            state.screen = Screen::Browser;
            Action::Redraw
        }
        KeyCode::Char('/') => {
            state.mode = Mode::LogSearch;
            state.log_search.clear();
            Action::Redraw
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.log_scroll_up(1);
            Action::Redraw
        }
        KeyCode::Char('j') | KeyCode::Down => {
            state.log_scroll_down(1);
            Action::Redraw
        }
        KeyCode::PageUp => {
            state.log_scroll_up(10);
            Action::Redraw
        }
        KeyCode::PageDown => {
            state.log_scroll_down(10);
            Action::Redraw
        }
        KeyCode::Char('g') => {
            state.log_scroll_to_top();
            Action::Redraw
        }
        KeyCode::Char('G') => {
            state.log_scroll_to_bottom();
            Action::Redraw
        }
        _ => Action::Ignore,
    }
}

fn handle_browser(state: &mut State, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
        KeyCode::Enter | KeyCode::Char('b') => Action::StartBuild,
        KeyCode::Char('w') => Action::StartWatch,
        KeyCode::Char('l') => match state.selected_browser_target() {
            Some(target) => {
                state.open_logs(target.clone());
                Action::ViewLogs(target)
            }
            None => {
                state.last_error = Some("no target to show logs for".into());
                Action::Redraw
            }
        },
        KeyCode::Char('e') => match state.selected_browser_target() {
            Some(target) => {
                // Show the overlay immediately (with a loading state); the
                // query.explained reply fills it in (ADR-0033).
                state.explain = None;
                state.mode = Mode::Explain;
                Action::Explain(target)
            }
            None => {
                state.last_error = Some("no target to explain".into());
                Action::Redraw
            }
        },
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
        KeyCode::Char('A') => {
            // Toggle affected mode: enter prompt if off, clear if on.
            if state.affected.is_some() {
                Action::ClearAffected
            } else {
                state.mode = Mode::AffectedPrompt;
                state.affected_input.clear();
                Action::Redraw
            }
        }
        KeyCode::Char('R') => {
            // Manual refresh of the affected set. Only meaningful when
            // affected mode is on.
            if state.affected.is_some() {
                Action::RefreshAffectedAgain
            } else {
                Action::Ignore
            }
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
    // Pane resize: Ctrl-Up / Ctrl-Down grow / shrink the log pane.
    // Independent of which pane has focus, since both panes benefit
    // from the user being able to dial in the split.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Up => {
                state.shrink_log_pane();
                return Action::Redraw;
            }
            KeyCode::Down => {
                state.grow_log_pane();
                return Action::Redraw;
            }
            _ => {}
        }
    }
    // Tab toggles focus between the target list (top) and the log
    // pane (bottom). Each focus interprets j/k differently.
    if matches!(key.code, KeyCode::Tab) {
        state.toggle_focus();
        return Action::Redraw;
    }
    // Log-pane keys when the log has focus: j/k page-scroll the
    // pane (offset from tail), g/G snap to top/bottom of buffer.
    if state.focus == Focus::Log {
        match key.code {
            KeyCode::Char('k') | KeyCode::Up => {
                state.log_scroll_up(1);
                return Action::Redraw;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                state.log_scroll_down(1);
                return Action::Redraw;
            }
            KeyCode::PageUp => {
                state.log_scroll_up(10);
                return Action::Redraw;
            }
            KeyCode::PageDown => {
                state.log_scroll_down(10);
                return Action::Redraw;
            }
            KeyCode::Char('G') => {
                state.log_scroll_to_top();
                return Action::Redraw;
            }
            KeyCode::Char('g') => {
                state.log_scroll_to_bottom();
                return Action::Redraw;
            }
            _ => {}
        }
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Action::Quit,
        KeyCode::Char('/') => {
            state.mode = Mode::LogSearch;
            state.log_search.clear();
            Action::Redraw
        }
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

/// Any key dismisses the explain overlay (ADR-0033).
fn handle_explain(state: &mut State, _key: KeyEvent) -> Action {
    state.mode = Mode::Normal;
    state.explain = None;
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

fn handle_log_search(state: &mut State, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            state.log_search.clear();
            state.mode = Mode::Normal;
            Action::Redraw
        }
        KeyCode::Enter => {
            state.mode = Mode::Normal;
            Action::Redraw
        }
        KeyCode::Backspace => {
            state.log_search.pop();
            Action::Redraw
        }
        KeyCode::Char(c) => {
            if !c.is_control() && !key.modifiers.contains(KeyModifiers::CONTROL) {
                state.log_search.push(c);
                Action::Redraw
            } else {
                Action::Ignore
            }
        }
        _ => Action::Ignore,
    }
}

fn handle_affected_prompt(state: &mut State, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            state.mode = Mode::Normal;
            state.affected_input.clear();
            Action::Redraw
        }
        KeyCode::Enter => {
            let base = state.affected_input.trim().to_string();
            state.mode = Mode::Normal;
            if base.is_empty() {
                Action::Redraw
            } else {
                Action::RefreshAffected { base }
            }
        }
        KeyCode::Backspace => {
            state.affected_input.pop();
            Action::Redraw
        }
        KeyCode::Char(c) => {
            if !c.is_control() && !key.modifiers.contains(KeyModifiers::CONTROL) {
                state.affected_input.push(c);
                Action::Redraw
            } else {
                Action::Ignore
            }
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

    #[test]
    fn a_opens_affected_prompt() {
        let mut s = browser();
        assert_eq!(handle(&mut s, key(KeyCode::Char('A'))), Action::Redraw);
        assert_eq!(s.mode, Mode::AffectedPrompt);
    }

    #[test]
    fn affected_prompt_enter_submits_base() {
        let mut s = browser();
        s.mode = Mode::AffectedPrompt;
        s.affected_input = "main".into();
        let action = handle(&mut s, key(KeyCode::Enter));
        match action {
            Action::RefreshAffected { base } => assert_eq!(base, "main"),
            other => panic!("expected RefreshAffected, got {other:?}"),
        }
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn affected_prompt_esc_cancels() {
        let mut s = browser();
        s.mode = Mode::AffectedPrompt;
        s.affected_input = "main".into();
        handle(&mut s, key(KeyCode::Esc));
        assert_eq!(s.mode, Mode::Normal);
        assert!(s.affected_input.is_empty());
    }

    #[test]
    fn a_clears_when_affected_active() {
        use giant_core_aliases::AffectedState as A;
        let mut s = browser();
        s.affected = Some(A {
            base: "main".into(),
            ids: Default::default(),
            refreshing: false,
            last_error: None,
            last_refresh: None,
        });
        assert_eq!(
            handle(&mut s, key(KeyCode::Char('A'))),
            Action::ClearAffected
        );
    }

    #[test]
    fn tab_in_building_toggles_focus() {
        let mut s = State {
            screen: Screen::Building,
            ..State::default()
        };
        assert_eq!(s.focus, Focus::Catalog);
        handle(&mut s, key(KeyCode::Tab));
        assert_eq!(s.focus, Focus::Log);
        handle(&mut s, key(KeyCode::Tab));
        assert_eq!(s.focus, Focus::Catalog);
    }

    #[test]
    fn j_in_log_focus_scrolls_log() {
        let mut s = State {
            screen: Screen::Building,
            focus: Focus::Log,
            ..State::default()
        };
        s.log_scroll_back = 5;
        handle(&mut s, key(KeyCode::Char('j')));
        assert_eq!(s.log_scroll_back, 4);
        handle(&mut s, key(KeyCode::Char('k')));
        handle(&mut s, key(KeyCode::Char('k')));
        assert_eq!(s.log_scroll_back, 6);
    }

    #[test]
    fn ctrl_down_grows_log_pane() {
        let mut s = State {
            screen: Screen::Building,
            ..State::default()
        };
        handle(&mut s, ctrl_arrow(KeyCode::Down));
        let after = s.log_pane_rows.unwrap_or(8);
        handle(&mut s, ctrl_arrow(KeyCode::Down));
        assert!(s.log_pane_rows.unwrap() > after);
    }

    fn ctrl_arrow(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::CONTROL)
    }

    #[test]
    fn slash_in_building_enters_log_search() {
        let mut s = State {
            screen: Screen::Building,
            ..State::default()
        };
        assert_eq!(handle(&mut s, key(KeyCode::Char('/'))), Action::Redraw);
        assert_eq!(s.mode, Mode::LogSearch);
    }

    #[test]
    fn log_search_typing_appends_then_enter_commits() {
        let mut s = State {
            screen: Screen::Building,
            mode: Mode::LogSearch,
            ..State::default()
        };
        handle(&mut s, key(KeyCode::Char('e')));
        handle(&mut s, key(KeyCode::Char('r')));
        handle(&mut s, key(KeyCode::Char('r')));
        assert_eq!(s.log_search, "err");
        handle(&mut s, key(KeyCode::Enter));
        assert_eq!(s.mode, Mode::Normal);
        assert_eq!(s.log_search, "err"); // persists after commit
    }

    #[test]
    fn log_search_esc_clears() {
        let mut s = State {
            screen: Screen::Building,
            mode: Mode::LogSearch,
            log_search: "warn".into(),
            ..State::default()
        };
        handle(&mut s, key(KeyCode::Esc));
        assert_eq!(s.mode, Mode::Normal);
        assert!(s.log_search.is_empty());
    }

    #[test]
    fn r_triggers_refresh_when_affected_active() {
        use giant_core_aliases::AffectedState as A;
        let mut s = browser();
        s.affected = Some(A {
            base: "main".into(),
            ids: Default::default(),
            refreshing: false,
            last_error: None,
            last_refresh: None,
        });
        assert_eq!(
            handle(&mut s, key(KeyCode::Char('R'))),
            Action::RefreshAffectedAgain
        );
    }

    /// Avoid the long crate-internal path in the tests above.
    mod giant_core_aliases {
        pub use crate::state::AffectedState;
    }
}
