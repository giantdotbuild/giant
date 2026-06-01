//! TUI state. Pure data + a single `apply(Event)` entry point.
//!
//! State is driven by the events stream coming out of a `giant
//! session` subprocess (TDD-0014). The state machine has four
//! visible screens; transitions are triggered by both engine events
//! (catalog ready, build finished) and key actions (Enter, Esc).

use giant::events::{Event, LogStream, TargetCounts, TargetResultKind};
use giant::model::TargetId;
use giant::selection::{PatternMatcher, has_glob_chars};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::Instant;

/// Maximum log lines retained per target. Older lines drop off the
/// front of the ring when a target writes more than this. Tuned for
/// "enough to see the failure context without holding 100 MB."
pub const LOGS_PER_TARGET_CAP: usize = 500;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Initial catalog stream in flight. Waiting for engine.ready.
    #[default]
    Loading,
    /// Default screen: filter / search / pick a selection.
    Browser,
    /// A one-shot build is running.
    Building,
    /// File-watching mode: cycles rebuild on file changes. Same UI as
    /// `Building` but `Esc` stops the watch rather than the build.
    Watching,
    /// The build just finished; hold on the summary.
    BuildFinished,
}

#[derive(Debug, Default, Clone)]
pub struct Filters {
    pub search: String,
    /// Targets must carry at least one of these tags (OR). Empty
    /// means "ignore include filter."
    pub tag_include: HashSet<String>,
    /// Targets must carry none of these tags (AND NOT). Empty means
    /// "ignore exclude filter."
    pub tag_exclude: HashSet<String>,
    pub status: StatusFilter,
    pub test_only: bool,
}

impl Filters {
    pub fn has_any_tag_filter(&self) -> bool {
        !self.tag_include.is_empty() || !self.tag_exclude.is_empty()
    }
}

/// State of one tag in the picker.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum TagState {
    #[default]
    Neutral,
    Include,
    Exclude,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    #[default]
    All,
    Running,
    Failed,
}

impl StatusFilter {
    pub fn cycle(self) -> Self {
        match self {
            Self::All => Self::Running,
            Self::Running => Self::Failed,
            Self::Failed => Self::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Running => "running",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Normal,
    Search,
    Help,
    /// Multi-select tag picker modal. Tracks a cursor; space cycles
    /// the cursor row through neutral → include → exclude → neutral.
    TagPicker,
    /// Entering the git ref baseline for affected mode. Typed chars
    /// accumulate in `state.affected_input`; Enter submits, Esc cancels.
    AffectedPrompt,
    /// Searching within the log pane. Typed chars accumulate in
    /// `state.log_search`; the pane filters to matching lines until the
    /// query is cleared (Esc or empty + Enter).
    LogSearch,
}

/// "Affected since <base>" filter on the catalog. While set, the
/// browser shows only target IDs the engine reports as affected vs
/// the git baseline. Recomputed automatically when the workspace
/// changes (TUI runs a file watcher behind the scenes).
#[derive(Debug, Clone)]
pub struct AffectedState {
    pub base: String,
    pub ids: HashSet<TargetId>,
    /// True while a recompute is in flight. The status badge shows
    /// "refreshing…" so the user knows the displayed set may be stale.
    pub refreshing: bool,
    pub last_error: Option<String>,
    pub last_refresh: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetStatus {
    Queued,
    Running,
    Built,
    Cached,
    Remote,
    External,
    Skipped,
    Failed,
}

impl TargetStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LiveCounts {
    pub queued: usize,
    pub running: usize,
    pub built: usize,
    pub cached: usize,
    pub failed: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone)]
pub struct TargetView {
    pub status: TargetStatus,
    pub started_at: Option<Instant>,
    pub finished_at: Option<Instant>,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
}

impl Default for TargetView {
    fn default() -> Self {
        Self {
            status: TargetStatus::Queued,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub target: TargetId,
    pub stream: LogStream,
    pub line: String,
}

/// Static metadata for one target, learned from `target.described`.
#[derive(Debug, Default, Clone)]
pub struct CatalogEntry {
    pub tags: HashSet<String>,
    pub test: bool,
    pub command: String,
    pub deps: Vec<TargetId>,
}

#[derive(Debug, Default)]
pub struct State {
    pub screen: Screen,
    pub catalog: BTreeMap<TargetId, CatalogEntry>,
    pub catalog_error: Option<String>,
    /// Last error message from the session (e.g. command.rejected).
    /// Cleared on the next state-mutating action.
    pub last_error: Option<String>,
    // Build state (populated while screen is Building/BuildFinished):
    pub build_id: Option<String>,
    pub parallelism: usize,
    pub started_at: Option<Instant>,
    pub targets: BTreeMap<TargetId, TargetView>,
    /// Per-target log ring buffers. Each target keeps its last N
    /// lines; the bottom pane in the build view shows whichever
    /// target the cursor points at.
    pub logs: HashMap<TargetId, VecDeque<LogLine>>,
    pub final_summary: Option<TargetCounts>,
    pub final_duration_ms: Option<u64>,
    pub final_ok: Option<bool>,
    // Cross-screen UI state:
    pub scroll_offset: usize,
    pub filters: Filters,
    pub mode: Mode,
    /// command_id → expected build_id, populated from command.accepted.
    /// Tracks the build the TUI just kicked off so it can cancel by id.
    pub pending_build_id: Option<String>,
    /// True while we're shutting down the session. UI shows a
    /// "quitting…" overlay so the user has feedback during the
    /// (typically <50ms) drain.
    pub quitting: bool,
    /// Cursor inside the tag picker modal.
    pub tag_picker_cursor: usize,
    /// Cursor in the build view's target list. Indexes into the
    /// sorted/filtered build targets; clamped on each access. The
    /// selected target's logs render in the bottom pane.
    pub build_cursor: usize,

    /// "Affected since <base>" filter. None = off.
    pub affected: Option<AffectedState>,
    /// Buffer for the `AffectedPrompt` text input.
    pub affected_input: String,
    /// Workspace root absolute path. Populated from `engine.hello`'s
    /// `workspace` field once the session has reported it; needed for
    /// spawning the file watcher that drives affected auto-refresh.
    pub workspace_root: Option<std::path::PathBuf>,

    /// Substring filter applied to the log pane. Case-insensitive.
    /// Empty means "show everything." Cleared by Esc or empty Enter
    /// in `Mode::LogSearch`.
    pub log_search: String,

    /// Which pane the build view's keys act on. Tab cycles. Catalog
    /// is the target list at the top; Log is the recent-output pane
    /// at the bottom.
    pub focus: Focus,

    /// Number of lines the log pane occupies in the build view.
    /// Clamped to a safe range in the renderer. None = use the
    /// default (`FOOTER_HEIGHT` from `ui.rs`).
    pub log_pane_rows: Option<u16>,

    /// How far up from the tail of the log buffer the user has
    /// scrolled with j/k while the log pane is focused. 0 = follow
    /// tail; bumped up by k, down by j.
    pub log_scroll_back: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    #[default]
    Catalog,
    Log,
}

impl State {
    /// Fold one NDJSON event from the session into the view.
    /// Bootstrap events from the build event stream are filtered.
    pub fn apply(&mut self, ev: Event) {
        if is_bootstrap(&ev) {
            return;
        }
        match ev {
            // ----- Catalog / session lifecycle ----------------------
            Event::TargetDescribed {
                id,
                tags,
                test,
                command,
                deps,
                ..
            } => {
                self.catalog.insert(
                    id,
                    CatalogEntry {
                        tags: tags.into_iter().collect(),
                        test,
                        command,
                        deps,
                    },
                );
            }
            Event::EngineHello { workspace, .. } if !workspace.is_empty() => {
                self.workspace_root = Some(std::path::PathBuf::from(workspace));
            }
            Event::EngineReady if self.screen == Screen::Loading => {
                self.screen = Screen::Browser;
            }
            Event::CatalogInvalidating => {
                self.catalog.clear();
            }
            Event::CatalogReady => {
                // Catalog is fresh; nothing more to do here.
            }
            Event::AffectedChanged { base, target_ids } => {
                // Reconcile against our local subscription state.
                // The engine is authoritative; if the user clicked
                // "clear" we'll have dropped state.affected already,
                // and a stray event (in-flight when we unsubscribed)
                // is ignored.
                if let Some(aff) = self.affected.as_mut()
                    && aff.base == base
                {
                    aff.ids = target_ids.into_iter().collect();
                    aff.refreshing = false;
                    aff.last_error = None;
                    aff.last_refresh = Some(Instant::now());
                }
            }
            Event::AffectedError { base, message } => {
                if let Some(aff) = self.affected.as_mut()
                    && aff.base == base
                {
                    aff.refreshing = false;
                    aff.last_error = Some(message);
                }
            }
            // ----- Build events -------------------------------------
            Event::BuildStarted {
                id,
                target_ids,
                parallelism,
                ..
            } => {
                if self.build_id.as_ref() != Some(&id) {
                    // New build cycle (first one, or a queued one
                    // starting after the previous finished). Reset.
                    self.targets.clear();
                    self.logs.clear();
                    self.final_summary = None;
                    self.final_duration_ms = None;
                    self.final_ok = None;
                    self.build_id = Some(id);
                    self.parallelism = parallelism;
                    self.started_at = Some(Instant::now());
                    self.build_cursor = 0;
                }
                for tid in target_ids {
                    self.targets.entry(tid).or_default();
                }
                if self.screen == Screen::Browser {
                    self.screen = Screen::Building;
                }
            }
            Event::TargetQueued { id, .. } => {
                self.targets.entry(id).or_default();
            }
            Event::TargetStarted { id, .. } => {
                let view = self.targets.entry(id).or_default();
                view.status = TargetStatus::Running;
                view.started_at = Some(Instant::now());
            }
            Event::TargetLog {
                id, stream, line, ..
            } => {
                let buf = self
                    .logs
                    .entry(id.clone())
                    .or_insert_with(|| VecDeque::with_capacity(64));
                if buf.len() >= LOGS_PER_TARGET_CAP {
                    buf.pop_front();
                }
                buf.push_back(LogLine {
                    target: id,
                    stream,
                    line,
                });
            }
            Event::TargetFinished {
                id,
                result,
                duration_ms,
                error,
                ..
            } => {
                let view = self.targets.entry(id).or_default();
                view.status = result_kind_to_status(result);
                view.finished_at = Some(Instant::now());
                view.duration_ms = Some(duration_ms);
                view.error = error;
            }
            Event::BuildFinished {
                ok,
                duration_ms,
                counts,
                ..
            } => {
                self.final_summary = Some(counts);
                self.final_duration_ms = Some(duration_ms);
                self.final_ok = Some(ok);
                if self.screen == Screen::Building {
                    self.screen = Screen::BuildFinished;
                }
            }
            // ----- Command acknowledgements -------------------------
            Event::CommandAccepted { build: Some(b), .. } => {
                self.pending_build_id = Some(b);
            }
            Event::CommandRejected { reason, .. }
            | Event::CommandError {
                message: reason, ..
            } => {
                self.last_error = Some(reason);
            }
            _ => {}
        }
    }

    pub fn start_build_locally(&mut self) {
        // Visual transition is also triggered by build.started; this
        // is a "we sent the command, no event yet, but show the
        // running screen" optimisation so input feels instant.
        self.screen = Screen::Building;
        self.reset_build_state();
    }

    pub fn start_watch_locally(&mut self) {
        self.screen = Screen::Watching;
        self.reset_build_state();
    }

    fn reset_build_state(&mut self) {
        self.targets.clear();
        self.logs.clear();
        self.final_summary = None;
        self.final_duration_ms = None;
        self.final_ok = None;
        self.scroll_offset = 0;
        self.build_cursor = 0;
        self.build_id = None;
        self.pending_build_id = None;
    }

    pub fn return_to_browser(&mut self) {
        self.screen = Screen::Browser;
        self.scroll_offset = 0;
    }

    pub fn running_count(&self) -> usize {
        self.targets
            .values()
            .filter(|v| v.status == TargetStatus::Running)
            .count()
    }

    /// Live counts for the in-flight build. Used in the build header.
    pub fn live_counts(&self) -> LiveCounts {
        let mut c = LiveCounts::default();
        for v in self.targets.values() {
            match v.status {
                TargetStatus::Queued => c.queued += 1,
                TargetStatus::Running => c.running += 1,
                TargetStatus::Cached | TargetStatus::Remote | TargetStatus::External => {
                    c.cached += 1
                }
                TargetStatus::Built => c.built += 1,
                TargetStatus::Failed => c.failed += 1,
                TargetStatus::Skipped => c.skipped += 1,
            }
        }
        c
    }

    pub fn build_target_count(&self) -> usize {
        self.targets.len()
    }

    pub fn has_failures(&self) -> bool {
        self.targets
            .values()
            .any(|v| v.status == TargetStatus::Failed)
    }

    pub fn exit_code(&self) -> i32 {
        match self.final_ok {
            Some(true) => 0,
            _ if self.has_failures() => 1,
            _ => 0,
        }
    }

    /// Sorted, filtered view of the catalog for the browser.
    pub fn filtered_catalog(&self) -> Vec<(&TargetId, &CatalogEntry)> {
        let mut rows: Vec<(&TargetId, &CatalogEntry)> = self
            .catalog
            .iter()
            .filter(|(id, entry)| self.catalog_matches_filters(id, entry))
            .collect();
        rows.sort_by_key(|(id, _)| id.as_str().to_string());
        rows
    }

    fn catalog_matches_filters(&self, id: &TargetId, entry: &CatalogEntry) -> bool {
        if !matches_search(&self.filters.search, id.as_str()) {
            return false;
        }
        if !tags_pass(&self.filters, &entry.tags) {
            return false;
        }
        if self.filters.test_only && !entry.test {
            return false;
        }
        if let Some(aff) = &self.affected
            && !aff.ids.contains(id)
        {
            return false;
        }
        true
    }

    /// Sorted view of build targets - failed first, then running,
    /// queued, then completed.
    pub fn sorted_build_targets(&self) -> Vec<(&TargetId, &TargetView)> {
        let mut entries: Vec<(&TargetId, &TargetView)> = self
            .targets
            .iter()
            .filter(|(id, v)| self.build_target_matches_filters(id, v))
            .collect();
        entries.sort_by_key(|(id, v)| {
            let bucket = match v.status {
                TargetStatus::Failed => 0,
                TargetStatus::Running => 1,
                TargetStatus::Queued => 2,
                _ => 3,
            };
            (bucket, (*id).as_str().to_string())
        });
        entries
    }

    fn build_target_matches_filters(&self, id: &TargetId, v: &TargetView) -> bool {
        if !matches_search(&self.filters.search, id.as_str()) {
            return false;
        }
        if self.filters.has_any_tag_filter() {
            let tags = self
                .catalog
                .get(id)
                .map(|e| &e.tags)
                .cloned()
                .unwrap_or_default();
            if !tags_pass(&self.filters, &tags) {
                return false;
            }
        }
        match self.filters.status {
            StatusFilter::All => true,
            StatusFilter::Running => v.status == TargetStatus::Running,
            StatusFilter::Failed => v.status == TargetStatus::Failed,
        }
    }

    /// Target IDs the current filter selects.
    pub fn selection_for_build(&self) -> Vec<TargetId> {
        self.filtered_catalog()
            .iter()
            .map(|(id, _)| (*id).clone())
            .collect()
    }

    pub fn known_tags(&self) -> Vec<String> {
        let mut all: HashSet<&str> = HashSet::new();
        for entry in self.catalog.values() {
            all.extend(entry.tags.iter().map(|s| s.as_str()));
        }
        let mut v: Vec<String> = all.into_iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    }

    /// State of the cursor row in the tag picker - useful for the UI
    /// to render a highlight.
    pub fn tag_picker_state(&self, tag: &str) -> TagState {
        if self.filters.tag_include.contains(tag) {
            TagState::Include
        } else if self.filters.tag_exclude.contains(tag) {
            TagState::Exclude
        } else {
            TagState::Neutral
        }
    }

    /// Cycle the cursor row in the tag picker through the three
    /// states: Neutral → Include → Exclude → Neutral.
    pub fn cycle_tag_at_cursor(&mut self) {
        let tags = self.known_tags();
        if tags.is_empty() {
            return;
        }
        let idx = self.tag_picker_cursor.min(tags.len().saturating_sub(1));
        let tag = tags[idx].clone();
        match self.tag_picker_state(&tag) {
            TagState::Neutral => {
                self.filters.tag_include.insert(tag);
            }
            TagState::Include => {
                self.filters.tag_include.remove(&tag);
                self.filters.tag_exclude.insert(tag);
            }
            TagState::Exclude => {
                self.filters.tag_exclude.remove(&tag);
            }
        }
        self.scroll_offset = 0;
    }

    pub fn move_tag_cursor(&mut self, delta: isize) {
        let n = self.known_tags().len();
        if n == 0 {
            self.tag_picker_cursor = 0;
            return;
        }
        let cur = self.tag_picker_cursor as isize;
        let new = (cur + delta).clamp(0, n as isize - 1);
        self.tag_picker_cursor = new as usize;
    }

    pub fn open_tag_picker(&mut self) {
        if self.known_tags().is_empty() {
            // Nothing to pick from - show a friendly error chip
            // instead of entering a useless modal.
            self.last_error = Some("no tags declared in this workspace".into());
            return;
        }
        self.mode = Mode::TagPicker;
        self.tag_picker_cursor = 0;
    }

    pub fn close_tag_picker(&mut self) {
        self.mode = Mode::Normal;
    }

    pub fn cycle_status(&mut self) {
        self.filters.status = self.filters.status.cycle();
        self.scroll_offset = 0;
    }

    pub fn toggle_test_only(&mut self) {
        self.filters.test_only = !self.filters.test_only;
        self.scroll_offset = 0;
    }

    pub fn clear_filters(&mut self) {
        self.filters = Filters::default();
        self.scroll_offset = 0;
    }

    pub fn visible_count(&self) -> usize {
        match self.screen {
            Screen::Browser => self.filtered_catalog().len(),
            Screen::Building | Screen::Watching | Screen::BuildFinished => {
                self.sorted_build_targets().len()
            }
            Screen::Loading => 0,
        }
    }

    pub fn scroll_up(&mut self, n: usize) {
        match self.screen {
            Screen::Building | Screen::Watching | Screen::BuildFinished => {
                self.move_build_cursor(-(n as isize));
            }
            _ => {
                self.scroll_offset = self.scroll_offset.saturating_sub(n);
            }
        }
    }

    pub fn scroll_down(&mut self, n: usize) {
        match self.screen {
            Screen::Building | Screen::Watching | Screen::BuildFinished => {
                self.move_build_cursor(n as isize);
            }
            _ => {
                let max = self.visible_count().saturating_sub(1);
                self.scroll_offset = (self.scroll_offset + n).min(max);
            }
        }
    }

    pub fn scroll_top(&mut self) {
        match self.screen {
            Screen::Building | Screen::Watching | Screen::BuildFinished => {
                self.build_cursor = 0;
            }
            _ => {
                self.scroll_offset = 0;
            }
        }
    }

    pub fn scroll_bottom(&mut self) {
        match self.screen {
            Screen::Building | Screen::Watching | Screen::BuildFinished => {
                let n = self.sorted_build_targets().len();
                self.build_cursor = n.saturating_sub(1);
            }
            _ => {
                self.scroll_offset = self.visible_count().saturating_sub(1);
            }
        }
    }

    /// Toggle which pane the build view's keys act on. Always
    /// resets log_scroll_back when switching to Catalog focus, so
    /// the next time the user moves to Log they get a fresh window
    /// at the tail.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Catalog => Focus::Log,
            Focus::Log => {
                self.log_scroll_back = 0;
                Focus::Catalog
            }
        };
    }

    pub fn log_scroll_up(&mut self, n: usize) {
        self.log_scroll_back = self.log_scroll_back.saturating_add(n);
    }

    pub fn log_scroll_down(&mut self, n: usize) {
        self.log_scroll_back = self.log_scroll_back.saturating_sub(n);
    }

    pub fn log_scroll_to_bottom(&mut self) {
        self.log_scroll_back = 0;
    }

    pub fn log_scroll_to_top(&mut self) {
        self.log_scroll_back = usize::MAX; // clamped in the renderer
    }

    /// Resize the log pane. The renderer clamps to a sane range; we
    /// just bump the stored row count and let the clamp do its job.
    pub fn grow_log_pane(&mut self) {
        let current = self.log_pane_rows.unwrap_or(8);
        self.log_pane_rows = Some(current.saturating_add(2));
    }

    pub fn shrink_log_pane(&mut self) {
        let current = self.log_pane_rows.unwrap_or(8);
        self.log_pane_rows = Some(current.saturating_sub(2).max(4));
    }

    fn move_build_cursor(&mut self, delta: isize) {
        let n = self.sorted_build_targets().len();
        if n == 0 {
            self.build_cursor = 0;
            return;
        }
        let cur = self.build_cursor as isize;
        let new = (cur + delta).clamp(0, n as isize - 1) as usize;
        self.build_cursor = new;
        // The renderer computes its own viewport offset around the
        // cursor each draw - see ui::compute_build_list_offset - so
        // we don't track scroll_offset for the build view here. That
        // means moving the cursor down just moves the cursor down;
        // the list scrolls only when the cursor would otherwise leave
        // the viewport.
    }

    /// Target id at the build cursor, or `None` if the list is empty.
    pub fn selected_build_target(&self) -> Option<TargetId> {
        let rows = self.sorted_build_targets();
        rows.get(self.build_cursor).map(|(id, _)| (*id).clone())
    }

    /// Log buffer for the cursor target. Empty if no logs yet.
    pub fn selected_target_logs(&self) -> &[LogLine] {
        static EMPTY: &[LogLine] = &[];
        let Some(id) = self.selected_build_target() else {
            return EMPTY;
        };
        self.logs.get(&id).map(|d| d.as_slices().0).unwrap_or(EMPTY)
    }
}

/// Same semantics as `selection::SelectionOpts::passes_tags`:
/// - Empty include set → any tag passes the include filter.
/// - Non-empty include set → at least one of the target's tags must
///   appear in include (OR / union).
/// - Excluded tags filter as AND-NOT: target passes only if none of
///   its tags are in the exclude set.
fn tags_pass(filters: &Filters, target_tags: &HashSet<String>) -> bool {
    if !filters.tag_include.is_empty()
        && filters
            .tag_include
            .intersection(target_tags)
            .next()
            .is_none()
    {
        return false;
    }
    if filters
        .tag_exclude
        .intersection(target_tags)
        .next()
        .is_some()
    {
        return false;
    }
    true
}

/// Match a search query against a target id.
///
/// - Empty query → match all.
/// - Query with glob chars (`*`, `?`, `[`) → engine selection language
///   (same matcher `giant build bin:*` uses). Case-sensitive, `:`
///   segments don't cross under `*`. Bad globs while the user is
///   mid-typing match nothing.
/// - No glob chars → case-insensitive substring match.
fn matches_search(query: &str, id: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if has_glob_chars(query) {
        match PatternMatcher::compile(query) {
            Ok(p) => p.matches_str(id),
            Err(_) => false,
        }
    } else {
        id.to_ascii_lowercase()
            .contains(&query.to_ascii_lowercase())
    }
}

fn result_kind_to_status(k: TargetResultKind) -> TargetStatus {
    match k {
        TargetResultKind::Built => TargetStatus::Built,
        TargetResultKind::CacheHit => TargetStatus::Cached,
        TargetResultKind::RemoteCacheHit => TargetStatus::Remote,
        TargetResultKind::ExternalCacheHit => TargetStatus::External,
        TargetResultKind::Skipped => TargetStatus::Skipped,
        TargetResultKind::Failed => TargetStatus::Failed,
    }
}

fn is_bootstrap(ev: &Event) -> bool {
    match ev {
        Event::BuildStarted { id, .. } | Event::BuildFinished { id, .. } => {
            id.starts_with("bootstrap_")
        }
        Event::TargetQueued { build, .. }
        | Event::TargetStarted { build, .. }
        | Event::TargetLog { build, .. }
        | Event::TargetFinished { build, .. }
        | Event::DiscoveryMerged { build, .. } => build.starts_with("bootstrap_"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(s: &str) -> TargetId {
        TargetId::new(s)
    }

    fn described(id: &str, tags: &[&str], test: bool) -> Event {
        Event::TargetDescribed {
            id: tid(id),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            test,
            command: format!("cmd:{id}"),
            inputs: vec![],
            outputs: vec![],
            deps: vec![],
        }
    }

    #[test]
    fn loading_becomes_browser_on_engine_ready() {
        let mut s = State::default();
        assert_eq!(s.screen, Screen::Loading);
        s.apply(described("a", &[], false));
        s.apply(Event::EngineReady);
        assert_eq!(s.screen, Screen::Browser);
        assert_eq!(s.catalog.len(), 1);
    }

    #[test]
    fn catalog_invalidating_clears_catalog() {
        let mut s = State::default();
        s.apply(described("a", &[], false));
        s.apply(described("b", &[], false));
        assert_eq!(s.catalog.len(), 2);
        s.apply(Event::CatalogInvalidating);
        assert!(s.catalog.is_empty());
        s.apply(described("c", &[], false));
        s.apply(Event::CatalogReady);
        assert_eq!(s.catalog.len(), 1);
    }

    #[test]
    fn build_started_in_browser_screen_switches_to_building() {
        let mut s = State {
            screen: Screen::Browser,
            ..State::default()
        };
        s.apply(Event::BuildStarted {
            id: "b_1".into(),
            selection: vec![],
            target_ids: vec![tid("a"), tid("b")],
            parallelism: 4,
        });
        assert_eq!(s.screen, Screen::Building);
        assert_eq!(s.targets.len(), 2);
    }

    #[test]
    fn build_finished_in_building_switches_to_finished() {
        let mut s = State {
            screen: Screen::Building,
            ..State::default()
        };
        s.apply(Event::BuildFinished {
            id: "b_1".into(),
            ok: true,
            duration_ms: 50,
            counts: TargetCounts::default(),
        });
        assert_eq!(s.screen, Screen::BuildFinished);
        assert!(s.final_summary.is_some());
    }

    #[test]
    fn command_accepted_records_build_id_for_cancel() {
        let mut s = State::default();
        s.apply(Event::CommandAccepted {
            command_id: "c1".into(),
            build: Some("b_1".into()),
        });
        assert_eq!(s.pending_build_id.as_deref(), Some("b_1"));
    }

    #[test]
    fn command_rejected_sets_last_error() {
        let mut s = State::default();
        s.apply(Event::CommandRejected {
            command_id: "c1".into(),
            reason: "unknown target: x".into(),
        });
        assert_eq!(s.last_error.as_deref(), Some("unknown target: x"));
    }

    #[test]
    fn search_filter_substring_case_insensitive_when_no_globs() {
        let mut s = State::default();
        s.apply(described("go:bin:server", &[], false));
        s.apply(described("docker:api", &[], false));
        s.filters.search = "GO:".into();
        let ids: Vec<&str> = s
            .filtered_catalog()
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(ids, vec!["go:bin:server"]);
    }

    #[test]
    fn search_filter_uses_engine_globs_when_pattern_has_stars() {
        let mut s = State::default();
        s.apply(described("bin:giant", &[], false));
        s.apply(described("bin:giant-tui", &[], false));
        s.apply(described("docker:bin:api", &[], false));
        // `bin:*` should match only `bin:` segment 1, not `docker:bin:api`.
        s.filters.search = "bin:*".into();
        let ids: Vec<&str> = s
            .filtered_catalog()
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(ids, vec!["bin:giant", "bin:giant-tui"]);
    }

    #[test]
    fn search_filter_double_star_crosses_segments() {
        let mut s = State::default();
        s.apply(described("docker:api", &[], false));
        s.apply(described("docker:foo:bar", &[], false));
        // `docker:*` matches one segment after the prefix.
        s.filters.search = "docker:*".into();
        let ids: Vec<&str> = s
            .filtered_catalog()
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(ids, vec!["docker:api"]);
        // `docker:**` matches the rest of the id regardless of depth.
        s.filters.search = "docker:**".into();
        let ids: Vec<&str> = s
            .filtered_catalog()
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(ids, vec!["docker:api", "docker:foo:bar"]);
    }

    #[test]
    fn search_filter_bad_glob_mid_typing_matches_nothing() {
        let mut s = State::default();
        s.apply(described("bin:giant", &[], false));
        s.filters.search = "[".into(); // unterminated class
        assert!(s.filtered_catalog().is_empty());
    }

    #[test]
    fn tag_filter_include_passes_only_matching_targets() {
        let mut s = State::default();
        s.apply(described("a", &["release"], false));
        s.apply(described("b", &[], false));
        s.filters.tag_include.insert("release".into());
        let ids: Vec<&str> = s
            .filtered_catalog()
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(ids, vec!["a"]);
    }

    #[test]
    fn tag_filter_include_unions_multiple_tags() {
        let mut s = State::default();
        s.apply(described("a", &["release"], false));
        s.apply(described("b", &["smoke"], false));
        s.apply(described("c", &["other"], false));
        s.filters.tag_include.insert("release".into());
        s.filters.tag_include.insert("smoke".into());
        let ids: Vec<&str> = s
            .filtered_catalog()
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn tag_filter_exclude_removes_matches() {
        let mut s = State::default();
        s.apply(described("a", &["release"], false));
        s.apply(described("b", &["release", "flaky"], false));
        s.filters.tag_include.insert("release".into());
        s.filters.tag_exclude.insert("flaky".into());
        let ids: Vec<&str> = s
            .filtered_catalog()
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(ids, vec!["a"]);
    }

    #[test]
    fn cycle_tag_at_cursor_steps_through_neutral_include_exclude() {
        let mut s = State::default();
        s.apply(described("a", &["release", "smoke"], false));
        s.apply(described("b", &["smoke"], false));
        // known_tags is sorted: ["release", "smoke"]
        assert_eq!(s.known_tags(), vec!["release", "smoke"]);
        s.tag_picker_cursor = 0;
        s.cycle_tag_at_cursor();
        assert!(s.filters.tag_include.contains("release"));
        s.cycle_tag_at_cursor();
        assert!(!s.filters.tag_include.contains("release"));
        assert!(s.filters.tag_exclude.contains("release"));
        s.cycle_tag_at_cursor();
        assert!(!s.filters.tag_exclude.contains("release"));
    }

    #[test]
    fn open_tag_picker_with_no_tags_flashes_error_instead_of_modal() {
        let mut s = State::default();
        s.apply(described("a", &[], false));
        s.open_tag_picker();
        assert_eq!(s.mode, Mode::Normal);
        assert!(s.last_error.is_some());
    }

    #[test]
    fn move_tag_cursor_clamps_to_known_tags() {
        let mut s = State::default();
        s.apply(described("a", &["x", "y", "z"], false));
        s.move_tag_cursor(-1);
        assert_eq!(s.tag_picker_cursor, 0);
        s.move_tag_cursor(100);
        assert_eq!(s.tag_picker_cursor, 2);
    }

    #[test]
    fn test_only_filter_works() {
        let mut s = State::default();
        s.apply(described("a", &[], false));
        s.apply(described("b", &[], true));
        s.filters.test_only = true;
        let ids: Vec<&str> = s
            .filtered_catalog()
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(ids, vec!["b"]);
    }

    #[test]
    fn selection_for_build_returns_filtered_ids() {
        let mut s = State::default();
        s.apply(described("a", &[], false));
        s.apply(described("b", &[], false));
        s.filters.search = "a".into();
        let sel = s.selection_for_build();
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].as_str(), "a");
    }

    #[test]
    fn return_to_browser_resets_scroll() {
        let mut s = State {
            screen: Screen::BuildFinished,
            scroll_offset: 5,
            ..State::default()
        };
        s.return_to_browser();
        assert_eq!(s.screen, Screen::Browser);
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn bootstrap_events_are_filtered() {
        let mut s = State {
            screen: Screen::Building,
            ..State::default()
        };
        s.apply(Event::TargetStarted {
            build: "bootstrap_x".into(),
            id: tid("discover"),
            cache_key: "".into(),
            command: "".into(),
        });
        assert!(s.targets.is_empty());
    }

    #[test]
    fn target_log_caps_per_target_ring() {
        let mut s = State::default();
        for i in 0..(LOGS_PER_TARGET_CAP + 5) {
            s.apply(Event::TargetLog {
                build: "b".into(),
                id: tid("a"),
                stream: LogStream::Stdout,
                line: format!("l{i}"),
                truncated: false,
            });
        }
        assert_eq!(s.logs[&tid("a")].len(), LOGS_PER_TARGET_CAP);
    }

    #[test]
    fn selected_target_logs_returns_cursor_target_buffer() {
        let mut s = State {
            screen: Screen::Building,
            ..State::default()
        };
        s.apply(Event::BuildStarted {
            id: "b_1".into(),
            selection: vec![],
            target_ids: vec![tid("a"), tid("b")],
            parallelism: 1,
        });
        s.apply(Event::TargetLog {
            build: "b_1".into(),
            id: tid("a"),
            stream: LogStream::Stdout,
            line: "hello from a".into(),
            truncated: false,
        });
        s.apply(Event::TargetLog {
            build: "b_1".into(),
            id: tid("b"),
            stream: LogStream::Stdout,
            line: "hello from b".into(),
            truncated: false,
        });
        // Cursor at 0 → first sorted target (a) - its logs.
        s.build_cursor = 0;
        let logs = s.selected_target_logs();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].line.contains("hello from a"));
        // Cursor at 1 → second target (b).
        s.build_cursor = 1;
        let logs = s.selected_target_logs();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].line.contains("hello from b"));
    }

    #[test]
    fn build_cursor_clamps_to_visible_count() {
        let mut s = State {
            screen: Screen::Building,
            ..State::default()
        };
        s.apply(Event::BuildStarted {
            id: "b_1".into(),
            selection: vec![],
            target_ids: vec![tid("a"), tid("b"), tid("c")],
            parallelism: 1,
        });
        s.scroll_down(100);
        assert_eq!(s.build_cursor, 2);
        s.scroll_up(100);
        assert_eq!(s.build_cursor, 0);
    }
}
