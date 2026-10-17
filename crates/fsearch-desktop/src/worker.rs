//! Replaceable background builds/queries with bounded UI events.

use fsearch_core::index::{
    build_index, IndexBuildEvent, IndexHealth, IndexLimits, IndexProgress, IndexSnapshot,
    SearchQuery,
};
pub use fsearch_core::index::{EntryFilter, EntryKind};
use fsearch_core::SearchMode;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

const RESULT_LIMIT: usize = 20_000;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

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
        progress: IndexProgress,
    },
    Published {
        scan_generation: u64,
        snapshot: Arc<IndexSnapshot>,
    },
    Finished {
        scan_generation: u64,
        snapshot: Option<Arc<IndexSnapshot>>,
        stale: bool,
        cancelled: bool,
        progress: IndexProgress,
        diagnostics: Vec<String>,
        failure: Option<String>,
    },
    Results {
        scan_generation: u64,
        query_generation: u64,
        snapshot_generation: u64,
        snapshot_revision: u64,
        indices: Vec<usize>,
        total_matches: usize,
    },
}

struct ScanRequest {
    generation: u64,
    root: PathBuf,
}

#[derive(Clone)]
struct QueryRequest {
    scan_generation: u64,
    generation: u64,
    query: Query,
}

#[derive(Default)]
struct Mailbox {
    scan: Option<ScanRequest>,
    query: Option<QueryRequest>,
    retired: Vec<Arc<IndexSnapshot>>,
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
            | Event::Published {
                scan_generation, ..
            } => self.scanning(*scan_generation),
            Event::Finished {
                scan_generation, ..
            } => self.current_scan(*scan_generation),
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
    pub fn new(wake: impl Fn() + Send + Sync + 'static, limits: IndexLimits) -> io::Result<Self> {
        Self::with_limits(wake, limits)
    }

    fn with_limits(
        wake: impl Fn() + Send + Sync + 'static,
        limits: IndexLimits,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared::new());
        let (sender, events) = mpsc::sync_channel(8);
        let background = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("fsearch-background".into())
            .spawn(move || run(background, sender, wake, limits))?;
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

    /// Transfer released snapshots so their final deallocation stays off egui.
    pub fn retire(&self, snapshot: Arc<IndexSnapshot>) {
        let mut mailbox = self.shared.mailbox();
        mailbox.retired.push(snapshot);
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

fn run(shared: Arc<Shared>, sender: SyncSender<Event>, wake: impl Fn(), limits: IndexLimits) {
    let mut cache: Option<Arc<IndexSnapshot>> = None;
    loop {
        let (scan, query, retired) = {
            let mut mailbox = shared.mailbox();
            while mailbox.scan.is_none()
                && mailbox.query.is_none()
                && mailbox.retired.is_empty()
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
            let retired = std::mem::take(&mut mailbox.retired);
            if let Some(scan) = mailbox.scan.take() {
                (Some(scan), None, retired)
            } else {
                (None, mailbox.query.take(), retired)
            }
        };
        drop(retired);
        if let Some(scan) = scan {
            // Scope reuse is deliberately strict: never use lossy spelling or
            // canonicalize through the filesystem on the UI/render thread.
            if cache
                .as_ref()
                .is_some_and(|snapshot| snapshot.root().as_os_str() != scan.root.as_os_str())
            {
                cache = None;
            }
            cache = build_snapshot(&shared, &sender, &wake, &scan, cache, limits);
        } else if let (Some(request), Some(snapshot)) = (query, &cache) {
            send_results(&shared, &sender, &wake, snapshot, &request);
        }
    }
}

fn send_results(
    shared: &Shared,
    sender: &SyncSender<Event>,
    wake: &impl Fn(),
    snapshot: &IndexSnapshot,
    request: &QueryRequest,
) {
    let query = SearchQuery {
        text: request.query.text.clone().into(),
        mode: request.query.mode,
        filter: request.query.filter,
    };
    if let Some(results) = snapshot.query_with(&query, RESULT_LIMIT, || {
        shared.querying(request.scan_generation, request.generation)
    }) {
        deliver(
            shared,
            sender,
            wake,
            Event::Results {
                scan_generation: request.scan_generation,
                query_generation: request.generation,
                snapshot_generation: snapshot.generation(),
                snapshot_revision: snapshot.revision(),
                indices: results.indices,
                total_matches: results.total_matches,
            },
        );
    }
}

fn build_snapshot(
    shared: &Shared,
    sender: &SyncSender<Event>,
    wake: &impl Fn(),
    request: &ScanRequest,
    fallback: Option<Arc<IndexSnapshot>>,
    limits: IndexLimits,
) -> Option<Arc<IndexSnapshot>> {
    let generation = request.generation;
    let mut last_progress = Instant::now();
    let mut last_publish = Instant::now();
    let mut progress = IndexProgress::default();
    let mut preview: Option<Arc<IndexSnapshot>> = None;
    let mut last_query = None;
    let outcome = build_index(&request.root, generation, limits, |event| {
        if !shared.scanning(generation) {
            return false;
        }
        match event {
            IndexBuildEvent::Progress(update) => {
                progress = update;
                if last_progress.elapsed() >= PROGRESS_INTERVAL {
                    last_progress = Instant::now();
                    let retired = std::mem::take(&mut shared.mailbox().retired);
                    drop(retired);
                    if sender
                        .try_send(Event::Progress {
                            scan_generation: generation,
                            progress,
                        })
                        .is_ok()
                    {
                        wake();
                    }
                    // One pending query is serviced against the old snapshot
                    // during a rebuild; this work remains on the background thread.
                    if let Some(snapshot) = fallback.as_ref().or(preview.as_ref()) {
                        let query = shared.mailbox().query.clone();
                        if let Some(query) = query {
                            if last_query != Some(query.generation) {
                                send_results(shared, sender, wake, snapshot, &query);
                                last_query = Some(query.generation);
                            }
                        }
                    }
                }
            }
            IndexBuildEvent::Published(snapshot) => {
                progress = snapshot.progress();
                if fallback.is_none()
                    && (preview.is_none() || last_publish.elapsed() >= PROGRESS_INTERVAL)
                {
                    let snapshot = Arc::new(snapshot);
                    if !deliver(
                        shared,
                        sender,
                        wake,
                        Event::Published {
                            scan_generation: generation,
                            snapshot: Arc::clone(&snapshot),
                        },
                    ) {
                        return false;
                    }
                    preview = Some(Arc::clone(&snapshot));
                    last_publish = Instant::now();
                    // Clone the latest request rather than consuming it: the
                    // terminal revision must also be searched after this batch.
                    let query = shared.mailbox().query.clone();
                    if let Some(query) = query {
                        send_results(shared, sender, wake, &snapshot, &query);
                    }
                }
            }
        }
        shared.scanning(generation)
    });
    let cancelled = !shared.scanning(generation);
    let (snapshot, stale, diagnostics, failure) = match outcome {
        Ok(built) => {
            progress = built.progress();
            let incomplete = built.health() != IndexHealth::Current;
            let mut diagnostics: Vec<_> = built.issues().iter().map(ToString::to_string).collect();
            if progress.issues > diagnostics.len() {
                diagnostics.push(format!(
                    "{} additional scan issues omitted",
                    progress.issues - diagnostics.len()
                ));
            }
            if let Some(reason) = built.stop_reason() {
                diagnostics.push(format!("Index incomplete: {reason}. Limits stop enumeration; match counts cover retained entries only."));
            }
            let retaining_old = incomplete && fallback.is_some();
            let snapshot = if retaining_old {
                fallback
            } else {
                Some(Arc::new(built))
            };
            (snapshot, retaining_old || cancelled, diagnostics, None)
        }
        Err(error) => (
            fallback,
            true,
            vec![error.to_string()],
            Some(error.to_string()),
        ),
    };
    deliver(
        shared,
        sender,
        wake,
        Event::Finished {
            scan_generation: generation,
            snapshot: snapshot.clone(),
            stale,
            cancelled,
            progress,
            diagnostics,
            failure,
        },
    );
    // Keep a valid same-scope snapshot even if delivery was superseded. The
    // next request still validates its original root before any reuse.
    snapshot
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
    use std::error::Error;
    use std::fs;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> io::Result<Self> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fsearch-index-worker-{}-{sequence}",
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
                    eprintln!("Cannot remove worker fixture: {error}");
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
        Ok(worker.events.recv_timeout(Duration::from_secs(10))?)
    }
    fn finished(worker: &Worker, generation: u64) -> Result<Event, Box<dyn Error>> {
        loop {
            let event = receive(worker)?;
            if matches!(&event, Event::Finished { scan_generation, .. } if *scan_generation == generation)
            {
                return Ok(event);
            }
        }
    }
    fn snapshot(worker: &Worker, generation: u64) -> Result<Arc<IndexSnapshot>, Box<dyn Error>> {
        match finished(worker, generation)? {
            Event::Finished {
                snapshot: Some(snapshot),
                stale: false,
                failure: None,
                ..
            } => Ok(snapshot),
            event => panic!("Unexpected completion: {event:?}"),
        }
    }
    fn results(
        worker: &Worker,
        scan: u64,
        generation: u64,
    ) -> Result<(Vec<usize>, usize), Box<dyn Error>> {
        loop {
            if let Event::Results {
                scan_generation,
                query_generation,
                indices,
                total_matches,
                ..
            } = receive(worker)?
            {
                if scan_generation == scan && query_generation == generation {
                    return Ok((indices, total_matches));
                }
            }
        }
    }
    fn gated_worker(limits: IndexLimits) -> io::Result<(Worker, mpsc::Sender<()>)> {
        let (release, released) = mpsc::channel();
        let released = Mutex::new(Some(released));
        let worker = Worker::with_limits(
            move || {
                let receiver = released
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(receiver) = receiver {
                    let _ = receiver.recv_timeout(Duration::from_secs(10));
                }
            },
            limits,
        )?;
        Ok((worker, release))
    }
    fn await_preview(worker: &Worker) -> Result<(), Box<dyn Error>> {
        loop {
            if matches!(receive(worker)?, Event::Published { .. }) {
                return Ok(());
            }
        }
    }

    #[test]
    fn queries_reuse_snapshot_without_disk_and_sort_native_paths() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(directory.0.join("z.txt"), [])?;
        fs::write(directory.0.join("a.txt"), [])?;
        let worker = Worker::new(|| {}, IndexLimits::default())?;
        worker.scan(1, 1, directory.0.clone(), query(""));
        let snapshot = snapshot(&worker, 1)?;
        let (indices, total) = results(&worker, 1, 1)?;
        assert_eq!(total, 2);
        let paths: Vec<_> = indices
            .iter()
            .map(|i| snapshot.get(*i).unwrap().path.as_path())
            .collect();
        assert_eq!(
            paths,
            [directory.0.join("a.txt"), directory.0.join("z.txt")]
        );
        fs::remove_dir_all(&directory.0)?;
        worker.query(1, 2, query("z.txt"));
        let (indices, total) = results(&worker, 1, 2)?;
        assert_eq!(total, 1);
        assert_eq!(
            snapshot.get(indices[0]).unwrap().path.file_name(),
            Some(std::ffi::OsStr::new("z.txt"))
        );
        Ok(())
    }

    #[test]
    fn failed_rebuild_preserves_same_root_and_queries_old_generation() -> Result<(), Box<dyn Error>>
    {
        let directory = TestDirectory::new()?;
        fs::write(directory.0.join("old.txt"), [])?;
        let worker = Worker::new(|| {}, IndexLimits::default())?;
        worker.scan(1, 1, directory.0.clone(), query(""));
        let old = snapshot(&worker, 1)?;
        results(&worker, 1, 1)?;
        fs::remove_dir_all(&directory.0)?;
        worker.scan(2, 2, directory.0.clone(), query("old"));
        match finished(&worker, 2)? {
            Event::Finished {
                snapshot: Some(retained),
                stale: true,
                failure: Some(_),
                ..
            } => assert!(Arc::ptr_eq(&old, &retained)),
            event => panic!("Unexpected rebuild: {event:?}"),
        }
        assert_eq!(results(&worker, 2, 2)?.1, 1);
        worker.query(2, 3, query("old"));
        assert_eq!(results(&worker, 2, 3)?.1, 1);
        Ok(())
    }

    #[test]
    fn failed_new_root_cannot_reuse_previous_scope() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(directory.0.join("old.txt"), [])?;
        let worker = Worker::new(|| {}, IndexLimits::default())?;
        worker.scan(1, 1, directory.0.clone(), query(""));
        snapshot(&worker, 1)?;
        results(&worker, 1, 1)?;
        worker.scan(2, 2, directory.0.join("missing"), query(""));
        assert!(matches!(
            finished(&worker, 2)?,
            Event::Finished {
                snapshot: None,
                failure: Some(_),
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn limited_rebuild_retains_usable_old_snapshot_as_stale() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(directory.0.join("old.txt"), [])?;
        let worker = Worker::with_limits(
            || {},
            IndexLimits {
                max_entries: 2,
                ..IndexLimits::default()
            },
        )?;
        worker.scan(1, 1, directory.0.clone(), query(""));
        let old = snapshot(&worker, 1)?;
        results(&worker, 1, 1)?;
        for name in ["new-a.txt", "new-b.txt"] {
            fs::write(directory.0.join(name), [])?;
        }
        worker.scan(2, 2, directory.0.clone(), query(""));
        match finished(&worker, 2)? {
            Event::Finished {
                snapshot: Some(retained),
                stale: true,
                progress,
                diagnostics,
                ..
            } => {
                assert!(Arc::ptr_eq(&old, &retained));
                assert_eq!(progress.entries, 2);
                assert!(!diagnostics.is_empty());
            }
            event => panic!("Unexpected limited rebuild: {event:?}"),
        }
        assert_eq!(results(&worker, 2, 2)?.1, 1);
        Ok(())
    }

    #[test]
    fn rapid_queries_replace_pending_query_without_cancelling_scan() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(directory.0.join("latest.txt"), [])?;
        let (worker, release) = gated_worker(IndexLimits::default())?;
        worker.scan(1, 1, directory.0.clone(), query(""));
        await_preview(&worker)?;
        for generation in 2..1_000 {
            worker.query(1, generation, query("absent"));
        }
        worker.query(1, 1_000, query("latest"));
        assert!(worker.shared.scanning(1));
        release.send(())?;
        snapshot(&worker, 1)?;
        assert_eq!(results(&worker, 1, 1_000)?.1, 1);
        Ok(())
    }

    #[test]
    fn newer_scope_discards_build_and_runs_latest_query() -> Result<(), Box<dyn Error>> {
        let first = TestDirectory::new()?;
        let second = TestDirectory::new()?;
        fs::write(first.0.join("old.txt"), [])?;
        fs::write(second.0.join("new.txt"), [])?;
        let (worker, release) = gated_worker(IndexLimits::default())?;
        worker.scan(1, 1, first.0.clone(), query("old"));
        await_preview(&worker)?;
        worker.scan(2, 2, second.0.clone(), query("absent"));
        worker.query(2, 3, query("new"));
        release.send(())?;
        let snapshot = snapshot(&worker, 2)?;
        let (indices, total) = results(&worker, 2, 3)?;
        assert_eq!(total, 1);
        assert_eq!(
            snapshot.get(indices[0]).unwrap().path,
            second.0.join("new.txt")
        );
        Ok(())
    }

    #[test]
    fn cancellation_keeps_first_build_partial_and_requeryable() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        for i in 0..30 {
            fs::write(directory.0.join(format!("file-{i}.txt")), [])?;
        }
        let (worker, release) = gated_worker(IndexLimits {
            batch_size: 1,
            ..IndexLimits::default()
        })?;
        worker.scan(1, 1, directory.0.clone(), query(""));
        await_preview(&worker)?;
        worker.cancel_scan();
        release.send(())?;
        let count = match finished(&worker, 1)? {
            Event::Finished {
                snapshot: Some(snapshot),
                stale: true,
                cancelled: true,
                ..
            } => {
                assert_eq!(snapshot.health(), IndexHealth::Partial);
                assert!(snapshot.len() < 30);
                snapshot.len()
            }
            event => panic!("Unexpected cancellation: {event:?}"),
        };
        worker.query(1, 2, query(""));
        assert_eq!(results(&worker, 1, 2)?.1, count);
        Ok(())
    }

    #[test]
    fn query_cancellation_does_not_cancel_build() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(directory.0.join("file.txt"), [])?;
        let (worker, release) = gated_worker(IndexLimits::default())?;
        worker.scan(1, 1, directory.0.clone(), query(""));
        await_preview(&worker)?;
        worker.cancel_query();
        assert!(worker.shared.scanning(1));
        release.send(())?;
        snapshot(&worker, 1)?;
        worker.query(1, 2, query("file"));
        assert_eq!(results(&worker, 1, 2)?.1, 1);
        Ok(())
    }

    #[test]
    fn backpressure_releases_stale_final_event() -> Result<(), Box<dyn Error>> {
        let shared = Arc::new(Shared::new());
        shared.scan_generation.store(1, Ordering::Release);
        shared.query_generation.store(1, Ordering::Release);
        let (sender, _receiver) = mpsc::sync_channel(1);
        sender.send(Event::Progress {
            scan_generation: 1,
            progress: IndexProgress::default(),
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
                    snapshot_generation: 1,
                    snapshot_revision: 1,
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

    #[test]
    fn superseded_terminal_delivery_keeps_fallback_for_next_same_root_rebuild(
    ) -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(directory.0.join("old.txt"), [])?;
        let old = Arc::new(build_index(
            &directory.0,
            1,
            IndexLimits::default(),
            |_| true,
        )?);
        fs::remove_dir_all(&directory.0)?;

        let shared = Arc::new(Shared::new());
        shared.scan_generation.store(2, Ordering::Release);
        let (sender, receiver) = mpsc::sync_channel(1);
        // Hold terminal delivery under backpressure so supersession occurs
        // after the build's current-generation check, without timing sleeps.
        sender.send(Event::Progress {
            scan_generation: 2,
            progress: IndexProgress::default(),
        })?;
        let (blocked, blocked_rx) = mpsc::channel();
        let (done, done_rx) = mpsc::channel();
        let background = Arc::clone(&shared);
        let background_sender = sender.clone();
        let fallback = Arc::clone(&old);
        let root = directory.0.clone();
        let thread = thread::spawn(move || {
            let kept = build_snapshot(
                &background,
                &background_sender,
                &|| {
                    let _ = blocked.send(());
                },
                &ScanRequest {
                    generation: 2,
                    root,
                },
                Some(fallback),
                IndexLimits::default(),
            );
            let _ = done.send(kept);
        });
        blocked_rx.recv_timeout(Duration::from_secs(5))?;
        shared.scan_generation.store(3, Ordering::Release);
        let kept = done_rx.recv_timeout(Duration::from_secs(5))?;
        assert!(thread.join().is_ok());
        assert!(kept.as_ref().is_some_and(|kept| Arc::ptr_eq(kept, &old)));
        receiver.recv_timeout(Duration::from_secs(5))?;

        let retained = build_snapshot(
            &shared,
            &sender,
            &|| {},
            &ScanRequest {
                generation: 3,
                root: directory.0.clone(),
            },
            kept,
            IndexLimits::default(),
        );
        assert!(retained
            .as_ref()
            .is_some_and(|retained| Arc::ptr_eq(retained, &old)));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(5))?,
            Event::Finished {
                scan_generation: 3,
                snapshot: Some(_),
                stale: true,
                failure: Some(_),
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn superseded_current_first_build_remains_same_root_fallback() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let old_path = directory.0.join("old.txt");
        fs::write(&old_path, [])?;
        // A tiny build publishes its terminal Current snapshot before the
        // worker checks whether another scan superseded the request.
        let (worker, release) = gated_worker(IndexLimits::default())?;
        worker.scan(1, 1, directory.0.clone(), query(""));
        let published = loop {
            match receive(&worker)? {
                Event::Published {
                    scan_generation: 1,
                    snapshot,
                } => break snapshot,
                Event::Progress { .. } => {}
                event => panic!("Unexpected event before gated publication: {event:?}"),
            }
        };
        assert_eq!(published.health(), IndexHealth::Current);
        assert_eq!(published.generation(), 1);
        assert_eq!(published.len(), 1);

        worker.scan(2, 2, directory.0.clone(), query("old"));
        fs::remove_dir_all(&directory.0)?;
        release.send(())?;
        match finished(&worker, 2)? {
            Event::Finished {
                snapshot: Some(retained),
                stale: true,
                failure: Some(_),
                ..
            } => {
                assert_eq!(retained.generation(), 1);
                assert_eq!(retained.revision(), published.revision());
                assert_eq!(retained.health(), IndexHealth::Current);
                assert_eq!(retained.root().as_os_str(), directory.0.as_os_str());
                assert_eq!(retained.len(), 1);
                assert_eq!(retained.get(0).map(|entry| &entry.path), Some(&old_path));
            }
            event => panic!("Superseded first build lost its applicable snapshot: {event:?}"),
        }
        assert_eq!(results(&worker, 2, 2)?.1, 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn linked_root_retains_native_spelling_and_kind() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let target = directory.0.join("target");
        fs::create_dir(&target)?;
        fs::write(target.join("unscanned.txt"), [])?;
        let link = directory.0.join("link");
        std::os::unix::fs::symlink(&target, &link)?;
        let worker = Worker::new(|| {}, IndexLimits::default())?;
        for (i, suffix) in ["", "/", "/."].iter().enumerate() {
            let mut raw = link.clone().into_os_string();
            raw.push(suffix);
            let root = PathBuf::from(raw);
            let generation = i as u64 + 1;
            worker.scan(generation, generation, root.clone(), query(""));
            let snapshot = snapshot(&worker, generation)?;
            assert_eq!(snapshot.len(), 1);
            assert_eq!(snapshot.get(0).unwrap().path.as_os_str(), root.as_os_str());
            assert_eq!(snapshot.get(0).unwrap().kind, EntryKind::Link);
            results(&worker, generation, generation)?;
        }
        Ok(())
    }
}
