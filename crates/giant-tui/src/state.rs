//! TUI state. Pure data + a single `apply(Event)` entry point.
//!
//! State is driven by the events stream coming out of a `giant
//! session` subprocess (TDD-0014). The state machine has four
//! visible screens; transitions are triggered by both engine events
//! (catalog ready, build finished) and key actions (Enter, Esc).

use giant::events::{Event, LogStream, TargetCounts, TargetResultKind};
use giant::model::TargetId;
use giant::selection::{PatternMatcher, has_glob_chars};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::time::Instant;

pub const RECENT_LOGS_CAP: usize = 200;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Initial catalog stream in flight. Waiting for engine.ready.
    #[default]
    Loading,
    /// Default screen: filter / search / pick a selection.
    Browser,
    /// A one-shot build is running.
    Building,
    /// The build just finished; hold on the summary.
    BuildFinished,
}

#[derive(Debug, Default, Clone)]
pub struct Filters {
    pub search: String,
    pub tag: Option<String>,
    pub status: StatusFilter,
    pub test_only: bool,
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
    pub recent_logs: VecDeque<LogLine>,
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
            Event::EngineReady if self.screen == Screen::Loading => {
                self.screen = Screen::Browser;
            }
            Event::CatalogInvalidating => {
                self.catalog.clear();
            }
            Event::CatalogReady => {
                // Catalog is fresh; nothing more to do here.
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
                    self.recent_logs.clear();
                    self.final_summary = None;
                    self.final_duration_ms = None;
                    self.final_ok = None;
                    self.build_id = Some(id);
                    self.parallelism = parallelism;
                    self.started_at = Some(Instant::now());
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
                if self.recent_logs.len() >= RECENT_LOGS_CAP {
                    self.recent_logs.pop_front();
                }
                self.recent_logs.push_back(LogLine {
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
            Event::CommandAccepted {
                build: Some(b), ..
            } => {
                self.pending_build_id = Some(b);
            }
            Event::CommandRejected { reason, .. } | Event::CommandError { message: reason, .. } => {
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
        self.targets.clear();
        self.recent_logs.clear();
        self.final_summary = None;
        self.final_duration_ms = None;
        self.final_ok = None;
        self.scroll_offset = 0;
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
        if let Some(tag) = &self.filters.tag
            && !entry.tags.contains(tag)
        {
            return false;
        }
        if self.filters.test_only && !entry.test {
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
        if let Some(tag) = &self.filters.tag {
            let has = self.catalog.get(id).is_some_and(|e| e.tags.contains(tag));
            if !has {
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

    pub fn cycle_tag(&mut self) {
        let tags = self.known_tags();
        if tags.is_empty() {
            return;
        }
        self.filters.tag = match &self.filters.tag {
            None => Some(tags[0].clone()),
            Some(current) => {
                let pos = tags.iter().position(|t| t == current);
                match pos {
                    Some(i) if i + 1 < tags.len() => Some(tags[i + 1].clone()),
                    _ => None,
                }
            }
        };
        self.scroll_offset = 0;
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
            Screen::Building | Screen::BuildFinished => self.sorted_build_targets().len(),
            Screen::Loading => 0,
        }
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    pub fn scroll_down(&mut self, n: usize) {
        let max = self.visible_count().saturating_sub(1);
        self.scroll_offset = (self.scroll_offset + n).min(max);
    }

    pub fn scroll_top(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn scroll_bottom(&mut self) {
        self.scroll_offset = self.visible_count().saturating_sub(1);
    }
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
        id.to_ascii_lowercase().contains(&query.to_ascii_lowercase())
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
        let ids: Vec<&str> = s.filtered_catalog().iter().map(|(id, _)| id.as_str()).collect();
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
    fn tag_filter_uses_catalog_tags() {
        let mut s = State::default();
        s.apply(described("a", &["release"], false));
        s.apply(described("b", &[], false));
        s.filters.tag = Some("release".into());
        let ids: Vec<&str> = s.filtered_catalog().iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["a"]);
    }

    #[test]
    fn test_only_filter_works() {
        let mut s = State::default();
        s.apply(described("a", &[], false));
        s.apply(described("b", &[], true));
        s.filters.test_only = true;
        let ids: Vec<&str> = s.filtered_catalog().iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["b"]);
    }

    #[test]
    fn cycle_tag_steps_through_known_then_back_to_none() {
        let mut s = State::default();
        s.apply(described("a", &["release", "smoke"], false));
        s.apply(described("b", &["release"], false));
        s.cycle_tag();
        assert_eq!(s.filters.tag.as_deref(), Some("release"));
        s.cycle_tag();
        assert_eq!(s.filters.tag.as_deref(), Some("smoke"));
        s.cycle_tag();
        assert_eq!(s.filters.tag, None);
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
    fn target_log_caps_at_ring_capacity() {
        let mut s = State::default();
        for i in 0..(RECENT_LOGS_CAP + 5) {
            s.apply(Event::TargetLog {
                build: "b".into(),
                id: tid("a"),
                stream: LogStream::Stdout,
                line: format!("l{i}"),
                truncated: false,
            });
        }
        assert_eq!(s.recent_logs.len(), RECENT_LOGS_CAP);
    }
}
