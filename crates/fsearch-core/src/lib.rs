//! File-system scanning and literal path matching, independent of any UI or CLI.
//!
//! Paths remain in their native representation. Matching is case-sensitive and
//! does not canonicalize paths or normalize Unicode.

pub mod scanner;
pub mod search;

pub use scanner::{
    scan_path, scan_path_with, ScanIssue, ScanOperation, ScanOutcome, ScanProgress, ScanReport,
};
pub use search::{find_paths, SearchMode};
