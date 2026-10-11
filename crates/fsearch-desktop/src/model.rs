use crate::worker::{Entry, Event};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Idle,
    Scanning,
    Querying,
    Ready,
    Cancelled,
    Failed,
}

pub struct ViewState {
    pub scan_generation: u64,
    pub query_generation: u64,
    pub entries: Arc<Vec<Entry>>,
    pub indices: Vec<usize>,
    pub total_matches: usize,
    pub visited: usize,
    pub issues: usize,
    pub diagnostics: Vec<String>,
    pub phase: Phase,
    pub has_snapshot: bool,
    pub selected: Option<usize>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            scan_generation: 0,
            query_generation: 0,
            entries: Arc::default(),
            indices: Vec::new(),
            total_matches: 0,
            visited: 0,
            issues: 0,
            diagnostics: Vec::new(),
            phase: Phase::Idle,
            has_snapshot: false,
            selected: None,
        }
    }
}

impl ViewState {
    pub fn begin_scan(&mut self) {
        self.scan_generation += 1;
        self.query_generation += 1;
        self.entries = Arc::default();
        self.has_snapshot = false;
        self.visited = 0;
        self.issues = 0;
        self.diagnostics.clear();
        self.clear_results();
        self.phase = Phase::Scanning;
    }

    pub fn change_query(&mut self) {
        self.query_generation += 1;
        self.clear_results();
        if self.phase != Phase::Scanning {
            self.phase = if self.has_snapshot {
                Phase::Querying
            } else {
                Phase::Idle
            };
        }
    }

    pub fn cancel(&mut self) {
        if self.phase == Phase::Scanning {
            self.scan_generation += 1;
            self.has_snapshot = false;
        }
        self.query_generation += 1;
        self.clear_results();
        self.phase = Phase::Cancelled;
    }

    fn clear_results(&mut self) {
        self.indices.clear();
        self.total_matches = 0;
        self.selected = None;
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Progress {
                scan_generation,
                entries,
                issues,
            } if scan_generation == self.scan_generation && self.phase == Phase::Scanning => {
                self.visited = self.visited.max(entries);
                self.issues = self.issues.max(issues);
            }
            Event::Scanned {
                scan_generation,
                entries,
                issues,
                diagnostics,
            } if scan_generation == self.scan_generation && self.phase == Phase::Scanning => {
                self.visited = entries.len();
                self.entries = entries;
                self.issues = issues;
                self.diagnostics = diagnostics;
                self.has_snapshot = true;
                self.phase = Phase::Querying;
            }
            Event::Results {
                scan_generation,
                query_generation,
                indices,
                total_matches,
            } if scan_generation == self.scan_generation
                && query_generation == self.query_generation
                && self.has_snapshot =>
            {
                self.indices = indices;
                self.total_matches = total_matches;
                self.selected = (!self.indices.is_empty()).then_some(0);
                self.phase = Phase::Ready;
            }
            Event::Failed {
                scan_generation,
                message,
            } if scan_generation == self.scan_generation => {
                self.diagnostics = vec![message];
                self.phase = Phase::Failed;
            }
            Event::Cancelled { scan_generation } if scan_generation == self.scan_generation => {
                self.phase = Phase::Cancelled;
            }
            _ => {}
        }
    }
}

pub fn move_selection(current: Option<usize>, count: usize, down: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match current {
        None => 0,
        Some(row) if down => row.saturating_add(1).min(count - 1),
        Some(row) => row.saturating_sub(1).min(count - 1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanned(generation: u64) -> Event {
        Event::Scanned {
            scan_generation: generation,
            entries: Arc::default(),
            issues: 0,
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn cancelled_scan_rejects_already_queued_completion() {
        let mut state = ViewState::default();
        state.begin_scan();
        state.cancel();
        state.apply(scanned(1));
        assert_eq!(state.phase, Phase::Cancelled);
        assert!(!state.has_snapshot);
    }

    #[test]
    fn old_query_cannot_replace_new_query() {
        let mut state = ViewState::default();
        state.begin_scan();
        state.apply(scanned(1));
        state.change_query();
        state.apply(Event::Results {
            scan_generation: 1,
            query_generation: 1,
            indices: vec![0],
            total_matches: 1,
        });
        assert!(state.indices.is_empty());
        assert_eq!(state.phase, Phase::Querying);
        state.apply(Event::Results {
            scan_generation: 1,
            query_generation: 2,
            indices: Vec::new(),
            total_matches: 0,
        });
        assert_eq!(state.phase, Phase::Ready);
    }

    #[test]
    fn query_cancellation_preserves_snapshot_for_next_query() {
        let mut state = ViewState::default();
        state.begin_scan();
        state.apply(scanned(1));
        state.cancel();
        assert!(state.has_snapshot);
        assert_eq!(state.scan_generation, 1);
        state.apply(Event::Results {
            scan_generation: 1,
            query_generation: 1,
            indices: vec![0],
            total_matches: 1,
        });
        assert_eq!(state.phase, Phase::Cancelled);
        state.change_query();
        assert_eq!(state.phase, Phase::Querying);
    }

    #[test]
    fn old_scope_errors_and_progress_are_ignored() {
        let mut state = ViewState::default();
        state.begin_scan();
        state.begin_scan();
        state.apply(Event::Failed {
            scan_generation: 1,
            message: "old failure".to_owned(),
        });
        state.apply(Event::Progress {
            scan_generation: 1,
            entries: 100,
            issues: 8,
        });
        assert_eq!(state.phase, Phase::Scanning);
        assert_eq!(state.visited, 0);
        assert!(state.diagnostics.is_empty());
    }

    #[test]
    fn keyboard_selection_is_bounded() {
        assert_eq!(move_selection(None, 0, true), None);
        assert_eq!(move_selection(None, 3, true), Some(0));
        assert_eq!(move_selection(Some(0), 3, false), Some(0));
        assert_eq!(move_selection(Some(2), 3, true), Some(2));
        assert_eq!(move_selection(Some(1), 3, false), Some(0));
    }
}
