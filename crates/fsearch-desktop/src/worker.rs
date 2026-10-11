//! A single background worker with replaceable requests and bounded UI events.

use fsearch_core::search::matches_path;
use fsearch_core::{scan_path_with, SearchMode};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

const RESULT_LIMIT: usize = 20_000;
const DIAGNOSTIC_LIMIT: usize = 64;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub display_path: String,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Link,
    Other,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum EntryFilter {
    All,
    Files,
    Directories,
}

#[derive(Debug, Clone)]
pub struct Query {
    pub text: String,
    pub mode: SearchMode,
    pub filter: EntryFilter,
}

#[derive(Debug)]
pub enum Event {
    Progress {
        scan_generation: u64,
        entries: usize,
        issues: usize,
    },
    Scanned {
        scan_generation: u64,
        entries: Arc<Vec<Entry>>,
        issues: usize,
        diagnostics: Vec<String>,
    },
    Results {
        scan_generation: u64,
        query_generation: u64,
        indices: Vec<usize>,
        total_matches: usize,
    },
    Failed {
        scan_generation: u64,
        message: String,
    },
    Cancelled {
        scan_generation: u64,
    },
}

struct ScanRequest {
    generation: u64,
    root: PathBuf,
}

struct QueryRequest {
    scan_generation: u64,
    generation: u64,
    query: Query,
}

#[derive(Default)]
struct Mailbox {
    scan: Option<ScanRequest>,
    query: Option<QueryRequest>,
}

struct Shared {
    mailbox: Mutex<Mailbox>,
    changed: Condvar,
    scan_generation: AtomicU64,
    query_generation: AtomicU64,
    scan_cancelled: AtomicBool,
    query_cancelled: AtomicBool,
    shutdown: AtomicBool,
}

impl Shared {
    fn new() -> Self {
        Self {
            mailbox: Mutex::new(Mailbox::default()),
            changed: Condvar::new(),
            scan_generation: AtomicU64::new(0),
            query_generation: AtomicU64::new(0),
            scan_cancelled: AtomicBool::new(false),
            query_cancelled: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
        }
    }

    fn mailbox(&self) -> MutexGuard<'_, Mailbox> {
        self.mailbox
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn current_scan(&self, generation: u64) -> bool {
        !self.shutdown.load(Ordering::Acquire)
            && self.scan_generation.load(Ordering::Acquire) == generation
    }

    fn scanning(&self, generation: u64) -> bool {
        self.current_scan(generation) && !self.scan_cancelled.load(Ordering::Acquire)
    }

    fn querying(&self, scan_generation: u64, query_generation: u64) -> bool {
        self.current_scan(scan_generation)
            && self.query_generation.load(Ordering::Acquire) == query_generation
            && !self.query_cancelled.load(Ordering::Acquire)
    }

    fn event_is_current(&self, event: &Event) -> bool {
        match event {
            Event::Progress {
                scan_generation, ..
            }
            | Event::Scanned {
                scan_generation, ..
            }
            | Event::Failed {
                scan_generation, ..
            } => self.scanning(*scan_generation),
            Event::Cancelled { scan_generation } => self.current_scan(*scan_generation),
            Event::Results {
                scan_generation,
                query_generation,
                ..
            } => self.querying(*scan_generation, *query_generation),
        }
    }
}

/// Keeps one pending scan and one pending query. Callers use a new generation
/// for each replacement and discard already queued events from older generations.
pub struct Worker {
    shared: Arc<Shared>,
    pub events: Receiver<Event>,
}

impl Worker {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> io::Result<Self> {
        let shared = Arc::new(Shared::new());
        let (sender, events) = mpsc::sync_channel(8);
        let background = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("fsearch-background".into())
            .spawn(move || run(background, sender, wake))?;
        drop(handle);
        Ok(Self { shared, events })
    }

    /// Replaces the root scan and its initial query without waiting for disk I/O.
    pub fn scan(&self, scan_generation: u64, query_generation: u64, root: PathBuf, query: Query) {
        let mut mailbox = self.shared.mailbox();
        self.shared
            .scan_generation
            .store(scan_generation, Ordering::Release);
        self.shared
            .query_generation
            .store(query_generation, Ordering::Release);
        self.shared.scan_cancelled.store(false, Ordering::Release);
        self.shared.query_cancelled.store(false, Ordering::Release);
        mailbox.scan = Some(ScanRequest {
            generation: scan_generation,
            root,
        });
        mailbox.query = Some(QueryRequest {
            scan_generation,
            generation: query_generation,
            query,
        });
        self.shared.changed.notify_one();
    }

    /// Searches the cached snapshot; updating a query never cancels its scan.
    pub fn query(&self, scan_generation: u64, query_generation: u64, query: Query) {
        let mut mailbox = self.shared.mailbox();
        if !self.shared.current_scan(scan_generation) {
            return;
        }
        self.shared
            .query_generation
            .store(query_generation, Ordering::Release);
        self.shared.query_cancelled.store(false, Ordering::Release);
        mailbox.query = Some(QueryRequest {
            scan_generation,
            generation: query_generation,
            query,
        });
        self.shared.changed.notify_one();
    }

    pub fn cancel_scan(&self) {
        self.shared.scan_cancelled.store(true, Ordering::Release);
        self.shared.query_cancelled.store(true, Ordering::Release);
        self.shared.changed.notify_one();
    }

    pub fn cancel_query(&self) {
        self.shared.query_cancelled.store(true, Ordering::Release);
        self.shared.changed.notify_one();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Use the mailbox mutex to avoid a shutdown notification racing between
        // the worker's empty-mailbox check and its Condvar::wait call.
        let _mailbox = self.shared.mailbox();
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.changed.notify_one();
        // A thread blocked inside an OS filesystem call cannot be interrupted.
        // Detaching avoids making window shutdown wait for that call to return.
    }
}

fn run(shared: Arc<Shared>, sender: SyncSender<Event>, wake: impl Fn()) {
    let mut cache: Option<(u64, Arc<Vec<Entry>>)> = None;
    loop {
        let (scan, query) = {
            let mut mailbox = shared.mailbox();
            while mailbox.scan.is_none()
                && mailbox.query.is_none()
                && !shared.shutdown.load(Ordering::Acquire)
            {
                mailbox = shared
                    .changed
                    .wait(mailbox)
                    .unwrap_or_else(|error| error.into_inner());
            }
            if shared.shutdown.load(Ordering::Acquire) {
                return;
            }
            if let Some(scan) = mailbox.scan.take() {
                (Some(scan), None)
            } else {
                (None, mailbox.query.take())
            }
        };

        if let Some(scan) = scan {
            cache = None;
            if let Some(entries) = build_snapshot(&shared, &sender, &wake, &scan) {
                cache = Some((scan.generation, entries));
            }
        } else if let (Some(request), Some((generation, entries))) = (query, &cache) {
            if request.scan_generation == *generation {
                if let Some((indices, total_matches)) =
                    collect_matches(entries, &request.query, || {
                        shared.querying(request.scan_generation, request.generation)
                    })
                {
                    deliver(
                        &shared,
                        &sender,
                        &wake,
                        Event::Results {
                            scan_generation: request.scan_generation,
                            query_generation: request.generation,
                            indices,
                            total_matches,
                        },
                    );
                }
            }
        }
    }
}

fn build_snapshot(
    shared: &Shared,
    sender: &SyncSender<Event>,
    wake: &impl Fn(),
    request: &ScanRequest,
) -> Option<Arc<Vec<Entry>>> {
    let generation = request.generation;
    if !shared.scanning(generation) {
        cancelled(shared, sender, wake, generation);
        return None;
    }
    let mut last_progress = Instant::now();
    let outcome = match scan_path_with(&request.root, |progress| {
        progress_event(
            shared,
            sender,
            wake,
            &mut last_progress,
            generation,
            progress.entries,
            progress.issues,
        );
        shared.scanning(generation)
    }) {
        Ok(outcome) => outcome,
        Err(issue) => {
            if shared.scanning(generation) {
                if !deliver(
                    shared,
                    sender,
                    wake,
                    Event::Failed {
                        scan_generation: generation,
                        message: issue.to_string(),
                    },
                ) {
                    cancelled(shared, sender, wake, generation);
                }
            } else {
                cancelled(shared, sender, wake, generation);
            }
            return None;
        }
    };
    if outcome.cancelled || !shared.scanning(generation) {
        cancelled(shared, sender, wake, generation);
        return None;
    }

    let mut issues = outcome.report.issues.len();
    let mut diagnostics: Vec<_> = outcome
        .report
        .issues
        .iter()
        .take(DIAGNOSTIC_LIMIT)
        .map(ToString::to_string)
        .collect();
    let total_entries = outcome.report.paths.len();
    let mut entries = Vec::with_capacity(total_entries);
    for path in outcome.report.paths {
        if !shared.scanning(generation) {
            cancelled(shared, sender, wake, generation);
            return None;
        }
        let kind = match path_kind(&path) {
            Ok(kind) => kind,
            Err(error) => {
                issues += 1;
                if diagnostics.len() < DIAGNOSTIC_LIMIT {
                    diagnostics.push(format!("read metadata '{}': {error}", path.display()));
                }
                EntryKind::Other
            }
        };
        let name = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned();
        let display_path = path.to_string_lossy().into_owned();
        entries.push(Entry {
            path,
            name,
            display_path,
            kind,
        });
        progress_event(
            shared,
            sender,
            wake,
            &mut last_progress,
            generation,
            total_entries,
            issues,
        );
    }
    if !shared.scanning(generation) {
        cancelled(shared, sender, wake, generation);
        return None;
    }
    // Raw path spelling is the deterministic tie-breaker, including names that
    // happen to produce identical lossy display strings.
    entries.sort_unstable_by(|left, right| left.path.as_os_str().cmp(right.path.as_os_str()));
    if !shared.scanning(generation) {
        cancelled(shared, sender, wake, generation);
        return None;
    }
    if issues > diagnostics.len() {
        diagnostics.push(format!(
            "{} additional issues are omitted",
            issues - diagnostics.len()
        ));
    }
    let entries = Arc::new(entries);
    if deliver(
        shared,
        sender,
        wake,
        Event::Scanned {
            scan_generation: generation,
            entries: Arc::clone(&entries),
            issues,
            diagnostics,
        },
    ) {
        Some(entries)
    } else {
        cancelled(shared, sender, wake, generation);
        None
    }
}

fn path_kind(path: &Path) -> io::Result<EntryKind> {
    // The scanner has already validated the original spelling. A trailing `/`
    // or `/.` can make symlink_metadata inspect a directory link's target;
    // remove those components only for classification, retaining the raw entry.
    let metadata_path: PathBuf = path.components().collect();
    fs::symlink_metadata(metadata_path).map(|metadata| entry_kind(&metadata))
}

fn entry_kind(metadata: &fs::Metadata) -> EntryKind {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return EntryKind::Link;
        }
    }
    let kind = metadata.file_type();
    if kind.is_symlink() {
        EntryKind::Link
    } else if kind.is_dir() {
        EntryKind::Directory
    } else if kind.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

fn collect_matches(
    entries: &[Entry],
    query: &Query,
    current: impl Fn() -> bool,
) -> Option<(Vec<usize>, usize)> {
    let mut indices = Vec::with_capacity(entries.len().min(RESULT_LIMIT));
    let mut total_matches = 0;
    let query_text = OsStr::new(&query.text);
    for (index, entry) in entries.iter().enumerate() {
        if index % 256 == 0 && !current() {
            return None;
        }
        let included = match query.filter {
            EntryFilter::All => true,
            EntryFilter::Files => entry.kind == EntryKind::File,
            EntryFilter::Directories => entry.kind == EntryKind::Directory,
        };
        if included && matches_path(&entry.path, query_text, query.mode) {
            total_matches += 1;
            if indices.len() < RESULT_LIMIT {
                indices.push(index);
            }
        }
    }
    current().then_some((indices, total_matches))
}

fn progress_event(
    shared: &Shared,
    sender: &SyncSender<Event>,
    wake: &impl Fn(),
    last: &mut Instant,
    generation: u64,
    entries: usize,
    issues: usize,
) {
    if last.elapsed() >= PROGRESS_INTERVAL && shared.scanning(generation) {
        *last = Instant::now();
        if sender
            .try_send(Event::Progress {
                scan_generation: generation,
                entries,
                issues,
            })
            .is_ok()
        {
            wake();
        }
    }
}

fn cancelled(shared: &Shared, sender: &SyncSender<Event>, wake: &impl Fn(), generation: u64) {
    deliver(
        shared,
        sender,
        wake,
        Event::Cancelled {
            scan_generation: generation,
        },
    );
}

/// Final events have bounded backpressure. A stale request or shutdown releases
/// the sender even when the UI has stopped draining the event channel.
fn deliver(
    shared: &Shared,
    sender: &SyncSender<Event>,
    wake: &impl Fn(),
    mut event: Event,
) -> bool {
    let mut requested_drain = false;
    loop {
        if !shared.event_is_current(&event) {
            return false;
        }
        match sender.try_send(event) {
            Ok(()) => {
                wake();
                return true;
            }
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(pending)) => {
                event = pending;
                if !requested_drain {
                    wake();
                    requested_drain = true;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::error::Error;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> io::Result<Self> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "fsearch-worker-{}-{timestamp}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0) {
                if error.kind() != io::ErrorKind::NotFound {
                    eprintln!("Cannot remove worker fixture {:?}: {error}", self.0);
                }
            }
        }
    }

    fn query(text: &str) -> Query {
        Query {
            text: text.into(),
            mode: SearchMode::NameSubstring,
            filter: EntryFilter::All,
        }
    }

    fn receive(worker: &Worker) -> Result<Event, Box<dyn Error>> {
        Ok(worker.events.recv_timeout(Duration::from_secs(5))?)
    }

    fn await_scanned(worker: &Worker, generation: u64) -> Result<Arc<Vec<Entry>>, Box<dyn Error>> {
        loop {
            match receive(worker)? {
                Event::Scanned {
                    scan_generation,
                    entries,
                    issues,
                    ..
                } if scan_generation == generation => {
                    assert_eq!(issues, 0);
                    return Ok(entries);
                }
                Event::Progress { .. } => {}
                event => panic!("Unexpected event while awaiting snapshot: {event:?}"),
            }
        }
    }

    fn await_results(
        worker: &Worker,
        scan: u64,
        generation: u64,
    ) -> Result<(Vec<usize>, usize), Box<dyn Error>> {
        loop {
            match receive(worker)? {
                Event::Results {
                    scan_generation,
                    query_generation,
                    indices,
                    total_matches,
                } if scan_generation == scan && query_generation == generation => {
                    return Ok((indices, total_matches))
                }
                Event::Progress { .. } => {}
                event => panic!("Unexpected event while awaiting results: {event:?}"),
            }
        }
    }

    #[test]
    fn queries_reuse_sorted_cache_after_files_are_removed() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let first = directory.0.join("a.txt");
        let second = directory.0.join("z.txt");
        fs::write(&second, [])?;
        fs::write(&first, [])?;
        let worker = Worker::new(|| {})?;
        worker.scan(1, 1, directory.0.clone(), query(""));
        let entries = await_scanned(&worker, 1)?;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, first);
        assert_eq!(entries[1].path, second);
        assert_eq!(await_results(&worker, 1, 1)?, (vec![0, 1], 2));
        fs::remove_file(&first)?;
        fs::remove_file(&second)?;
        worker.query(1, 2, query("z.txt"));
        assert_eq!(await_results(&worker, 1, 2)?, (vec![1], 1));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn linked_directory_roots_keep_link_kind_and_original_spelling() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new()?;
        let target = directory.0.join("target");
        fs::create_dir(&target)?;
        fs::write(target.join("not-traversed.txt"), [])?;
        let link = directory.0.join("link");
        symlink(&target, &link)?;
        let worker = Worker::new(|| {})?;

        for (index, suffix) in ["", "/", "/."].into_iter().enumerate() {
            let mut spelling = link.as_os_str().to_os_string();
            spelling.push(suffix);
            let root = PathBuf::from(spelling);
            let scan_generation = index as u64 + 1;
            let query_generation = scan_generation * 10;
            worker.scan(scan_generation, query_generation, root.clone(), query(""));
            let entries = await_scanned(&worker, scan_generation)?;
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].kind, EntryKind::Link);
            assert_eq!(entries[0].path.as_os_str(), root.as_os_str());
            assert_eq!(entries[0].display_path, root.to_string_lossy());
            assert_eq!(
                await_results(&worker, scan_generation, query_generation)?,
                (vec![0], 1)
            );

            let mut directories = query("");
            directories.filter = EntryFilter::Directories;
            worker.query(scan_generation, query_generation + 1, directories);
            assert_eq!(
                await_results(&worker, scan_generation, query_generation + 1)?,
                (Vec::new(), 0)
            );
        }
        Ok(())
    }

    // Block only the first repaint callback, after a snapshot has been queued.
    // This makes replacements deterministic without relying on filesystem speed.
    fn gated_worker() -> io::Result<(Worker, mpsc::Sender<()>)> {
        let (release, released) = mpsc::channel();
        let released = Mutex::new(Some(released));
        let worker = Worker::new(move || {
            let receiver = released
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some(receiver) = receiver {
                let _ = receiver.recv_timeout(Duration::from_secs(5));
            }
        })?;
        Ok((worker, release))
    }

    #[test]
    fn rapid_queries_replace_pending_query_without_replacing_scan() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(directory.0.join("latest.txt"), [])?;
        let (worker, release) = gated_worker()?;
        worker.scan(1, 1, directory.0.clone(), query(""));
        let entries = await_scanned(&worker, 1)?;
        for generation in 2..1_000 {
            worker.query(1, generation, query("absent"));
        }
        worker.query(1, 1_000, query("latest"));
        assert!(worker.shared.scanning(1));
        release.send(())?;
        assert_eq!(await_results(&worker, 1, 1_000)?, (vec![0], 1));
        assert_eq!(entries[0].name, "latest.txt");
        Ok(())
    }

    #[test]
    fn newer_scope_discards_old_query_and_runs_its_latest_query() -> Result<(), Box<dyn Error>> {
        let first = TestDirectory::new()?;
        let second = TestDirectory::new()?;
        fs::write(first.0.join("old.txt"), [])?;
        fs::write(second.0.join("new.txt"), [])?;
        let (worker, release) = gated_worker()?;
        worker.scan(1, 1, first.0.clone(), query("old"));
        await_scanned(&worker, 1)?;
        worker.scan(2, 2, second.0.clone(), query("absent"));
        worker.query(2, 3, query("new"));
        release.send(())?;
        let entries = await_scanned(&worker, 2)?;
        assert_eq!(entries[0].path, second.0.join("new.txt"));
        assert_eq!(await_results(&worker, 2, 3)?, (vec![0], 1));
        Ok(())
    }

    #[test]
    fn pending_scan_can_be_cancelled_without_publishing_a_snapshot() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(directory.0.join("file.txt"), [])?;
        let (worker, release) = gated_worker()?;
        worker.scan(1, 1, directory.0.clone(), query(""));
        await_scanned(&worker, 1)?;
        worker.scan(2, 2, directory.0.clone(), query(""));
        worker.cancel_scan();
        release.send(())?;
        assert!(matches!(
            receive(&worker)?,
            Event::Cancelled { scan_generation: 2 }
        ));
        Ok(())
    }

    fn entry(index: usize, kind: EntryKind) -> Entry {
        let name = format!("entry-{index:05}.txt");
        Entry {
            path: PathBuf::from(&name),
            display_path: name.clone(),
            name,
            kind,
        }
    }

    #[test]
    fn large_queries_count_all_matches_but_cap_indices() {
        let entries: Vec<_> = (0..RESULT_LIMIT + 123)
            .map(|index| entry(index, EntryKind::File))
            .collect();
        let (indices, total) = collect_matches(&entries, &query("entry"), || true).unwrap();
        assert_eq!(total, RESULT_LIMIT + 123);
        assert_eq!(indices.len(), RESULT_LIMIT);
        assert_eq!(indices, (0..RESULT_LIMIT).collect::<Vec<_>>());
    }

    #[test]
    fn large_queries_observe_cancellation_before_completing() {
        let entries: Vec<_> = (0..4_096)
            .map(|index| entry(index, EntryKind::File))
            .collect();
        let checks = Cell::new(0);
        assert!(collect_matches(&entries, &query(""), || {
            checks.set(checks.get() + 1);
            checks.get() < 3
        })
        .is_none());
        assert_eq!(checks.get(), 3);
    }

    #[test]
    fn filters_keep_links_and_other_entries_only_in_all() {
        let entries = [
            entry(0, EntryKind::File),
            entry(1, EntryKind::Directory),
            entry(2, EntryKind::Link),
            entry(3, EntryKind::Other),
        ];
        let mut request = query("");
        assert_eq!(
            collect_matches(&entries, &request, || true).unwrap(),
            (vec![0, 1, 2, 3], 4)
        );
        request.filter = EntryFilter::Files;
        assert_eq!(
            collect_matches(&entries, &request, || true).unwrap(),
            (vec![0], 1)
        );
        request.filter = EntryFilter::Directories;
        assert_eq!(
            collect_matches(&entries, &request, || true).unwrap(),
            (vec![1], 1)
        );
    }

    #[test]
    fn backpressure_releases_stale_final_event() -> Result<(), Box<dyn Error>> {
        let shared = Arc::new(Shared::new());
        shared.scan_generation.store(1, Ordering::Release);
        shared.query_generation.store(1, Ordering::Release);
        let (sender, _receiver) = mpsc::sync_channel(1);
        sender.send(Event::Progress {
            scan_generation: 1,
            entries: 0,
            issues: 0,
        })?;
        let (started, started_rx) = mpsc::channel();
        let (done, done_rx) = mpsc::channel();
        let background = Arc::clone(&shared);
        let thread = thread::spawn(move || {
            let delivered = deliver(
                &background,
                &sender,
                &|| {
                    let _ = started.send(());
                },
                Event::Results {
                    scan_generation: 1,
                    query_generation: 1,
                    indices: vec![],
                    total_matches: 0,
                },
            );
            let _ = done.send(delivered);
        });
        started_rx.recv_timeout(Duration::from_secs(5))?;
        shared.query_generation.store(2, Ordering::Release);
        assert!(!done_rx.recv_timeout(Duration::from_secs(5))?);
        assert!(thread.join().is_ok());
        Ok(())
    }
}
