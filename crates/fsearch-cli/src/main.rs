use clap::Parser;
use fsearch_core::{find_paths, scan_path, SearchMode};
use std::ffi::OsStr;
use std::process::ExitCode;
use std::time::Instant;

use crate::args::Args;

mod args;

fn main() -> ExitCode {
    let start = Instant::now();
    let args = Args::parse();
    let report = match scan_path(&args.directory) {
        Ok(report) => report,
        Err(issue) => {
            eprintln!("Scan failed: {issue}");
            return ExitCode::FAILURE;
        }
    };

    for issue in &report.issues {
        eprintln!("Scan incomplete: {issue}");
    }
    let status = if report.issues.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    };

    if report.paths.is_empty() {
        // Keep the prototype's output for a successfully scanned empty directory.
        if report.issues.is_empty() {
            println!("File list not found");
        }
        return status;
    }
    let found_files = find_paths(
        &report.paths,
        OsStr::new(&args.filename),
        SearchMode::PathSubstring,
    );
    if found_files.is_empty() {
        return status;
    }
    dbg!(&found_files);
    if cfg!(debug_assertions) {
        println!("Total time taken: {:?}", start.elapsed());
    }
    status
}
