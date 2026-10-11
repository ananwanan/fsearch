//! Iterative file-system enumeration that preserves native paths and scan errors.

use std::error::Error;
use std::fmt;
use std::fs::{self, DirEntry, Metadata};
use std::io;
use std::path::{Path, PathBuf};

/// A directory scan's observed paths and recoverable child-entry failures.
#[derive(Debug, Default)]
pub struct ScanReport {
    /// Observed files, directories, and links, excluding an ordinary directory root.
    pub paths: Vec<PathBuf>,
    /// Failures below the root; successfully observed paths remain available.
    pub issues: Vec<ScanIssue>,
}

/// The file-system operation that failed while scanning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanOperation {
    /// Read an entry's metadata without following symbolic links.
    ReadMetadata,
    /// Open a directory for enumeration.
    ReadDirectory,
    /// Read the next entry from an open directory.
    ReadEntry,
}

impl fmt::Display for ScanOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ReadMetadata => "read metadata",
            Self::ReadDirectory => "read directory",
            Self::ReadEntry => "read directory entry",
        })
    }
}

/// A scan failure with its original path, operation, and underlying I/O error.
#[derive(Debug)]
pub struct ScanIssue {
    /// Failed entry, or its parent directory if enumeration supplied no entry path.
    pub path: PathBuf,
    /// Operation that produced the error.
    pub operation: ScanOperation,
    /// Original I/O error, preserving its kind and platform error code.
    pub error: io::Error,
}

impl fmt::Display for ScanIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} '{}': {}",
            self.operation,
            self.path.display(),
            self.error
        )
    }
}

impl Error for ScanIssue {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.error)
    }
}

/// Enumerate a path without descending into a linked final root or observed child link.
///
/// An existing non-directory root (including a link) is the sole candidate. An
/// ordinary directory root is excluded; its descendant files, directories, and
/// links are included. Relative input stays relative. Enumeration order follows
/// the file system and is not a search-result ordering guarantee.
///
/// The final root component is inspected after removing redundant separators and
/// trailing `.` components from the metadata-check path only; `..` is not resolved
/// and paths are not canonicalized. Explicit root ancestors are still resolved by
/// the OS, including any ancestor links. Metadata checks and directory reads are
/// separate operations, so this is not an atomic no-follow guarantee.
///
/// Root metadata and directory-open failures return `Err`. Failures encountered
/// below the root are accumulated in [`ScanReport::issues`] while other entries
/// continue to be scanned. An observed entry remains in `paths` even if it
/// disappears before its metadata is read. The report is not a file-system
/// snapshot, and all paths are retained in memory.
pub fn scan_path(root: impl AsRef<Path>) -> Result<ScanReport, ScanIssue> {
    let root = root.as_ref();
    // Validate the caller's spelling using the OS's path rules first.
    let metadata = fs::symlink_metadata(root).map_err(|error| ScanIssue {
        path: root.to_path_buf(),
        operation: ScanOperation::ReadMetadata,
        error,
    })?;
    // A trailing separator or `.` would make the OS traverse a final directory
    // link during that validation. Inspect the final component separately before
    // deciding to enumerate: candidates and errors keep the caller's spelling.
    let metadata_path: PathBuf = root.components().collect();
    let metadata = if metadata_path.as_os_str() == root.as_os_str() {
        metadata
    } else {
        fs::symlink_metadata(&metadata_path).map_err(|error| ScanIssue {
            path: root.to_path_buf(),
            operation: ScanOperation::ReadMetadata,
            error,
        })?
    };

    if !is_traversable_directory(&metadata) {
        return Ok(ScanReport {
            paths: vec![root.to_path_buf()],
            issues: Vec::new(),
        });
    }

    let entries = fs::read_dir(root).map_err(|error| ScanIssue {
        path: root.to_path_buf(),
        operation: ScanOperation::ReadDirectory,
        error,
    })?;
    let mut report = ScanReport::default();
    let mut pending_directories = Vec::new();
    record_entries(root, entries, &mut report, &mut pending_directories);
    scan_pending_directories(&mut report, &mut pending_directories);

    Ok(report)
}

fn scan_pending_directories(report: &mut ScanReport, pending_directories: &mut Vec<PathBuf>) {
    while let Some(directory) = pending_directories.pop() {
        // A queued directory can disappear or become a link while other entries
        // are scanned. Recheck before opening it; path-based std operations still
        // do not provide an atomic file-system snapshot.
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if is_traversable_directory(&metadata) => {}
            Ok(_) => continue,
            Err(error) => {
                report.issues.push(ScanIssue {
                    path: directory,
                    operation: ScanOperation::ReadMetadata,
                    error,
                });
                continue;
            }
        }
        match fs::read_dir(&directory) {
            Ok(entries) => {
                record_entries(&directory, entries, report, pending_directories);
            }
            Err(error) => report.issues.push(ScanIssue {
                path: directory,
                operation: ScanOperation::ReadDirectory,
                error,
            }),
        }
    }
}

fn record_entries(
    directory: &Path,
    entries: impl Iterator<Item = io::Result<DirEntry>>,
    report: &mut ScanReport,
    pending_directories: &mut Vec<PathBuf>,
) {
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.issues.push(ScanIssue {
                    path: directory.to_path_buf(),
                    operation: ScanOperation::ReadEntry,
                    error,
                });
                continue;
            }
        };
        let path = entry.path();
        report.paths.push(path.clone());

        match fs::symlink_metadata(&path) {
            Ok(metadata) if is_traversable_directory(&metadata) => {
                pending_directories.push(path);
            }
            Ok(_) => {}
            Err(error) => report.issues.push(ScanIssue {
                path,
                operation: ScanOperation::ReadMetadata,
                error,
            }),
        }
    }
}

fn is_traversable_directory(metadata: &Metadata) -> bool {
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return false;
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    }
    #[cfg(not(windows))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> io::Result<Self> {
            Self::new_in(&std::env::temp_dir())
        }

        fn new_in(parent: &Path) -> io::Result<Self> {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos();
            loop {
                let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
                let path = parent.join(format!(
                    "fsearch-scanner-{}-{timestamp}-{sequence}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Ok(Self(path)),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0) {
                eprintln!(
                    "cannot remove test directory '{}': {error}",
                    self.0.display()
                );
            }
        }
    }

    fn with_trailing_separator(path: &Path) -> PathBuf {
        let mut spelling = path.as_os_str().to_os_string();
        spelling.push(std::path::MAIN_SEPARATOR_STR);
        PathBuf::from(spelling)
    }

    #[test]
    fn empty_directory_is_a_successful_empty_scan() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let report = scan_path(directory.path())?;
        assert!(report.paths.is_empty());
        assert!(report.issues.is_empty());
        Ok(())
    }

    #[test]
    fn empty_root_is_not_treated_as_the_current_directory() {
        let issue = scan_path("").unwrap_err();
        assert_eq!(issue.path.as_os_str(), Path::new("").as_os_str());
        assert_eq!(issue.operation, ScanOperation::ReadMetadata);
        assert_eq!(issue.error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn file_root_is_the_only_candidate() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let file = directory.path().join("report.txt");
        fs::write(&file, b"fixture")?;
        let report = scan_path(&file)?;
        assert_eq!(report.paths, vec![file]);
        assert!(report.issues.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn file_root_with_directory_suffix_is_rejected() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let file = directory.path().join("report.txt");
        fs::write(&file, [])?;
        for input in [with_trailing_separator(&file), file.join(".")] {
            let issue = scan_path(&input).unwrap_err();
            assert_eq!(issue.path.as_os_str(), input.as_os_str());
            assert_eq!(issue.operation, ScanOperation::ReadMetadata);
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn file_root_with_directory_suffix_preserves_the_windows_path() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let file = directory.path().join("report.txt");
        fs::write(&file, [])?;
        for input in [with_trailing_separator(&file), file.join(".")] {
            let report = scan_path(&input)?;
            assert_eq!(report.paths.len(), 1);
            assert_eq!(report.paths[0].as_os_str(), input.as_os_str());
            assert!(report.issues.is_empty());
        }
        Ok(())
    }

    #[test]
    fn nonexistent_root_reports_the_original_error() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let missing = directory.path().join("missing");
        let issue = scan_path(&missing).unwrap_err();
        assert_eq!(issue.path, missing);
        assert_eq!(issue.operation, ScanOperation::ReadMetadata);
        assert_eq!(issue.error.kind(), io::ErrorKind::NotFound);
        assert!(issue.source().is_some());
        assert!(issue.to_string().contains("read metadata"));
        Ok(())
    }

    #[test]
    fn nested_directories_and_unicode_paths_are_included() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let child = directory.path().join("报告 🌏");
        let nested = child.join("nested");
        let file = nested.join("with spaces.txt");
        fs::create_dir_all(&nested)?;
        fs::write(&file, [])?;
        let report = scan_path(directory.path())?;
        let paths: HashSet<_> = report.paths.into_iter().collect();
        assert_eq!(paths, HashSet::from([child, nested, file]));
        assert!(report.issues.is_empty());
        Ok(())
    }

    #[test]
    fn relative_root_preserves_relative_paths() -> Result<(), Box<dyn Error>> {
        let current_directory = std::env::current_dir()?;
        let directory = TestDirectory::new_in(&current_directory)?;
        fs::write(directory.path().join("relative.txt"), [])?;
        let relative_root = directory.path().strip_prefix(&current_directory)?;
        let report = scan_path(relative_root)?;
        assert_eq!(report.paths, vec![relative_root.join("relative.txt")]);
        assert!(!report.paths[0].is_absolute());
        assert!(report.issues.is_empty());
        Ok(())
    }

    #[test]
    fn deep_directories_use_iterative_enumeration() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let mut nested = directory.path().to_path_buf();
        // Keep the fixture within ordinary Windows path-length limits.
        for _ in 0..48 {
            nested.push("d");
            fs::create_dir(&nested)?;
        }
        let file = nested.join("leaf");
        fs::write(&file, [])?;
        let report = scan_path(directory.path())?;
        assert_eq!(report.paths.len(), 49);
        assert!(report.paths.contains(&file));
        assert!(report.issues.is_empty());
        Ok(())
    }

    #[test]
    fn entry_read_errors_are_retained_and_other_entries_continue() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let file = directory.path().join("visible.txt");
        fs::write(&file, [])?;
        let entry = fs::read_dir(directory.path())?.next().transpose()?.unwrap();
        let mut report = ScanReport::default();
        let mut pending = Vec::new();
        record_entries(
            directory.path(),
            [
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "fixture")),
                Ok(entry),
            ]
            .into_iter(),
            &mut report,
            &mut pending,
        );
        assert_eq!(report.paths, vec![file]);
        assert!(pending.is_empty());
        assert_eq!(report.issues.len(), 1);
        let issue = &report.issues[0];
        assert_eq!(issue.path, directory.path());
        assert_eq!(issue.operation, ScanOperation::ReadEntry);
        assert_eq!(issue.error.kind(), io::ErrorKind::PermissionDenied);
        Ok(())
    }

    #[test]
    fn entry_deleted_before_metadata_read_keeps_the_observed_path() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let file = directory.path().join("removed.txt");
        fs::write(&file, [])?;
        let entry = fs::read_dir(directory.path())?.next().transpose()?.unwrap();
        fs::remove_file(&file)?;
        let mut report = ScanReport::default();
        let mut pending = Vec::new();
        record_entries(
            directory.path(),
            std::iter::once(Ok(entry)),
            &mut report,
            &mut pending,
        );
        assert_eq!(report.paths, vec![file.clone()]);
        assert!(pending.is_empty());
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].path, file);
        assert_eq!(report.issues[0].operation, ScanOperation::ReadMetadata);
        assert_eq!(report.issues[0].error.kind(), io::ErrorKind::NotFound);
        Ok(())
    }

    #[test]
    fn queued_directory_deleted_before_open_keeps_partial_results() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let child = directory.path().join("removed-directory");
        let file = directory.path().join("retained.txt");
        fs::create_dir(&child)?;
        fs::write(&file, [])?;
        let mut report = ScanReport::default();
        let mut pending = Vec::new();
        record_entries(
            directory.path(),
            fs::read_dir(directory.path())?,
            &mut report,
            &mut pending,
        );
        fs::remove_dir(&child)?;
        scan_pending_directories(&mut report, &mut pending);
        assert_eq!(report.paths.len(), 2);
        assert!(report.paths.contains(&child));
        assert!(report.paths.contains(&file));
        assert!(pending.is_empty());
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].path, child);
        assert_eq!(report.issues[0].operation, ScanOperation::ReadMetadata);
        assert_eq!(report.issues[0].error.kind(), io::ErrorKind::NotFound);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn queued_directory_replaced_by_a_link_is_not_followed() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new()?;
        let child = directory.path().join("changed");
        fs::create_dir(&child)?;
        let mut report = ScanReport::default();
        let mut pending = Vec::new();
        record_entries(
            directory.path(),
            fs::read_dir(directory.path())?,
            &mut report,
            &mut pending,
        );
        fs::remove_dir(&child)?;
        symlink(directory.path(), &child)?;
        scan_pending_directories(&mut report, &mut pending);
        assert_eq!(report.paths, vec![child]);
        assert!(report.issues.is_empty());
        assert!(pending.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_link_roots_and_cycles_are_not_followed() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new()?;
        let child = directory.path().join("child");
        fs::create_dir(&child)?;
        let cycle = child.join("cycle");
        let broken = directory.path().join("broken");
        symlink(directory.path(), &cycle)?;
        symlink(directory.path().join("missing"), &broken)?;

        let report = scan_path(directory.path())?;
        assert_eq!(report.paths.len(), 3);
        assert!(report.paths.contains(&child));
        assert!(report.paths.contains(&cycle));
        assert!(report.paths.contains(&broken));
        assert!(report.issues.is_empty());
        for input in [
            cycle.clone(),
            with_trailing_separator(&cycle),
            cycle.join("."),
            broken.clone(),
        ] {
            let report = scan_path(&input)?;
            assert_eq!(report.paths.len(), 1);
            assert_eq!(report.paths[0].as_os_str(), input.as_os_str());
            assert!(report.issues.is_empty());
        }
        // A broken link is observable on its own, but directory suffixes require
        // a target directory and therefore remain invalid file-system paths.
        for input in [with_trailing_separator(&broken), broken.join(".")] {
            let issue = scan_path(&input).unwrap_err();
            assert_eq!(issue.path.as_os_str(), input.as_os_str());
            assert_eq!(issue.operation, ScanOperation::ReadMetadata);
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_remain_native_paths() -> Result<(), Box<dyn Error>> {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let directory = TestDirectory::new()?;
        let file = directory
            .path()
            .join(OsString::from_vec(b"native-\xff.txt".to_vec()));
        fs::write(&file, [])?;
        let report = scan_path(directory.path())?;
        assert_eq!(report.paths, vec![file]);
        assert!(report.paths[0].to_str().is_none());
        assert!(report.issues.is_empty());
        Ok(())
    }

    #[cfg(windows)]
    fn create_junction(link: &Path, target: &Path) -> Result<(), Box<dyn Error>> {
        use std::process::Command;

        let script_directory = TestDirectory::new()?;
        let script = script_directory.path().join("junction-fixture.ps1");
        fs::write(
            &script,
            "param([string]$Link, [string]$Target)\n$ErrorActionPreference = 'Stop'\nNew-Item -ItemType Junction -Path $Link -Target $Target | Out-Null\n",
        )?;
        // Paths are passed as separate process arguments, never shell command text.
        let output = Command::new("powershell")
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File"])
            .arg(&script)
            .arg("-Link")
            .arg(link)
            .arg("-Target")
            .arg(target)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "cannot create junction fixture: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
            .into());
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn directory_junctions_are_candidates_and_not_followed() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let target = directory.path().join("target");
        fs::create_dir(&target)?;
        let file = target.join("leaf.txt");
        fs::write(&file, [])?;
        let junction = directory.path().join("junction");
        create_junction(&junction, &target)?;

        let report = scan_path(directory.path())?;
        let paths: HashSet<_> = report.paths.into_iter().collect();
        assert_eq!(paths, HashSet::from([target, file, junction.clone()]));
        assert!(report.issues.is_empty());
        for input in [
            junction.clone(),
            with_trailing_separator(&junction),
            junction.join("."),
        ] {
            let report = scan_path(&input)?;
            assert_eq!(report.paths.len(), 1);
            assert_eq!(report.paths[0].as_os_str(), input.as_os_str());
            assert!(report.issues.is_empty());
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn queued_directory_replaced_by_a_junction_is_not_followed() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let child = directory.path().join("changed");
        fs::create_dir(&child)?;
        let mut report = ScanReport::default();
        let mut pending = Vec::new();
        record_entries(
            directory.path(),
            fs::read_dir(directory.path())?,
            &mut report,
            &mut pending,
        );
        fs::remove_dir(&child)?;
        create_junction(&child, directory.path())?;
        scan_pending_directories(&mut report, &mut pending);
        assert_eq!(report.paths, vec![child]);
        assert!(report.issues.is_empty());
        assert!(pending.is_empty());
        Ok(())
    }
}
