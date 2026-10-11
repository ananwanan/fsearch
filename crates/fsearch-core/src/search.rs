use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Literal, case-sensitive matching modes; no mode performs fuzzy matching.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchMode {
    /// Compare only the final file-name component for equality.
    ExactName,
    /// Compare the entire native path spelling, without canonicalization.
    ExactPath,
    /// Find a contiguous substring within the final file-name component.
    NameSubstring,
    /// Find a contiguous substring within the entire path (the CLI default).
    PathSubstring,
}

/// Return borrowed matching paths in their original input order.
///
/// Native path data is used directly, including non-UTF-8 names. Empty substring
/// queries match every applicable candidate; an empty exact query only matches
/// an empty candidate. The caller retains ownership of the scan's paths.
pub fn find_paths<'a>(paths: &'a [PathBuf], query: &OsStr, mode: SearchMode) -> Vec<&'a Path> {
    paths
        .iter()
        .filter(|path| matches_path(path, query, mode))
        .map(PathBuf::as_path)
        .collect()
}

/// Match one native path without allocating a result vector or touching disk.
pub fn matches_path(path: &Path, query: &OsStr, mode: SearchMode) -> bool {
    match mode {
        SearchMode::ExactName => path.file_name() == Some(query),
        SearchMode::ExactPath => path.as_os_str() == query,
        SearchMode::NameSubstring => path
            .file_name()
            .is_some_and(|name| contains_native(name, query)),
        SearchMode::PathSubstring => contains_native(path.as_os_str(), query),
    }
}

fn contains_native(candidate: &OsStr, query: &OsStr) -> bool {
    let query = query.as_encoded_bytes();
    query.is_empty()
        || candidate
            .as_encoded_bytes()
            .windows(query.len())
            .any(|window| window == query)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search<'a>(paths: &'a [PathBuf], query: &str, mode: SearchMode) -> Vec<&'a Path> {
        find_paths(paths, OsStr::new(query), mode)
    }

    #[test]
    fn exact_name_is_distinct_from_exact_path() {
        let paths = [PathBuf::from("reports").join("report.pdf")];
        assert_eq!(search(&paths, "report.pdf", SearchMode::ExactName).len(), 1);
        assert!(search(&paths, "report.pdf", SearchMode::ExactPath).is_empty());
        assert_eq!(
            find_paths(&paths, paths[0].as_os_str(), SearchMode::ExactPath),
            vec![paths[0].as_path()]
        );
        assert!(find_paths(&paths, paths[0].as_os_str(), SearchMode::ExactName).is_empty());
    }

    #[test]
    fn substring_scope_is_explicit() {
        let paths = [PathBuf::from("reports").join("notes.txt")];
        assert_eq!(
            search(&paths, "reports", SearchMode::PathSubstring).len(),
            1
        );
        assert!(search(&paths, "reports", SearchMode::NameSubstring).is_empty());
        assert_eq!(search(&paths, "notes", SearchMode::NameSubstring).len(), 1);
    }

    #[test]
    fn all_modes_are_case_sensitive() {
        let paths = [PathBuf::from("Report.pdf")];
        for mode in [
            SearchMode::ExactName,
            SearchMode::ExactPath,
            SearchMode::NameSubstring,
            SearchMode::PathSubstring,
        ] {
            assert!(search(&paths, "report.pdf", mode).is_empty());
        }
    }

    #[test]
    fn substring_is_contiguous_not_fuzzy() {
        let paths = [PathBuf::from("report.pdf")];
        assert_eq!(search(&paths, "port.p", SearchMode::NameSubstring).len(), 1);
        assert!(search(&paths, "rptpdf", SearchMode::NameSubstring).is_empty());
    }

    #[test]
    fn empty_queries_have_defined_semantics() {
        let paths = [PathBuf::from("a.txt"), PathBuf::from("b.txt")];
        for mode in [SearchMode::NameSubstring, SearchMode::PathSubstring] {
            assert_eq!(search(&paths, "", mode).len(), 2);
        }
        for mode in [SearchMode::ExactName, SearchMode::ExactPath] {
            assert!(search(&paths, "", mode).is_empty());
        }
        assert!(search(&[], "", SearchMode::PathSubstring).is_empty());
    }

    #[test]
    fn preserves_order_and_duplicate_names() {
        let paths = [
            PathBuf::from("z").join("report.pdf"),
            PathBuf::from("a").join("report.pdf"),
        ];
        assert_eq!(
            search(&paths, "report.pdf", SearchMode::ExactName),
            vec![paths[0].as_path(), paths[1].as_path()]
        );
    }

    #[test]
    fn unicode_spaces_and_metacharacters_are_literal() {
        let paths = [PathBuf::from("资料 [草稿] $预算.txt")];
        for query in ["资料", " [草稿]", "$预算", "资料 [草稿] $预算.txt"] {
            assert_eq!(search(&paths, query, SearchMode::NameSubstring).len(), 1);
        }
        assert!(search(&paths, "*.txt", SearchMode::NameSubstring).is_empty());
    }

    #[test]
    fn does_not_normalize_unicode_or_path_spelling() {
        let paths = [PathBuf::from("café.txt"), PathBuf::from("a/./b.txt")];
        assert!(search(&paths, "cafe\u{301}.txt", SearchMode::ExactName).is_empty());
        assert!(search(&paths, "a/b.txt", SearchMode::ExactPath).is_empty());
        assert_eq!(search(&paths, "a/./b.txt", SearchMode::ExactPath).len(), 1);
    }

    #[test]
    fn long_query_and_name_do_not_require_filesystem_io() {
        let name = format!("{}.txt", "a".repeat(16_384));
        let paths = [PathBuf::from(&name)];
        assert_eq!(search(&paths, &name, SearchMode::ExactName).len(), 1);
        assert!(search(&paths, &"b".repeat(32_768), SearchMode::PathSubstring).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn invalid_utf8_is_preserved_without_lossy_false_matches() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let name = OsString::from_vec(b"report-\xff.txt".to_vec());
        let paths = [PathBuf::from("folder").join(&name)];
        assert_eq!(
            search(&paths, "report-", SearchMode::NameSubstring).len(),
            1
        );
        assert_eq!(find_paths(&paths, &name, SearchMode::ExactName).len(), 1);
        assert!(search(&paths, "\u{fffd}", SearchMode::PathSubstring).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn unpaired_utf16_is_preserved_without_lossy_false_matches() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let name = OsString::from_wide(&[b'a' as u16, 0xD800, b'b' as u16]);
        let paths = [PathBuf::from("folder").join(&name)];
        assert_eq!(find_paths(&paths, &name, SearchMode::ExactName).len(), 1);
        assert_eq!(search(&paths, "a", SearchMode::NameSubstring).len(), 1);
        assert!(search(&paths, "\u{fffd}", SearchMode::PathSubstring).is_empty());
    }
}
