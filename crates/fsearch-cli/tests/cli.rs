use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> io::Result<Self> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fsearch-cli-test-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, name: &str) -> io::Result<PathBuf> {
        let path = self.0.join(name);
        fs::write(&path, b"CLI regression fixture")?;
        Ok(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!("Could not remove test directory {:?}: {error}", self.0);
            }
        }
    }
}

fn search(directory: &Path, query: &str, short: bool) -> io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_search"))
        .arg(if short { "-d" } else { "--directory" })
        .arg(directory)
        .arg(if short { "-f" } else { "--filename" })
        .arg(query)
        .output()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_result(output: &Output, path: &Path, present: bool) {
    // Compare native Path Debug, including escaped undecodable bytes. Match
    // only the quoted value so source locations and vector order can vary.
    let expected = format!("{path:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.contains(&expected),
        present,
        "Unexpected presence of {expected} in {stderr}"
    );
}

fn assert_nonempty_output(output: &Output) {
    assert_success(output);
    assert!(!output.stderr.is_empty(), "dbg! results should use stderr");
    let stdout = String::from_utf8_lossy(&output.stdout);
    if cfg!(debug_assertions) {
        assert!(stdout.starts_with("Total time taken: "), "{stdout}");
    } else {
        assert!(stdout.is_empty(), "{stdout}");
    }
}

#[test]
fn required_parameters_are_enforced() -> io::Result<()> {
    for arguments in [
        Vec::<&OsStr>::new(),
        vec![OsStr::new("--directory"), OsStr::new(".")],
        vec![OsStr::new("--filename"), OsStr::new("report")],
        vec![OsStr::new("-d"), OsStr::new(".")],
        vec![OsStr::new("-f"), OsStr::new("report")],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_search"))
            .args(arguments)
            .output()?;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("required"));
        assert!(output.stdout.is_empty());
    }
    Ok(())
}

#[test]
fn long_and_short_parameters_search_a_file_root() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    let file = directory.file("report.txt")?;
    for short in [false, true] {
        let output = search(&file, "report", short)?;
        assert_nonempty_output(&output);
        assert_result(&output, &file, true);
    }
    Ok(())
}

#[test]
fn directory_root_includes_files_and_subdirectories_but_not_itself() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    let file = directory.file("first.txt")?;
    let subdirectory = directory.path().join("nested");
    fs::create_dir(&subdirectory)?;
    let nested_file = subdirectory.join("second.txt");
    fs::write(&nested_file, b"nested fixture")?;

    let output = search(directory.path(), "", false)?;
    assert_nonempty_output(&output);
    for path in [&file, &subdirectory, &nested_file] {
        assert_result(&output, path, true);
    }
    assert_result(&output, directory.path(), false);
    Ok(())
}

#[test]
fn substring_query_can_match_an_ancestor_directory() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    let ancestor = directory.path().join("ancestor-hit");
    fs::create_dir(&ancestor)?;
    let file = ancestor.join("unrelated.txt");
    fs::write(&file, b"fixture")?;

    let output = search(&ancestor, "ancestor-hit", false)?;
    assert_nonempty_output(&output);
    assert_result(&output, &file, true);
    assert_result(&output, &ancestor, false);
    Ok(())
}

#[test]
fn substring_matching_is_case_sensitive() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    let file = directory.file("Report.txt")?;
    let output = search(directory.path(), "Report", false)?;
    assert_nonempty_output(&output);
    assert_result(&output, &file, true);

    let output = search(directory.path(), "report", false)?;
    assert_success(&output);
    assert!(output.stderr.is_empty());
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn substring_matching_requires_contiguous_characters() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    let file = directory.file("report.pdf")?;
    let output = search(directory.path(), "port.p", false)?;
    assert_nonempty_output(&output);
    assert_result(&output, &file, true);

    let output = search(directory.path(), "rptpdf", false)?;
    assert_success(&output);
    assert!(output.stderr.is_empty());
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn unicode_and_spaces_are_preserved() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    let file = directory.file("中文 报告.txt")?;
    let output = search(directory.path(), "中文 报告", true)?;
    assert_nonempty_output(&output);
    assert_result(&output, &file, true);
    Ok(())
}

#[test]
fn empty_directory_has_no_results_or_timing() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    let output = search(directory.path(), "anything", false)?;
    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "File list not found"
    );
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn no_matches_have_no_results_or_timing() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    directory.file("report.txt")?;
    let output = search(directory.path(), "definitely-absent-query", false)?;
    assert_success(&output);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn missing_root_reports_scan_failure_and_exits_with_failure() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    let missing = directory.path().join("does-not-exist");
    let output = search(&missing, "report", false)?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Scan failed:"));
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn relative_root_preserves_relative_result_paths() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    let relative_root = Path::new("nested");
    let absolute_root = directory.path().join(relative_root);
    fs::create_dir(&absolute_root)?;
    let relative_file = relative_root.join("report.txt");
    let absolute_file = directory.path().join(&relative_file);
    fs::write(&absolute_file, b"relative fixture")?;

    let output = Command::new(env!("CARGO_BIN_EXE_search"))
        .current_dir(directory.path())
        .arg("--directory")
        .arg(relative_root)
        .arg("--filename")
        .arg("report")
        .output()?;
    assert_nonempty_output(&output);
    assert_result(&output, &relative_file, true);
    assert_result(&output, &absolute_file, false);
    Ok(())
}

#[cfg(unix)]
fn native_name_fixture_supported(result: io::Result<()>) -> io::Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(error) => {
            #[cfg(target_os = "macos")]
            if error.raw_os_error() == Some(92) {
                // Darwin EILSEQ: this filesystem rejects invalid UTF-8 names.
                eprintln!(
                    "SKIPPED native-filename filesystem assertions: the filesystem \
                     rejected an invalid UTF-8 fixture name: {error}"
                );
                return Ok(false);
            }
            Err(error)
        }
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_root_and_file_names_preserve_native_paths() -> io::Result<()> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let assert_rejected_root = |root: &Path| -> io::Result<()> {
        let output = search(root, "ascii-hit", false)?;
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("Scan failed:"));
        assert!(output.stdout.is_empty());
        Ok(())
    };
    let directory = TestDirectory::new()?;
    let root = directory
        .path()
        .join(OsString::from_vec(b"root-\xff".to_vec()));
    if !native_name_fixture_supported(fs::create_dir(&root))? {
        return assert_rejected_root(&root);
    }
    let file = root.join(OsString::from_vec(b"ascii-hit-\xfe.txt".to_vec()));
    if !native_name_fixture_supported(fs::write(&file, b"native path fixture"))? {
        return assert_rejected_root(&file);
    }

    let output = search(&root, "ascii-hit", false)?;
    assert_nonempty_output(&output);
    assert_result(&output, &file, true);
    assert_result(&output, &root, false);
    Ok(())
}

#[cfg(unix)]
struct PermissionsGuard {
    path: PathBuf,
    original: fs::Permissions,
}

#[cfg(unix)]
impl PermissionsGuard {
    fn deny_access(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;

        let original = fs::metadata(path)?.permissions();
        fs::set_permissions(path, fs::Permissions::from_mode(0o000))?;
        Ok(Self {
            path: path.to_path_buf(),
            original,
        })
    }
}

#[cfg(unix)]
impl Drop for PermissionsGuard {
    fn drop(&mut self) {
        if let Err(error) = fs::set_permissions(&self.path, self.original.clone()) {
            eprintln!("Could not restore permissions for {:?}: {error}", self.path);
        }
    }
}

#[cfg(unix)]
#[test]
fn partial_scan_reports_failure_and_keeps_successful_matches() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    let visible = directory.file("visible-report.txt")?;
    let blocked = directory.path().join("blocked");
    fs::create_dir(&blocked)?;
    let inaccessible_file = blocked.join("visible-hidden.txt");
    fs::write(&inaccessible_file, b"inaccessible fixture")?;
    let _permissions = PermissionsGuard::deny_access(&blocked)?;

    match fs::read_dir(&blocked) {
        Ok(_) => {
            eprintln!(
                "SKIPPED permission-denied assertions: this process can read a mode-000 \
                 directory (for example, when running as root)"
            );
            return Ok(());
        }
        Err(error) => assert_eq!(error.kind(), io::ErrorKind::PermissionDenied),
    }

    let output = search(directory.path(), "visible", false)?;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Scan incomplete:"), "{stderr}");
    assert_result(&output, &visible, true);
    assert_result(&output, &inaccessible_file, false);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if cfg!(debug_assertions) {
        assert!(stdout.starts_with("Total time taken: "), "{stdout}");
    } else {
        assert!(stdout.is_empty(), "{stdout}");
    }
    Ok(())
}
