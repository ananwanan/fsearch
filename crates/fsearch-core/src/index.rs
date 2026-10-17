//! Bounded, queryable native-path snapshots, independent of a frontend.
//!
//! The portable identity is a build generation plus an observation ordinal; it
//! is not a stable filesystem ID. Completed scans are best-effort observations,
//! not atomic filesystem snapshots or promises of continuing freshness.

use crate::scanner::{is_traversable_directory, root_metadata};
use crate::search::matches_path;
use crate::{ScanIssue, ScanOperation, SearchMode};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, Metadata};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const FANOUT: usize = 32;
const FANOUT_BITS: u32 = 5;

/// Identity within one caller-assigned build generation. Never persist this as
/// an inode/file ID, or compare ordinals from different generations as identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EntryId {
    pub generation: u64,
    pub ordinal: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Link,
    Other,
}

impl EntryKind {
    fn from_metadata(metadata: &Metadata) -> Self {
        if metadata.file_type().is_symlink() || is_reparse_point(metadata) {
            Self::Link
        } else if metadata.is_dir() {
            Self::Directory
        } else if metadata.is_file() {
            Self::File
        } else {
            Self::Other
        }
    }
}

fn is_reparse_point(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

#[derive(Debug)]
pub struct IndexedEntry {
    pub id: EntryId,
    pub path: PathBuf,
    pub kind: EntryKind,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EntryFilter {
    #[default]
    All,
    Files,
    Directories,
}

impl EntryFilter {
    fn accepts(self, kind: EntryKind) -> bool {
        match self {
            Self::All => true,
            Self::Files => kind == EntryKind::File,
            Self::Directories => kind == EntryKind::Directory,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchQuery {
    pub text: OsString,
    pub mode: SearchMode,
    pub filter: EntryFilter,
}

#[derive(Debug, Eq, PartialEq)]
pub struct QueryResults {
    /// Observation ordinals, ordered by original native path, then ordinal.
    pub indices: Vec<usize>,
    /// All matches within this snapshot, even when `indices` is capped. A
    /// partial/building snapshot does not describe the whole requested tree.
    pub total_matches: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexHealth {
    Building,
    /// Traversal finished without observed errors. Changes are not monitored.
    Current,
    Partial,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexStopReason {
    Cancelled,
    EntryLimit,
    PathBytesLimit,
    PendingDirectoryLimit,
    PendingPathBytesLimit,
}

impl fmt::Display for IndexStopReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Cancelled => "index build cancelled",
            Self::EntryLimit => "index entry limit reached",
            Self::PathBytesLimit => "index path memory limit reached",
            Self::PendingDirectoryLimit => "pending directory limit reached",
            Self::PendingPathBytesLimit => "pending directory path memory limit reached",
        })
    }
}

/// Application allocation policy, not a process-RSS limit. Entry/path and
/// pending-directory caps are independent; issue details are sampled while the
/// full issue count is retained. OS buffers, allocator overhead, caller-retained
/// snapshots and query result storage are additional. A rebuild retaining an
/// old snapshot may hold both builds' entry/path budgets at once.
#[derive(Clone, Copy, Debug)]
pub struct IndexLimits {
    pub max_entries: usize,
    /// Sum of retained entry PathBuf capacities in native encoded bytes.
    pub max_path_bytes: usize,
    pub max_issues: usize,
    pub max_issue_path_bytes: usize,
    pub max_pending_directories: usize,
    pub max_pending_path_bytes: usize,
    /// Publication batch size; normalized to 1..=16,384 and the entry limit.
    pub batch_size: usize,
}

impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            max_entries: 1_000_000,
            max_path_bytes: 128 * 1024 * 1024,
            max_issues: 64,
            max_issue_path_bytes: 256 * 1024,
            max_pending_directories: 16_384,
            max_pending_path_bytes: 8 * 1024 * 1024,
            batch_size: 2_048,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IndexProgress {
    pub entries: usize,
    /// All observed failures, including details omitted by the sampling budget.
    pub issues: usize,
    pub retained_issues: usize,
    pub path_bytes: usize,
    pub issue_path_bytes: usize,
    pub pending_directories: usize,
    pub pending_path_bytes: usize,
}

/// A persistent fanout-32 tree avoids copying a growing batch-pointer table on
/// every publication. Appending copies at most 32 pointers per tree level;
/// previously published entries and their native path allocations stay shared.
#[derive(Debug)]
enum BatchNode {
    Leaf(Arc<[IndexedEntry]>),
    Branch(Box<[Arc<BatchNode>]>),
}

#[derive(Clone, Debug)]
pub struct IndexSnapshot {
    root: Arc<PathBuf>,
    generation: u64,
    revision: u64,
    health: IndexHealth,
    stop_reason: Option<IndexStopReason>,
    progress: IndexProgress,
    issues: Arc<[Arc<ScanIssue>]>,
    tree: Option<Arc<BatchNode>>,
    depth: u32,
    batch_size: usize,
}

impl IndexSnapshot {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Publication number within the generation, increasing even for a final
    /// health-only publication. Entry ordinals stay valid in later revisions.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn health(&self) -> IndexHealth {
        self.health
    }

    pub fn stop_reason(&self) -> Option<IndexStopReason> {
        self.stop_reason
    }

    pub fn progress(&self) -> IndexProgress {
        self.progress
    }

    pub fn issues(&self) -> &[Arc<ScanIssue>] {
        &self.issues
    }

    pub fn len(&self) -> usize {
        self.progress.entries
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: usize) -> Option<&IndexedEntry> {
        if index >= self.len() {
            return None;
        }
        let batch = index / self.batch_size;
        let mut node = self.tree.as_deref()?;
        let mut depth = self.depth;
        while let BatchNode::Branch(children) = node {
            let child = (batch >> ((depth - 1) * FANOUT_BITS)) & (FANOUT - 1);
            node = children.get(child)?.as_ref();
            depth -= 1;
        }
        match node {
            BatchNode::Leaf(entries) => entries.get(index % self.batch_size),
            BatchNode::Branch(_) => None,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &IndexedEntry> {
        let stack = self.tree.as_deref().into_iter().collect();
        SnapshotIter {
            stack,
            entries: [].iter(),
        }
    }

    /// Search only native paths already in memory. Query cancellation returns
    /// `None`, never a partial match count. Storage is O(result_limit), and the
    /// selected rows are the global first native paths, not the first scan hits.
    pub fn query_with(
        &self,
        query: &SearchQuery,
        result_limit: usize,
        mut keep_going: impl FnMut() -> bool,
    ) -> Option<QueryResults> {
        if !keep_going() {
            return None;
        }
        let mut selected = BinaryHeap::new();
        let mut total_matches = 0;
        for (index, entry) in self.iter().enumerate() {
            if index % 256 == 0 && !keep_going() {
                return None;
            }
            if !query.filter.accepts(entry.kind)
                || !matches_path(&entry.path, &query.text, query.mode)
            {
                continue;
            }
            total_matches += 1;
            let candidate = MatchRef { index, entry };
            if selected.len() < result_limit {
                selected.push(candidate);
            } else if selected.peek().is_some_and(|last| candidate < *last) {
                if let Some(mut last) = selected.peek_mut() {
                    *last = candidate;
                }
            }
        }
        if !keep_going() {
            return None;
        }
        let indices = selected
            .into_sorted_vec()
            .into_iter()
            .map(|item| item.index)
            .collect();
        // Sorting is bounded by the display limit, but a newer request may have
        // arrived while it ran. Never return that superseded result as current.
        if !keep_going() {
            return None;
        }
        Some(QueryResults {
            indices,
            total_matches,
        })
    }

    /// Lower-bound accounted entry structure/path storage, excluding tree
    /// nodes, root, diagnostics, allocator overhead and any other snapshots.
    pub fn entry_storage_bytes(&self) -> usize {
        self.len()
            .saturating_mul(size_of::<IndexedEntry>())
            .saturating_add(self.progress.path_bytes)
    }
}

struct SnapshotIter<'a> {
    stack: Vec<&'a BatchNode>,
    entries: std::slice::Iter<'a, IndexedEntry>,
}

impl<'a> Iterator for SnapshotIter<'a> {
    type Item = &'a IndexedEntry;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(entry) = self.entries.next() {
                return Some(entry);
            }
            match self.stack.pop()? {
                BatchNode::Leaf(entries) => self.entries = entries.iter(),
                BatchNode::Branch(children) => {
                    self.stack.extend(children.iter().rev().map(Arc::as_ref));
                }
            }
        }
    }
}

struct MatchRef<'a> {
    index: usize,
    entry: &'a IndexedEntry,
}

impl PartialEq for MatchRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for MatchRef<'_> {}

impl PartialOrd for MatchRef<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MatchRef<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.entry
            .path
            .as_os_str()
            .cmp(other.entry.path.as_os_str())
            .then_with(|| self.index.cmp(&other.index))
    }
}

#[derive(Debug)]
pub enum IndexBuildEvent {
    Progress(IndexProgress),
    /// Immutable, queryable revision. Building snapshots are always incomplete.
    Published(IndexSnapshot),
}

struct Builder {
    snapshot: IndexSnapshot,
    limits: IndexLimits,
    batch: Vec<IndexedEntry>,
    batches: usize,
    issues: Vec<Arc<ScanIssue>>,
    pending: Vec<PathBuf>,
}

impl Builder {
    fn new(root: &Path, generation: u64, limits: IndexLimits) -> Self {
        let batch_size = limits
            .batch_size
            .clamp(1, 16_384)
            .min(limits.max_entries.max(1));
        Self {
            snapshot: IndexSnapshot {
                root: Arc::new(root.to_path_buf()),
                generation,
                revision: 0,
                health: IndexHealth::Building,
                stop_reason: None,
                progress: IndexProgress::default(),
                issues: Arc::from([]),
                tree: None,
                depth: 0,
                batch_size,
            },
            limits,
            batch: Vec::new(),
            batches: 0,
            issues: Vec::new(),
            pending: Vec::new(),
        }
    }

    fn notify(&self, observer: &mut impl FnMut(IndexBuildEvent) -> bool) -> bool {
        observer(IndexBuildEvent::Progress(self.snapshot.progress))
    }

    fn push_entry(&mut self, path: PathBuf, kind: EntryKind) -> Result<(), IndexStopReason> {
        if self.snapshot.progress.entries >= self.limits.max_entries {
            return Err(IndexStopReason::EntryLimit);
        }
        let path = compact_path(path);
        let bytes = path.capacity();
        if bytes
            > self
                .limits
                .max_path_bytes
                .saturating_sub(self.snapshot.progress.path_bytes)
        {
            return Err(IndexStopReason::PathBytesLimit);
        }
        if self.batch.is_empty() {
            self.batch = Vec::with_capacity(
                self.snapshot
                    .batch_size
                    .min(self.limits.max_entries - self.snapshot.progress.entries),
            );
        }
        self.batch.push(IndexedEntry {
            id: EntryId {
                generation: self.snapshot.generation,
                ordinal: self.snapshot.progress.entries,
            },
            path,
            kind,
        });
        self.snapshot.progress.entries += 1;
        self.snapshot.progress.path_bytes += bytes;
        Ok(())
    }

    fn push_issue(&mut self, mut issue: ScanIssue) {
        self.snapshot.progress.issues += 1;
        if self.issues.len() >= self.limits.max_issues {
            return;
        }
        issue.path = compact_path(issue.path);
        let bytes = issue.path.capacity();
        if bytes
            > self
                .limits
                .max_issue_path_bytes
                .saturating_sub(self.snapshot.progress.issue_path_bytes)
        {
            return;
        }
        self.issues.push(Arc::new(issue));
        self.snapshot.progress.retained_issues += 1;
        self.snapshot.progress.issue_path_bytes += bytes;
    }

    fn push_directory(&mut self, path: PathBuf) -> Result<(), IndexStopReason> {
        if self.pending.len() >= self.limits.max_pending_directories {
            return Err(IndexStopReason::PendingDirectoryLimit);
        }
        let path = compact_path(path);
        let bytes = path.capacity();
        if bytes
            > self
                .limits
                .max_pending_path_bytes
                .saturating_sub(self.snapshot.progress.pending_path_bytes)
        {
            return Err(IndexStopReason::PendingPathBytesLimit);
        }
        // Unlike geometric Vec growth this never reserves beyond the declared
        // frontier count. Entry batches have a separately bounded allocation.
        if self.pending.len() == self.pending.capacity() {
            let available = self.limits.max_pending_directories - self.pending.len();
            self.pending.reserve_exact(available.min(256));
        }
        self.pending.push(path);
        self.snapshot.progress.pending_directories = self.pending.len();
        self.snapshot.progress.pending_path_bytes += bytes;
        Ok(())
    }

    fn pop_directory(&mut self) -> Option<PathBuf> {
        let path = self.pending.pop()?;
        self.snapshot.progress.pending_directories = self.pending.len();
        self.snapshot.progress.pending_path_bytes -= path.capacity();
        Some(path)
    }

    fn flush(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        let leaf = Arc::new(BatchNode::Leaf(Arc::from(std::mem::take(&mut self.batch))));
        match self.snapshot.tree.take() {
            None => self.snapshot.tree = Some(leaf),
            Some(root) => {
                let shift = self.snapshot.depth * FANOUT_BITS;
                if self.batches.checked_shr(shift).unwrap_or(0) != 0 {
                    self.snapshot.depth += 1;
                    let branch = branch_path(self.snapshot.depth - 1, leaf);
                    self.snapshot.tree =
                        Some(Arc::new(BatchNode::Branch(Box::new([root, branch]))));
                } else {
                    self.snapshot.tree =
                        Some(append_leaf(&root, self.snapshot.depth, self.batches, leaf));
                }
            }
        }
        self.batches += 1;
    }

    fn publish(&mut self, observer: &mut impl FnMut(IndexBuildEvent) -> bool) -> bool {
        self.flush();
        self.snapshot.revision += 1;
        self.snapshot.issues = self.issues.clone().into();
        observer(IndexBuildEvent::Published(self.snapshot.clone()))
    }

    fn finish(
        mut self,
        reason: Option<IndexStopReason>,
        observer: &mut impl FnMut(IndexBuildEvent) -> bool,
    ) -> IndexSnapshot {
        self.snapshot.stop_reason = reason;
        self.snapshot.health = if reason.is_some() || self.snapshot.progress.issues > 0 {
            IndexHealth::Partial
        } else {
            IndexHealth::Current
        };
        // The preceding progress check is the cancellation point. A terminal
        // publication is an observation of an already finished build, so its
        // return value cannot revoke the health just delivered to the caller.
        self.publish(observer);
        self.snapshot
    }
}

fn compact_path(path: PathBuf) -> PathBuf {
    path.into_os_string()
        .into_boxed_os_str()
        .into_os_string()
        .into()
}

fn branch_path(depth: u32, leaf: Arc<BatchNode>) -> Arc<BatchNode> {
    if depth == 0 {
        leaf
    } else {
        Arc::new(BatchNode::Branch(Box::new([branch_path(depth - 1, leaf)])))
    }
}

fn append_leaf(
    root: &Arc<BatchNode>,
    depth: u32,
    index: usize,
    leaf: Arc<BatchNode>,
) -> Arc<BatchNode> {
    let BatchNode::Branch(children) = root.as_ref() else {
        return leaf;
    };
    let child = (index >> ((depth - 1) * FANOUT_BITS)) & (FANOUT - 1);
    let mut next = children.to_vec();
    if let Some(existing) = children.get(child) {
        next[child] = append_leaf(existing, depth - 1, index, leaf);
    } else {
        next.push(branch_path(depth - 1, leaf));
    }
    Arc::new(BatchNode::Branch(next.into_boxed_slice()))
}

/// Traverse once into a bounded index without accumulating a `ScanReport` or
/// re-reading metadata to build a desktop cache. Progress callbacks run before
/// I/O and after observations; returning false cooperatively cancels. Blocking
/// OS I/O already in flight cannot be interrupted. The final snapshot is always
/// published (even if empty); returning false from that terminal notification
/// does not revoke the completed outcome.
///
/// Ordinary directory roots are excluded. File/link roots and original path
/// spellings match the compatibility scanner. Root I/O failures return `Err`;
/// child failures are counted and sampled, never reported as a complete index.
/// Limits stop traversal with an explicit partial snapshot, not silent eviction.
pub fn build_index(
    root: impl AsRef<Path>,
    generation: u64,
    limits: IndexLimits,
    mut observer: impl FnMut(IndexBuildEvent) -> bool,
) -> Result<IndexSnapshot, ScanIssue> {
    let root = root.as_ref();
    let mut builder = Builder::new(root, generation, limits);
    if !builder.notify(&mut observer) {
        return Ok(builder.finish(Some(IndexStopReason::Cancelled), &mut observer));
    }
    let metadata = root_metadata(root)?;
    if !is_traversable_directory(&metadata) {
        let mut reason = builder
            .push_entry(root.to_path_buf(), EntryKind::from_metadata(&metadata))
            .err();
        if reason.is_none() && !builder.notify(&mut observer) {
            reason = Some(IndexStopReason::Cancelled);
        }
        return Ok(builder.finish(reason, &mut observer));
    }
    if !builder.notify(&mut observer) {
        return Ok(builder.finish(Some(IndexStopReason::Cancelled), &mut observer));
    }
    let entries = fs::read_dir(root).map_err(|error| ScanIssue {
        path: root.to_path_buf(),
        operation: ScanOperation::ReadDirectory,
        error,
    })?;
    let mut reason = visit_directory(root, entries, &mut builder, &mut observer);
    while reason.is_none() {
        let Some(directory) = builder.pop_directory() else {
            break;
        };
        if !builder.notify(&mut observer) {
            reason = Some(IndexStopReason::Cancelled);
            break;
        }
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if is_traversable_directory(&metadata) => {}
            Ok(_) => continue,
            Err(error) => {
                builder.push_issue(ScanIssue {
                    path: directory,
                    operation: ScanOperation::ReadMetadata,
                    error,
                });
                continue;
            }
        }
        if !builder.notify(&mut observer) {
            reason = Some(IndexStopReason::Cancelled);
            break;
        }
        match fs::read_dir(&directory) {
            Ok(entries) => {
                reason = visit_directory(&directory, entries, &mut builder, &mut observer)
            }
            Err(error) => builder.push_issue(ScanIssue {
                path: directory,
                operation: ScanOperation::ReadDirectory,
                error,
            }),
        }
    }
    if reason.is_none() && !builder.notify(&mut observer) {
        reason = Some(IndexStopReason::Cancelled);
    }
    Ok(builder.finish(reason, &mut observer))
}

fn visit_directory(
    directory: &Path,
    mut entries: fs::ReadDir,
    builder: &mut Builder,
    observer: &mut impl FnMut(IndexBuildEvent) -> bool,
) -> Option<IndexStopReason> {
    loop {
        if !builder.notify(observer) {
            return Some(IndexStopReason::Cancelled);
        }
        let entry = entries.next()?;
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                builder.push_issue(ScanIssue {
                    path: directory.to_path_buf(),
                    operation: ScanOperation::ReadEntry,
                    error,
                });
                continue;
            }
        };
        let path = entry.path();
        // Record an observed entry even if it disappears before metadata. Kind
        // then remains Other and the structured failure marks the index partial.
        let (kind, traversable) = match fs::symlink_metadata(&path) {
            Ok(metadata) => (
                EntryKind::from_metadata(&metadata),
                is_traversable_directory(&metadata),
            ),
            Err(error) => {
                builder.push_issue(ScanIssue {
                    path: path.clone(),
                    operation: ScanOperation::ReadMetadata,
                    error,
                });
                (EntryKind::Other, false)
            }
        };
        let pending = traversable.then(|| path.clone());
        if let Err(reason) = builder.push_entry(path, kind) {
            return Some(reason);
        }
        if let Some(path) = pending {
            if let Err(reason) = builder.push_directory(path) {
                return Some(reason);
            }
        }
        if builder.batch.len() == builder.snapshot.batch_size && !builder.publish(observer) {
            return Some(IndexStopReason::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::error::Error;
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> io::Result<Self> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, AtomicOrdering::Relaxed);
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "fsearch-index-{}-{timestamp}-{sequence}",
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
                    eprintln!("cannot remove index fixture {:?}: {error}", self.0);
                }
            }
        }
    }

    fn query(text: impl Into<OsString>, mode: SearchMode) -> SearchQuery {
        SearchQuery {
            text: text.into(),
            mode,
            filter: EntryFilter::All,
        }
    }

    fn synthetic(paths: &[(&str, EntryKind)]) -> IndexSnapshot {
        let mut builder = Builder::new(Path::new("fixture"), 7, IndexLimits::default());
        for (path, kind) in paths {
            builder.push_entry(PathBuf::from(path), *kind).unwrap();
        }
        builder.finish(None, &mut |_| true)
    }

    #[test]
    fn batches_share_native_entries_and_have_stable_generation_scoped_ids() {
        let limits = IndexLimits {
            batch_size: 1,
            ..IndexLimits::default()
        };
        let mut builder = Builder::new(Path::new("fixture"), 42, limits);
        let mut first = None;
        // Cross both 32-way tree growth boundaries, exercising persistent
        // append, iteration, lookup and old revisions without filesystem I/O.
        for index in 0..1_100 {
            builder
                .push_entry(format!("item-{index:04}").into(), EntryKind::File)
                .unwrap();
            assert!(builder.publish(&mut |event| {
                if first.is_none() {
                    if let IndexBuildEvent::Published(snapshot) = event {
                        first = Some(snapshot);
                    }
                }
                true
            }));
        }
        let snapshot = builder.finish(None, &mut |_| true);
        let first = first.unwrap();
        assert_eq!(first.health(), IndexHealth::Building);
        assert_eq!(first.len(), 1);
        assert_eq!(first.revision(), 1);
        assert_eq!(snapshot.revision(), 1_101);
        assert_eq!(snapshot.health(), IndexHealth::Current);
        assert!(std::ptr::eq(
            first.get(0).unwrap(),
            snapshot.get(0).unwrap()
        ));
        for (index, entry) in snapshot.iter().enumerate() {
            assert_eq!(
                entry.id,
                EntryId {
                    generation: 42,
                    ordinal: index
                }
            );
            assert_eq!(entry.path, PathBuf::from(format!("item-{index:04}")));
            assert!(std::ptr::eq(entry, snapshot.get(index).unwrap()));
        }
        assert_eq!(snapshot.iter().count(), 1_100);
        assert!(snapshot.get(1_100).is_none());
    }

    #[test]
    fn final_short_batch_keeps_ordinal_lookup_correct() {
        let mut builder = Builder::new(
            Path::new("fixture"),
            12,
            IndexLimits {
                batch_size: 3,
                ..IndexLimits::default()
            },
        );
        for index in 0..100 {
            builder
                .push_entry(format!("item-{index}").into(), EntryKind::File)
                .unwrap();
            if builder.batch.len() == 3 {
                builder.publish(&mut |_| true);
            }
        }
        let snapshot = builder.finish(None, &mut |_| true);
        assert_eq!(snapshot.len(), 100);
        assert_eq!(snapshot.iter().count(), 100);
        assert_eq!(snapshot.get(99).unwrap().path, PathBuf::from("item-99"));
        assert!(snapshot.get(100).is_none());
    }

    #[test]
    fn query_modes_remain_literal_and_filter_kinds_without_disk_io() {
        let snapshot = synthetic(&[
            ("reports/report.pdf", EntryKind::File),
            ("reports", EntryKind::Directory),
            ("Report.pdf", EntryKind::Link),
            ("报告-é.txt", EntryKind::Other),
        ]);
        for (text, mode, expected) in [
            ("report.pdf", SearchMode::ExactName, vec![0]),
            ("reports/report.pdf", SearchMode::ExactPath, vec![0]),
            ("port.p", SearchMode::NameSubstring, vec![2, 0]),
            ("reports", SearchMode::PathSubstring, vec![1, 0]),
            ("rptpdf", SearchMode::NameSubstring, vec![]),
            ("报告", SearchMode::NameSubstring, vec![3]),
            ("", SearchMode::ExactName, vec![]),
        ] {
            let result = snapshot
                .query_with(&query(text, mode), 100, || true)
                .unwrap();
            assert_eq!(result.indices, expected);
            assert_eq!(result.total_matches, result.indices.len());
        }
        let mut request = query("", SearchMode::NameSubstring);
        request.filter = EntryFilter::Files;
        assert_eq!(
            snapshot.query_with(&request, 100, || true).unwrap().indices,
            vec![0]
        );
        request.filter = EntryFilter::Directories;
        assert_eq!(
            snapshot.query_with(&request, 100, || true).unwrap().indices,
            vec![1]
        );
    }

    #[test]
    fn capped_queries_select_global_native_order_and_count_every_match() {
        let snapshot = synthetic(&[
            ("z.txt", EntryKind::File),
            ("b.txt", EntryKind::File),
            ("a.txt", EntryKind::File),
            ("a.txt", EntryKind::File),
        ]);
        let request = query("", SearchMode::PathSubstring);
        assert_eq!(
            snapshot.query_with(&request, 2, || true).unwrap(),
            QueryResults {
                indices: vec![2, 3],
                total_matches: 4,
            }
        );
        assert_eq!(
            snapshot.query_with(&request, 0, || true).unwrap(),
            QueryResults {
                indices: vec![],
                total_matches: 4,
            }
        );
    }

    #[test]
    fn query_cancellation_never_returns_a_partial_total() {
        let mut builder = Builder::new(Path::new("fixture"), 1, IndexLimits::default());
        for index in 0..1_024 {
            builder
                .push_entry(format!("item-{index}").into(), EntryKind::File)
                .unwrap();
        }
        let snapshot = builder.finish(None, &mut |_| true);
        let checks = Cell::new(0);
        let result = snapshot.query_with(&query("", SearchMode::PathSubstring), 10, || {
            checks.set(checks.get() + 1);
            checks.get() < 3
        });
        assert!(result.is_none());
        assert_eq!(checks.get(), 3);
    }

    #[test]
    fn cancellation_precedes_root_io_and_publishes_partial_terminal_snapshot(
    ) -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let mut publications = Vec::new();
        let snapshot = build_index(
            directory.0.join("missing"),
            8,
            IndexLimits::default(),
            |event| {
                if let IndexBuildEvent::Published(snapshot) = event {
                    publications.push(snapshot);
                }
                false
            },
        )?;
        assert_eq!(snapshot.health(), IndexHealth::Partial);
        assert_eq!(snapshot.stop_reason(), Some(IndexStopReason::Cancelled));
        assert_eq!(snapshot.len(), 0);
        assert_eq!(publications.len(), 1);
        assert_eq!(publications[0].health(), snapshot.health());
        Ok(())
    }

    #[test]
    fn batch_cancellation_preserves_only_published_prefix_and_marks_it_partial(
    ) -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        for index in 0..8 {
            fs::write(directory.0.join(format!("file-{index}")), [])?;
        }
        let snapshot = build_index(
            &directory.0,
            1,
            IndexLimits {
                batch_size: 2,
                ..IndexLimits::default()
            },
            |event| !matches!(event, IndexBuildEvent::Published(ref snapshot) if snapshot.health() == IndexHealth::Building),
        )?;
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot.stop_reason(), Some(IndexStopReason::Cancelled));
        assert_eq!(snapshot.health(), IndexHealth::Partial);
        Ok(())
    }

    #[test]
    fn entry_limit_does_not_mislabel_a_truncated_index_as_current() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        for name in ["a", "b", "c"] {
            fs::write(directory.0.join(name), [])?;
        }
        let snapshot = build_index(
            &directory.0,
            1,
            IndexLimits {
                max_entries: 2,
                ..IndexLimits::default()
            },
            |_| true,
        )?;
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot.health(), IndexHealth::Partial);
        assert_eq!(snapshot.stop_reason(), Some(IndexStopReason::EntryLimit));
        let complete = build_index(
            &directory.0,
            2,
            IndexLimits {
                max_entries: 3,
                ..IndexLimits::default()
            },
            |_| true,
        )?;
        assert_eq!(complete.len(), 3);
        assert_eq!(complete.health(), IndexHealth::Current);
        Ok(())
    }

    #[test]
    fn path_and_frontier_budgets_reject_before_retaining_excess() {
        let limits = IndexLimits {
            max_path_bytes: 3,
            max_pending_directories: 1,
            max_pending_path_bytes: 3,
            ..IndexLimits::default()
        };
        let mut builder = Builder::new(Path::new("fixture"), 1, limits);
        assert_eq!(
            builder.push_entry("four".into(), EntryKind::File),
            Err(IndexStopReason::PathBytesLimit)
        );
        assert_eq!(builder.snapshot.progress.entries, 0);
        assert_eq!(builder.snapshot.progress.path_bytes, 0);
        assert_eq!(
            builder.push_directory("four".into()),
            Err(IndexStopReason::PendingPathBytesLimit)
        );
        assert!(builder.pending.is_empty());
        builder.push_directory("one".into()).unwrap();
        assert_eq!(
            builder.push_directory("two".into()),
            Err(IndexStopReason::PendingDirectoryLimit)
        );
        assert_eq!(builder.snapshot.progress.pending_path_bytes, 3);
        builder.pop_directory();
        assert_eq!(builder.snapshot.progress.pending_path_bytes, 0);
    }

    #[test]
    fn issue_sampling_keeps_full_error_count_and_partial_health() {
        let mut builder = Builder::new(
            Path::new("fixture"),
            1,
            IndexLimits {
                max_issues: 1,
                max_issue_path_bytes: 5,
                ..IndexLimits::default()
            },
        );
        for path in ["longer-than-budget", "short", "later"] {
            builder.push_issue(ScanIssue {
                path: path.into(),
                operation: ScanOperation::ReadMetadata,
                error: io::Error::new(io::ErrorKind::NotFound, "removed"),
            });
        }
        let snapshot = builder.finish(None, &mut |_| true);
        assert_eq!(snapshot.progress().issues, 3);
        assert_eq!(snapshot.progress().retained_issues, 1);
        assert_eq!(snapshot.progress().issue_path_bytes, 5);
        assert_eq!(snapshot.issues()[0].path, PathBuf::from("short"));
        assert_eq!(snapshot.health(), IndexHealth::Partial);
        assert_eq!(snapshot.stop_reason(), None);
    }

    #[test]
    fn concurrent_child_removal_is_counted_without_discarding_observed_paths(
    ) -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let child = directory.0.join("child");
        fs::create_dir(&child)?;
        fs::write(child.join("unobserved"), [])?;
        let mut removed = false;
        let mut removal = None;
        let snapshot = build_index(&directory.0, 1, IndexLimits::default(), |event| {
            if let IndexBuildEvent::Progress(progress) = event {
                if progress.entries == 1 && progress.pending_directories == 1 && !removed {
                    removal = Some(fs::remove_dir_all(&child));
                    removed = true;
                }
            }
            true
        })?;
        removal.ok_or_else(|| io::Error::other("expected queued directory progress"))??;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.get(0).unwrap().path, child);
        assert_eq!(snapshot.progress().issues, 1);
        assert_eq!(snapshot.health(), IndexHealth::Partial);
        Ok(())
    }

    #[test]
    fn file_root_and_deleted_tree_queries_keep_native_paths_without_rescanning(
    ) -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let file = directory.0.join("报告-é.txt");
        fs::write(&file, [])?;
        let snapshot = build_index(&file, 22, IndexLimits::default(), |_| true)?;
        fs::remove_file(&file)?;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.get(0).unwrap().kind, EntryKind::File);
        assert_eq!(snapshot.get(0).unwrap().path, file);
        let matches = snapshot
            .query_with(&query("报告", SearchMode::NameSubstring), 10, || true)
            .unwrap();
        assert_eq!(matches.indices, vec![0]);
        Ok(())
    }

    #[test]
    fn hard_links_remain_distinct_names_with_generation_scoped_identity(
    ) -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let file = directory.0.join("a.txt");
        let alias = directory.0.join("b.txt");
        fs::write(&file, [])?;
        fs::hard_link(&file, &alias)?;
        let first = build_index(&directory.0, 10, IndexLimits::default(), |_| true)?;
        let second = build_index(&directory.0, 11, IndexLimits::default(), |_| true)?;
        assert_eq!(first.len(), 2);
        assert_ne!(first.get(0).unwrap().id, first.get(1).unwrap().id);
        assert_ne!(first.get(0).unwrap().id, second.get(0).unwrap().id);
        let matches = first
            .query_with(&query(".txt", SearchMode::NameSubstring), 10, || true)
            .unwrap();
        assert_eq!(matches.total_matches, 2);
        let paths: Vec<_> = matches
            .indices
            .iter()
            .map(|&index| &first.get(index).unwrap().path)
            .collect();
        assert_eq!(paths, vec![&file, &alias]);
        Ok(())
    }

    #[test]
    fn empty_directory_publishes_one_complete_empty_revision() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let mut publications = 0;
        let snapshot = build_index(&directory.0, 1, IndexLimits::default(), |event| {
            if let IndexBuildEvent::Published(_) = event {
                publications += 1;
            }
            true
        })?;
        assert!(snapshot.is_empty());
        assert_eq!(snapshot.health(), IndexHealth::Current);
        assert_eq!(snapshot.revision(), 1);
        assert_eq!(publications, 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn link_roots_are_not_traversed_even_with_trailing_components() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;
        let directory = TestDirectory::new()?;
        let target = directory.0.join("target");
        fs::create_dir(&target)?;
        fs::write(target.join("hidden"), [])?;
        let link = directory.0.join("link");
        symlink(&target, &link)?;
        for suffix in ["", "/", "/."] {
            let mut native = link.as_os_str().to_os_string();
            native.push(suffix);
            let root = PathBuf::from(native);
            let snapshot = build_index(&root, 1, IndexLimits::default(), |_| true)?;
            assert_eq!(snapshot.len(), 1);
            assert_eq!(snapshot.get(0).unwrap().kind, EntryKind::Link);
            assert_eq!(snapshot.get(0).unwrap().path.as_os_str(), root.as_os_str());
        }
        Ok(())
    }

    #[test]
    fn malformed_unicode_remains_native_in_index_identity_and_matching() {
        #[cfg(unix)]
        let native = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(b"name-\xff.txt".to_vec())
        };
        #[cfg(windows)]
        let native = {
            use std::os::windows::ffi::OsStringExt;
            OsString::from_wide(&[0x006E, 0xD800, 0x002E, 0x0074])
        };
        #[cfg(not(any(unix, windows)))]
        let native = OsString::from("native");
        let mut builder = Builder::new(Path::new("fixture"), 9, IndexLimits::default());
        builder
            .push_entry(PathBuf::from(&native), EntryKind::File)
            .unwrap();
        let snapshot = builder.finish(None, &mut |_| true);
        assert_eq!(snapshot.get(0).unwrap().path.as_os_str(), native);
        assert_eq!(
            snapshot
                .query_with(&query(native, SearchMode::ExactPath), 1, || true)
                .unwrap()
                .total_matches,
            1
        );
    }
}
