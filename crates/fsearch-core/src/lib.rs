//! File-system scanning and literal path matching, independent of any UI or CLI.
//!
//! Paths remain in their native representation. Matching is case-sensitive and
//! does not canonicalize paths or normalize Unicode.

pub mod index;
pub mod scanner;
pub mod search;

pub use index::{
    build_index, EntryFilter, EntryId, EntryKind, IndexBuildEvent, IndexHealth, IndexLimits,
    IndexProgress, IndexSnapshot, IndexStopReason, IndexedEntry, QueryResults, SearchQuery,
};

pub use scanner::{
    scan_path, scan_path_with, ScanIssue, ScanOperation, ScanOutcome, ScanProgress, ScanReport,
};
pub use search::{find_paths, SearchMode};
