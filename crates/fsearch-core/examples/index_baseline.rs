//! Repeatable index baseline; run through scripts/Measure-IndexBaseline.ps1 on Windows.
//! Std-only example: no benchmark framework or runtime dependencies.

use fsearch_core::{
    build_index, EntryFilter, IndexBuildEvent, IndexHealth, IndexLimits, IndexSnapshot,
    IndexStopReason, SearchMode, SearchQuery,
};
use std::error::Error;
use std::ffi::OsString;
use std::hint::black_box;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let root = PathBuf::from(args.next().ok_or_else(|| {
        io::Error::other("usage: index_baseline ROOT [ITERATIONS] [SELF_CHECK_PARENT]")
    })?);
    let iterations: usize = args
        .next()
        .map(|value| value.to_string_lossy().parse())
        .transpose()?
        .unwrap_or(100);
    if iterations == 0 {
        return Err(io::Error::other("iterations must be greater than zero").into());
    }
    let self_check_parent = args.next().map(PathBuf::from);
    if args.next().is_some() {
        return Err(io::Error::other("too many arguments").into());
    }

    memory("baseline")?;
    let first = timed_build(&root, 1, "first")?;
    require_complete(&first)?;
    memory("indexed")?;

    // Retain the usable first snapshot during a rebuild, as the desktop does.
    let rebuilt = timed_build(&root, 2, "rebuild_with_old_snapshot")?;
    require_complete(&rebuilt)?;
    memory("two_snapshots")?;
    if rebuilt.len() != first.len() {
        return Err(io::Error::other("dataset changed between baseline builds").into());
    }
    drop(first);
    memory("one_snapshot_after_rebuild")?;
    benchmark_queries(&rebuilt, iterations)?;
    verify_cancellation_and_limit(&root, rebuilt.len())?;

    if let Some(parent) = self_check_parent {
        verify_snapshot_without_files(&parent)?;
    }
    memory("idle_after_queries")?;
    // The idle interval is outside measured query timings. Peak WS is the OS
    // lifetime high-water mark, not a polling estimate.
    let idle_started = Instant::now();
    let cpu_before = process_cpu_time()?;
    std::thread::sleep(Duration::from_millis(250));
    if let (Some(before), Some(after)) = (cpu_before, process_cpu_time()?) {
        println!(
            "{{\"kind\":\"idle\",\"wall_ms\":{:.3},\"cpu_ms\":{:.3}}}",
            idle_started.elapsed().as_secs_f64() * 1000.0,
            after.saturating_sub(before).as_secs_f64() * 1000.0,
        );
    }
    memory("idle_250ms")?;
    Ok(())
}

fn timed_build(root: &Path, generation: u64, phase: &str) -> Result<IndexSnapshot, Box<dyn Error>> {
    let started = Instant::now();
    let mut publications = 0;
    let snapshot = build_index(root, generation, IndexLimits::default(), |event| {
        if matches!(event, IndexBuildEvent::Published(_)) {
            publications += 1;
        }
        true
    })?;
    let elapsed = started.elapsed();
    println!(
        "{{\"kind\":\"build\",\"phase\":\"{phase}\",\"elapsed_ms\":{:.3},\"entries\":{},\"issues\":{},\"path_bytes\":{},\"entry_storage_lower_bound_bytes\":{},\"publications\":{},\"generation\":{},\"revision\":{}}}",
        elapsed.as_secs_f64() * 1000.0,
        snapshot.len(),
        snapshot.progress().issues,
        snapshot.progress().path_bytes,
        snapshot.entry_storage_bytes(),
        publications,
        snapshot.generation(),
        snapshot.revision(),
    );
    Ok(snapshot)
}

fn require_complete(snapshot: &IndexSnapshot) -> io::Result<()> {
    if snapshot.health() != IndexHealth::Current || snapshot.stop_reason().is_some() {
        return Err(io::Error::other(
            "baseline requires a complete index; partial/limited/cancelled data is rejected",
        ));
    }
    Ok(())
}

fn benchmark_queries(snapshot: &IndexSnapshot, iterations: usize) -> Result<(), Box<dyn Error>> {
    let samples: Vec<_> = snapshot
        .iter()
        .step_by((snapshot.len() / 9).max(1))
        .take(9)
        .collect();
    if samples.is_empty() {
        return Err(io::Error::other("baseline requires a non-empty dataset").into());
    }
    for (label, mode) in [
        ("exact_name", SearchMode::ExactName),
        ("exact_path", SearchMode::ExactPath),
        ("name_substring", SearchMode::NameSubstring),
        ("path_substring", SearchMode::PathSubstring),
    ] {
        let mut queries: Vec<OsString> = match mode {
            SearchMode::ExactName => samples
                .iter()
                .filter_map(|entry| entry.path.file_name().map(OsString::from))
                .collect(),
            SearchMode::ExactPath => samples
                .iter()
                .map(|entry| entry.path.as_os_str().to_os_string())
                .collect(),
            SearchMode::NameSubstring | SearchMode::PathSubstring => [
                "report",
                "資料",
                "café",
                ".txt",
                ".rs",
                "000",
                "dir-",
                "",
                "[literal]",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
        };
        queries.push(OsString::from("fsearch-no-such-name-7be19f38"));
        measure_queries(snapshot, &queries, mode, label, "mixed", iterations)?;
        if matches!(mode, SearchMode::NameSubstring | SearchMode::PathSubstring) {
            measure_queries(
                snapshot,
                &[OsString::new()],
                mode,
                label,
                "empty_all",
                iterations,
            )?;
            measure_queries(
                snapshot,
                &[OsString::from("fsearch-no-such-name-7be19f38")],
                mode,
                label,
                "miss",
                iterations,
            )?;
        }
    }
    Ok(())
}

fn measure_queries(
    snapshot: &IndexSnapshot,
    queries: &[OsString],
    mode: SearchMode,
    label: &str,
    workload: &str,
    iterations: usize,
) -> io::Result<()> {
    let mut timings = Vec::with_capacity(iterations);
    let mut checksum = 0usize;
    for iteration in 0..(iterations + 10) {
        let query = SearchQuery {
            text: queries[iteration % queries.len()].clone(),
            mode,
            filter: EntryFilter::All,
        };
        let started = Instant::now();
        let result = snapshot
            .query_with(black_box(&query), 20_000, || true)
            .ok_or_else(|| io::Error::other("unexpected query cancellation"))?;
        let elapsed = started.elapsed();
        if workload == "empty_all"
            && (result.total_matches != snapshot.len()
                || result.indices.len() != snapshot.len().min(20_000))
        {
            return Err(io::Error::other("result cap lost the full match count"));
        }
        checksum = checksum.wrapping_add(black_box(result.total_matches));
        black_box(result.indices);
        if iteration >= 10 {
            timings.push(elapsed);
        }
    }
    timings.sort_unstable();
    println!(
        "{{\"kind\":\"query\",\"mode\":\"{label}\",\"workload\":\"{workload}\",\"iterations\":{iterations},\"warmups\":10,\"p50_us\":{:.3},\"p95_us\":{:.3},\"checksum\":{checksum}}}",
        percentile(&timings, 50).as_secs_f64() * 1_000_000.0,
        percentile(&timings, 95).as_secs_f64() * 1_000_000.0,
    );
    Ok(())
}

fn percentile(values: &[Duration], percent: usize) -> Duration {
    // Nearest-rank percentile; callers guarantee non-empty measurements.
    values[(values.len() * percent).div_ceil(100).saturating_sub(1)]
}

fn verify_cancellation_and_limit(
    root: &Path,
    complete_entries: usize,
) -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    let mut observed_entries = 0;
    let cancelled = build_index(root, 3, IndexLimits::default(), |event| {
        match event {
            IndexBuildEvent::Progress(progress) => observed_entries = progress.entries,
            IndexBuildEvent::Published(ref snapshot) => observed_entries = snapshot.len(),
        }
        observed_entries < 512
    })?;
    let reached_threshold = observed_entries >= 512;
    if reached_threshold && cancelled.stop_reason() != Some(IndexStopReason::Cancelled) {
        return Err(io::Error::other("build cancellation was not reported").into());
    }
    println!(
        "{{\"kind\":\"check\",\"name\":\"build_cancellation\",\"threshold_reached\":{reached_threshold},\"entries\":{},\"elapsed_ms\":{:.3}}}",
        cancelled.len(),
        started.elapsed().as_secs_f64() * 1000.0,
    );
    let query = SearchQuery {
        text: OsString::new(),
        mode: SearchMode::PathSubstring,
        filter: EntryFilter::All,
    };
    if cancelled.query_with(&query, 20_000, || false).is_some() {
        return Err(io::Error::other("query cancellation was not reported").into());
    }
    let limited = build_index(
        root,
        4,
        IndexLimits {
            max_entries: 32,
            ..IndexLimits::default()
        },
        |_| true,
    )?;
    if complete_entries > 32
        && (limited.health() == IndexHealth::Current
            || limited.stop_reason() != Some(IndexStopReason::EntryLimit))
    {
        return Err(io::Error::other("entry limit was reported as a complete index").into());
    }
    println!(
        "{{\"kind\":\"check\",\"name\":\"entry_limit\",\"entries\":{},\"partial\":{},\"query_cancelled\":true}}",
        limited.len(),
        limited.health() == IndexHealth::Partial,
    );
    Ok(())
}

fn verify_snapshot_without_files(parent: &Path) -> Result<(), Box<dyn Error>> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let root = parent.join(format!(
        "fsearch-detached-check-{}-{timestamp}",
        std::process::id()
    ));
    // create_dir must succeed: cleanup only ever targets this newly owned child.
    std::fs::create_dir(&root)?;
    let check_result = (|| -> Result<(), Box<dyn Error>> {
        std::fs::write(root.join("資料-report.txt"), [])?;
        let snapshot = build_index(&root, 5, IndexLimits::default(), |_| true)?;
        require_complete(&snapshot)?;
        std::fs::remove_file(root.join("資料-report.txt"))?;
        std::fs::remove_dir(&root)?;
        let result = snapshot
            .query_with(
                &SearchQuery {
                    text: OsString::from("資料-report.txt"),
                    mode: SearchMode::ExactName,
                    filter: EntryFilter::All,
                },
                20_000,
                || true,
            )
            .ok_or_else(|| io::Error::other("detached query unexpectedly cancelled"))?;
        if result.total_matches != 1 {
            return Err(io::Error::other("query lost its snapshot after removing files").into());
        }
        println!("{{\"kind\":\"check\",\"name\":\"query_after_root_removed\",\"passed\":true}}");
        Ok(())
    })();
    if root.exists() {
        // Only this known, freshly created one-file fixture can remain on error.
        let file = root.join("資料-report.txt");
        if file.exists() {
            std::fs::remove_file(file)?;
        }
        std::fs::remove_dir(root)?;
    }
    check_result
}

fn memory(phase: &str) -> io::Result<()> {
    if let Some(values) = process_memory()? {
        println!(
            "{{\"kind\":\"memory\",\"phase\":\"{phase}\",\"working_set_bytes\":{},\"peak_working_set_bytes\":{},\"private_committed_bytes\":{}}}",
            values.0, values.1, values.2,
        );
    } else {
        println!("{{\"kind\":\"memory\",\"phase\":\"{phase}\",\"available\":false}}");
    }
    Ok(())
}

#[cfg(windows)]
fn process_memory() -> io::Result<Option<(usize, usize, usize)>> {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
    }
    #[link(name = "psapi")]
    extern "system" {
        fn GetProcessMemoryInfo(process: *mut c_void, counters: *mut Counters, cb: u32) -> i32;
    }
    let mut counters = Counters {
        cb: std::mem::size_of::<Counters>() as u32,
        ..Counters::default()
    };
    // SAFETY: the pseudo handle is the current process and Counters has the
    // documented PROCESS_MEMORY_COUNTERS_EX layout and exact buffer length.
    let success = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some((
        counters.working_set_size,
        counters.peak_working_set_size,
        counters.private_usage,
    )))
}

#[cfg(not(windows))]
fn process_memory() -> io::Result<Option<(usize, usize, usize)>> {
    // Query/build timings are portable; this baseline deliberately does not
    // conflate other platforms' RSS/VM metrics with Windows private commitment.
    Ok(None)
}

#[cfg(windows)]
fn process_cpu_time() -> io::Result<Option<Duration>> {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    impl FileTime {
        fn ticks(&self) -> u64 {
            (u64::from(self.high) << 32) | u64::from(self.low)
        }
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessTimes(
            process: *mut c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }
    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    // SAFETY: all four output buffers use the documented FILETIME layout and
    // are writable; the pseudo handle refers to the current process.
    let success = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(Duration::from_nanos(
        (kernel.ticks() + user.ticks()) * 100,
    )))
}

#[cfg(not(windows))]
fn process_cpu_time() -> io::Result<Option<Duration>> {
    Ok(None)
}
