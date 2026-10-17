use crate::worker::Event;
use fsearch_core::{IndexProgress, IndexSnapshot, IndexedEntry};
use std::path::Path;
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
    pub snapshot: Option<Arc<IndexSnapshot>>,
    pub indices: Vec<usize>,
    pub total_matches: usize,
    pub progress: IndexProgress,
    pub diagnostics: Vec<String>,
    pub phase: Phase,
    pub selected: Option<usize>,
    pub stale: bool,
    pub building: bool,
    pub failure: Option<String>,
    cancelled_scan_generation: Option<u64>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            scan_generation: 0,
            query_generation: 0,
            snapshot: None,
            indices: Vec::new(),
            total_matches: 0,
            progress: IndexProgress::default(),
            diagnostics: Vec::new(),
            phase: Phase::Idle,
            selected: None,
            stale: false,
            building: false,
            failure: None,
            cancelled_scan_generation: None,
        }
    }
}

impl ViewState {
    pub fn has_snapshot(&self) -> bool {
        self.snapshot.is_some()
    }

    pub fn entry(&self, index: usize) -> Option<&IndexedEntry> {
        self.snapshot.as_ref()?.get(index)
    }

    pub fn begin_scan(&mut self, root: &Path) {
        self.scan_generation += 1;
        self.query_generation += 1;
        // Path equality may normalize components. A retained snapshot must have
        // the same original spelling because exact-path matching preserves it.
        let applicable = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.root().as_os_str() == root.as_os_str());
        if !applicable {
            self.snapshot = None;
            self.clear_results();
        }
        self.stale = applicable;
        self.progress = IndexProgress::default();
        self.diagnostics.clear();
        self.failure = None;
        self.cancelled_scan_generation = None;
        self.building = true;
        self.phase = Phase::Scanning;
    }

    pub fn change_query(&mut self) {
        self.query_generation += 1;
        self.clear_results();
        self.phase = if self.building {
            Phase::Scanning
        } else if self.has_snapshot() {
            Phase::Querying
        } else {
            Phase::Idle
        };
    }

    pub fn cancel(&mut self) {
        self.query_generation += 1;
        self.clear_results();
        if self.building {
            self.cancelled_scan_generation = Some(self.scan_generation);
        }
        self.stale |= self.building && self.has_snapshot();
        self.building = false;
        self.phase = Phase::Cancelled;
    }

    fn clear_results(&mut self) {
        self.indices.clear();
        self.total_matches = 0;
        self.selected = None;
    }

    fn snapshot_identity(&self) -> Option<(u64, u64)> {
        self.snapshot
            .as_ref()
            .map(|snapshot| (snapshot.generation(), snapshot.revision()))
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Progress {
                scan_generation,
                progress,
            } if scan_generation == self.scan_generation && self.building => {
                self.progress = progress;
            }
            Event::Published {
                scan_generation,
                snapshot,
            } if scan_generation == self.scan_generation && self.building => {
                self.progress = snapshot.progress();
                self.snapshot = Some(snapshot);
                self.stale = false;
                self.clear_results();
            }
            Event::Finished {
                scan_generation,
                snapshot,
                stale,
                cancelled,
                progress,
                diagnostics,
                failure,
            } if scan_generation == self.scan_generation => {
                let was_cancelled = self.phase == Phase::Cancelled;
                // A query may resume before a queued build completion arrives.
                // Its phase no longer records that the build was cancelled.
                let cancelled_build = self.cancelled_scan_generation == Some(scan_generation);
                let previous_identity = self.snapshot_identity();
                self.building = false;
                self.snapshot = snapshot;
                if self.snapshot_identity() != previous_identity {
                    self.clear_results();
                }
                self.stale = stale
                    || ((was_cancelled || cancelled_build || cancelled) && self.has_snapshot());
                self.progress = progress;
                self.diagnostics = diagnostics;
                self.failure = failure;
                self.phase =
                    if was_cancelled || ((cancelled || cancelled_build) && !self.has_snapshot()) {
                        Phase::Cancelled
                    } else if cancelled_build {
                        Phase::Querying
                    } else if cancelled {
                        Phase::Cancelled
                    } else if self.failure.is_some() {
                        Phase::Failed
                    } else if self.has_snapshot() {
                        Phase::Querying
                    } else {
                        Phase::Failed
                    };
            }
            Event::Results {
                scan_generation,
                query_generation,
                snapshot_generation,
                snapshot_revision,
                indices,
                total_matches,
            } if scan_generation == self.scan_generation
                && query_generation == self.query_generation
                && self.snapshot_identity() == Some((snapshot_generation, snapshot_revision))
                && self.phase != Phase::Cancelled =>
            {
                self.indices = indices;
                self.total_matches = total_matches;
                self.selected = (!self.indices.is_empty()).then_some(0);
                self.phase = if self.building {
                    Phase::Scanning
                } else {
                    Phase::Ready
                };
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
    use fsearch_core::{build_index, IndexBuildEvent, IndexLimits};
    use std::error::Error;
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> io::Result<Self> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos();
            let directory = Self(std::env::temp_dir().join(format!(
                "fsearch-model-{}-{timestamp}-{sequence}",
                std::process::id()
            )));
            fs::create_dir(&directory.0)?;
            fs::write(directory.0.join("first.txt"), [])?;
            fs::write(directory.0.join("second.txt"), [])?;
            Ok(directory)
        }

        fn snapshot(&self, generation: u64) -> Result<Arc<IndexSnapshot>, Box<dyn Error>> {
            Ok(Arc::new(build_index(
                &self.0,
                generation,
                IndexLimits::default(),
                |_| true,
            )?))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0) {
                if error.kind() != io::ErrorKind::NotFound {
                    eprintln!("Cannot remove model fixture {:?}: {error}", self.0);
                }
            }
        }
    }

    fn finished(scan_generation: u64, snapshot: Arc<IndexSnapshot>) -> Event {
        Event::Finished {
            scan_generation,
            progress: snapshot.progress(),
            snapshot: Some(snapshot),
            stale: false,
            cancelled: false,
            diagnostics: Vec::new(),
            failure: None,
        }
    }

    fn results(scan: u64, query: u64, snapshot: &IndexSnapshot) -> Event {
        Event::Results {
            scan_generation: scan,
            query_generation: query,
            snapshot_generation: snapshot.generation(),
            snapshot_revision: snapshot.revision(),
            indices: vec![0],
            total_matches: 1,
        }
    }

    #[test]
    fn same_root_rebuild_failure_retains_applicable_snapshot_and_results(
    ) -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let snapshot = directory.snapshot(1)?;
        let mut state = ViewState::default();
        state.begin_scan(&directory.0);
        state.apply(finished(1, Arc::clone(&snapshot)));
        state.apply(results(1, 1, &snapshot));
        state.begin_scan(&directory.0);
        assert_eq!(state.scan_generation, 2);
        assert_eq!(state.indices, vec![0]);
        assert!(state.stale);
        assert!(state.building);

        state.apply(Event::Finished {
            scan_generation: 2,
            snapshot: Some(Arc::clone(&snapshot)),
            stale: true,
            cancelled: false,
            progress: IndexProgress::default(),
            diagnostics: vec!["root inaccessible".into()],
            failure: Some("rebuild failed".into()),
        });
        assert_eq!(state.phase, Phase::Failed);
        assert!(!state.building);
        assert!(state.stale);
        assert_eq!(state.snapshot_identity(), Some((1, snapshot.revision())));
        assert_eq!(state.indices, vec![0]);
        assert_eq!(state.total_matches, 1);
        assert_eq!(state.failure.as_deref(), Some("rebuild failed"));
        assert_eq!(
            state.entry(0).map(|entry| &entry.path),
            snapshot.get(0).map(|entry| &entry.path)
        );
        Ok(())
    }

    #[test]
    fn changed_root_clears_snapshot_results_and_stale_state() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let snapshot = directory.snapshot(1)?;
        let mut state = ViewState::default();
        state.begin_scan(&directory.0);
        state.apply(finished(1, Arc::clone(&snapshot)));
        state.apply(results(1, 1, &snapshot));
        state.begin_scan(&directory.0.join("other"));
        assert!(!state.has_snapshot());
        assert!(!state.stale);
        assert!(state.indices.is_empty());
        assert_eq!(state.selected, None);
        assert_eq!(state.total_matches, 0);
        assert!(state.entry(0).is_none());
        Ok(())
    }

    #[test]
    fn changed_raw_root_spelling_does_not_reuse_snapshot() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let snapshot = directory.snapshot(1)?;
        let mut state = ViewState::default();
        state.begin_scan(&directory.0);
        state.apply(finished(1, snapshot));
        let mut spelling = directory.0.as_os_str().to_os_string();
        spelling.push("/.");
        state.begin_scan(Path::new(&spelling));
        assert!(!state.has_snapshot());
        assert!(!state.stale);
        Ok(())
    }

    #[test]
    fn querying_retained_snapshot_does_not_clear_stale_or_failure() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let snapshot = directory.snapshot(1)?;
        let mut state = ViewState::default();
        state.begin_scan(&directory.0);
        state.apply(Event::Finished {
            scan_generation: 1,
            progress: snapshot.progress(),
            snapshot: Some(Arc::clone(&snapshot)),
            stale: true,
            cancelled: false,
            diagnostics: vec!["rebuild failed".into()],
            failure: Some("rebuild failed".into()),
        });
        state.change_query();
        assert_eq!(state.phase, Phase::Querying);
        state.apply(results(1, 2, &snapshot));
        assert_eq!(state.phase, Phase::Ready);
        assert!(state.stale);
        assert_eq!(state.failure.as_deref(), Some("rebuild failed"));
        assert_eq!(state.diagnostics, vec!["rebuild failed"]);
        Ok(())
    }

    #[test]
    fn cancelled_scan_rejects_queued_preview_progress_and_success_state(
    ) -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let snapshot = directory.snapshot(1)?;
        let mut state = ViewState::default();
        state.begin_scan(&directory.0);
        state.cancel();
        state.apply(Event::Published {
            scan_generation: 1,
            snapshot: Arc::clone(&snapshot),
        });
        state.apply(Event::Progress {
            scan_generation: 1,
            progress: snapshot.progress(),
        });
        assert!(!state.has_snapshot());
        assert_eq!(state.progress, IndexProgress::default());
        assert_eq!(state.scan_generation, 1);
        assert_eq!(state.query_generation, 2);
        state.apply(finished(1, Arc::clone(&snapshot)));
        state.apply(results(1, 2, &snapshot));
        assert_eq!(state.phase, Phase::Cancelled);
        assert!(state.stale);
        assert!(!state.building);
        assert!(state.indices.is_empty());
        Ok(())
    }

    #[test]
    fn cancelled_build_keeps_snapshot_and_marks_it_stale() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let snapshot = directory.snapshot(1)?;
        let mut state = ViewState::default();
        state.begin_scan(&directory.0);
        state.apply(Event::Published {
            scan_generation: 1,
            snapshot,
        });
        state.cancel();
        assert!(state.has_snapshot());
        assert!(state.stale);
        assert_eq!(state.scan_generation, 1);
        assert_eq!(state.phase, Phase::Cancelled);
        state.change_query();
        assert_eq!(state.phase, Phase::Querying);
        assert!(state.stale);
        Ok(())
    }

    #[test]
    fn resumed_query_stays_stale_when_cancelled_build_completion_arrives(
    ) -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let snapshot = directory.snapshot(1)?;
        let mut state = ViewState::default();
        state.begin_scan(&directory.0);
        state.apply(Event::Published {
            scan_generation: 1,
            snapshot: Arc::clone(&snapshot),
        });
        state.cancel();
        state.change_query();
        assert_eq!(state.phase, Phase::Querying);
        state.apply(finished(1, Arc::clone(&snapshot)));
        assert_eq!(state.phase, Phase::Querying);
        assert!(state.stale);
        state.apply(results(1, 3, &snapshot));
        assert_eq!(state.phase, Phase::Ready);
        assert!(state.stale);
        assert_eq!(state.indices, vec![0]);
        Ok(())
    }

    #[test]
    fn query_cancellation_preserves_current_snapshot_for_next_query() -> Result<(), Box<dyn Error>>
    {
        let directory = TestDirectory::new()?;
        let snapshot = directory.snapshot(1)?;
        let mut state = ViewState::default();
        state.begin_scan(&directory.0);
        state.apply(finished(1, Arc::clone(&snapshot)));
        state.cancel();
        assert!(state.has_snapshot());
        assert!(!state.stale);
        assert_eq!(state.scan_generation, 1);
        state.apply(results(1, 1, &snapshot));
        assert_eq!(state.phase, Phase::Cancelled);
        state.change_query();
        state.apply(results(1, 3, &snapshot));
        assert_eq!(state.phase, Phase::Ready);
        assert!(!state.stale);
        Ok(())
    }

    #[test]
    fn old_query_cannot_replace_new_query() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let snapshot = directory.snapshot(1)?;
        let mut state = ViewState::default();
        state.begin_scan(&directory.0);
        state.apply(finished(1, Arc::clone(&snapshot)));
        state.change_query();
        state.apply(results(1, 1, &snapshot));
        assert!(state.indices.is_empty());
        assert_eq!(state.phase, Phase::Querying);
        state.apply(results(1, 2, &snapshot));
        assert_eq!(state.indices, vec![0]);
        assert_eq!(state.phase, Phase::Ready);
        Ok(())
    }

    #[test]
    fn old_scope_progress_previews_errors_and_results_are_ignored() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let snapshot = directory.snapshot(1)?;
        let mut state = ViewState::default();
        state.begin_scan(&directory.0);
        state.begin_scan(&directory.0);
        state.apply(Event::Progress {
            scan_generation: 1,
            progress: snapshot.progress(),
        });
        state.apply(Event::Published {
            scan_generation: 1,
            snapshot: Arc::clone(&snapshot),
        });
        state.apply(Event::Finished {
            scan_generation: 1,
            snapshot: Some(Arc::clone(&snapshot)),
            stale: true,
            cancelled: false,
            progress: snapshot.progress(),
            diagnostics: vec!["old error".into()],
            failure: Some("old failure".into()),
        });
        state.apply(results(1, 1, &snapshot));
        assert_eq!(state.phase, Phase::Scanning);
        assert_eq!(state.progress, IndexProgress::default());
        assert!(!state.has_snapshot());
        assert!(state.failure.is_none());
        assert!(state.diagnostics.is_empty());
        assert!(state.indices.is_empty());
        Ok(())
    }

    #[test]
    fn results_require_current_snapshot_generation_and_revision() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let mut snapshots: Vec<Arc<IndexSnapshot>> = Vec::new();
        let limits = IndexLimits {
            batch_size: 1,
            ..IndexLimits::default()
        };
        let final_snapshot = Arc::new(build_index(&directory.0, 1, limits, |event| {
            if let IndexBuildEvent::Published(snapshot) = event {
                snapshots.push(snapshot.into());
            }
            true
        })?);
        assert!(snapshots.len() >= 2);
        let first = &snapshots[0];
        let second = &snapshots[1];
        assert_ne!(first.revision(), second.revision());
        let mut state = ViewState::default();
        state.begin_scan(&directory.0);
        state.apply(Event::Published {
            scan_generation: 1,
            snapshot: Arc::clone(first),
        });
        state.apply(results(1, 1, first));
        assert_eq!(state.indices, vec![0]);
        assert_eq!(state.phase, Phase::Scanning);
        assert!(state.building);

        state.apply(Event::Published {
            scan_generation: 1,
            snapshot: Arc::clone(second),
        });
        assert!(state.indices.is_empty());
        state.apply(results(1, 1, first));
        assert!(state.indices.is_empty());
        let different_generation = directory.snapshot(99)?;
        state.apply(results(1, 1, &different_generation));
        assert!(state.indices.is_empty());
        state.apply(results(1, 1, second));
        assert_eq!(state.indices, vec![0]);

        state.apply(finished(1, Arc::clone(&final_snapshot)));
        assert!(state.indices.is_empty());
        assert_eq!(state.phase, Phase::Querying);
        state.apply(results(1, 1, second));
        assert!(state.indices.is_empty());
        state.apply(results(1, 1, &final_snapshot));
        assert_eq!(state.phase, Phase::Ready);
        assert_eq!(state.selected, Some(0));
        Ok(())
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
