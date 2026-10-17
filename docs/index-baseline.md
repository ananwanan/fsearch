# Index baseline

`fsearch-core/examples/index_baseline.rs` measures the portable index API directly,
without GUI rendering, debounce, channel delivery, or file actions. It adds no Cargo
dependencies. This is an initial comparison point for future changes, not a claim
of improved performance or a release performance gate.

## Reproduce on Windows

Use PowerShell 7 and the repository's Rust toolchain:

```powershell
pwsh -NoProfile -File scripts/Measure-IndexBaseline.ps1 `
  -FixtureFiles 50000 -Directories 200 -Iterations 100 `
  -RealRoot .\target\debug `
  -OutputPath .\docs\index-baseline-2026-10-17.json
```

`RealRoot` is optional and must name an existing, explicitly selected directory.
The script never chooses another real tree automatically. The example above uses
this project's existing build tree; it does not crawl a drive or a user profile.
Avoid concurrent builds while measuring that tree.

The generated fixture has 50,000 empty files in 200 directories, with deterministic
`report`, `資料`, `café`, `notes`, and `[literal]` names. Each run creates a unique
directory under `D:\Cache\Codex` when it exists, otherwise the system temporary
directory. Override this with `-FixtureParent`. Only that newly created fixture
is removed at the end; `-KeepFixture` retains it. Existing real data is read only.
The JSON report includes counts and environment details, without recording real
root paths, file names, or sampled native exact queries.

The wrapper builds the example with the workspace release profile. It runs each
dataset in a fresh process and records:

- First index build and immediate rebuild elapsed time, observed entries/errors,
  batch publications, native path capacity, and entry storage lower bound. The
  first complete snapshot stays alive during the rebuild, matching the desktop's
  fallback requirement.
- Four cached query workloads, each with 10 warmups followed by 100 measurements.
  Nearest-rank p50/p95 include matching and collection of up to 20,000 native-path
  sorted result indices, with the full match count. Query input construction and
  rendering are excluded. Exact queries sample up to nine native entries and a
  miss; substring queries cycle through `report`, `資料`, `café`, `.txt`, `.rs`,
  `000`, `dir-`, empty, `[literal]`, and a miss. All entry kinds are included.
  Substring modes also have separate `empty_all` and `miss` distributions. The
  broad empty query verifies the 20,000-result cap and unchanged total match count;
  its time includes the heap and ordering needed to select displayed results.
- Windows current working set, process lifetime peak working set, and current
  private committed bytes before indexing, after each build, with both snapshots,
  after releasing the first snapshot, and after querying. Peak working set comes
  from `GetProcessMemoryInfo`, so short peaks are not missed by polling. Private
  committed bytes are not private working set. A 250 ms idle sample reports the
  process CPU time delta using `GetProcessTimes`; its timer resolution is coarse.
- Cooperative build cancellation at 512 observed entries, immediate query
  cancellation, and a 32-entry budget that must remain explicitly partial. Small
  real trees can finish before reaching the cancellation threshold; the JSON
  reports this instead of pretending a cancellation happened.
- A separate, newly owned one-file fixture is indexed and then removed. Its
  cached exact query must still return the recorded entry. This checks that query
  execution does not re-read metadata or re-enumerate the original root.

No OS caches are flushed. Generated data is warm from creation. “First” means the
first index build in a fresh measured process, **not a cold-storage scan**. Rebuild
and query results are warm. File contents are not read. This harness does not
measure a million-entry tree, HDD/network behavior, GUI latency, or live updates.
The index has no watcher; automatic update lag is inapplicable.

The example can also run directly:

```text
cargo run -p fsearch-core --example index_baseline --release --locked -- ROOT 100 SELF_CHECK_PARENT
```

Build/query timing is portable, but process memory and idle CPU reporting currently
use Windows APIs. Other platforms explicitly emit `available: false` for memory;
Linux/macOS measurements have not been performed.

## Recorded Windows baseline

Measured on 2026-10-17 at 20:06 Asia/Shanghai. The
[companion JSON](index-baseline-2026-10-17.json) contains the actual run and all
counters. Environment: Windows 11 Pro 10.0.26300 x64, Intel Core i5-12600KF
(16 logical processors), NTFS on a KIOXIA EXCERIA G2 NVMe SSD, Rust 1.99.0
`x86_64-pc-windows-msvc`, workspace release profile with LTO, no debug symbols,
and `panic=abort`. The standalone benchmark executable was 229,888 bytes;
this is not the desktop executable size.

| Dataset | Observed entries | First build | Rebuild with old snapshot | Publications per build | Observed issues |
| --- | ---: | ---: | ---: | ---: | ---: |
| Generated 50,000 files + 200 directories | 50,200 | 1,267.902 ms | 1,463.141 ms | 25 | 0 |
| Existing repository `target/debug` tree | 3,713 | 111.480 ms | 83.443 ms | 2 | 0 |

The existing tree is a modest real build-artifact dataset and changes with Cargo
activity; it is not a large production filesystem. Both builds within each
measured run had the same entry count. The generated dataset retained 4,975,800
bytes of native path capacity and an entry-storage lower bound of 7,787,000 bytes.

Query times below are microseconds, as `p50 / p95`, with 100 measured queries per
row after 10 warmups:

| Mode | Workload | Generated 50,200 entries | Existing 3,713 entries |
| --- | --- | ---: | ---: |
| Exact name | Mixed samples + miss | 979.2 / 1,240.2 | 86.3 / 88.9 |
| Exact path | Mixed samples + miss | 117.3 / 172.1 | 3.5 / 4.2 |
| Name substring | Mixed | 4,095.6 / 13,618.6 | 237.7 / 689.2 |
| Name substring | Empty, all entries | 11,892.3 / 13,143.8 | 687.8 / 695.8 |
| Name substring | Miss | 946.6 / 1,046.8 | 100.4 / 103.6 |
| Path substring | Mixed | 11,019.1 / 20,244.3 | 604.9 / 811.2 |
| Path substring | Empty, all entries | 10,932.9 / 11,303.8 | 594.4 / 627.6 |
| Path substring | Miss | 7,240.2 / 7,625.4 | 575.9 / 602.9 |

Broad queries spend time selecting and ordering the capped result indices as
well as matching. These measurements do not isolate a pure predicate cost and
should not be compared to a matcher-only microbenchmark. Empty queries on the
generated fixture returned 20,000 indices while retaining a full count of 50,200.

| Process memory, MiB | Generated fixture | Existing tree |
| --- | ---: | ---: |
| Working set before indexing | 4.500 | 4.500 |
| Working set after first build | 13.059 | 5.508 |
| Process lifetime peak working set | 21.051 | 6.156 |
| Private commitment with both snapshots retained | 17.555 | 2.121 |
| Working set after queries and 250 ms idle | 13.875 | 5.652 |
| Private commitment after queries and idle | 11.020 | 1.707 |

Both runs stopped cancelled builds at 512 entries (12.633 ms for the generated
fixture, 12.134 ms for the existing tree). These times include scanning up to the
cancellation threshold; they are not the delay from an external cancel click.
Both runs also passed immediate query cancellation, the explicitly partial
32-entry limit, full-count/result-cap checks, and query-after-root-removal checks.
The 250 ms idle CPU samples recorded 0 ms at Windows process-time resolution;
this brief coarse sample does not establish a sustained idle CPU guarantee.

No earlier comparable baseline was available, so no speedup is asserted. Changes
to the API, query distribution, dataset, or default budgets require another run.

## Limits and interpretation

The harness uses `IndexLimits::default()`: at most 1,000,000 entries, 128 MiB of
retained native path capacity, 16,384 pending directories with 8 MiB of pending
path capacity, and 64 issue details with 256 KiB of retained issue paths. Normal
publication batches contain 2,048 entries. Hitting a traversal budget yields a
partial index and a stop reason; this baseline rejects incomplete builds.

These are allocation policies, not process RSS guarantees. Entry structs, tree
nodes, allocator/OS buffers, bounded query output, sampled errors, and old retained
snapshots are additional costs. Rebuilding while preserving an old snapshot can
hold two independent entry/path budgets. The recorded entry storage byte count is
a lower bound, and the OS counters are the measured process footprint.

The harness drops intermediate publications immediately; it does not simulate a
slow GUI holding a full event queue. Desktop unit tests cover bounded delivery,
cancellation, and scan/query generation races separately. A passing core baseline
does not substitute for those tests or for GUI and other-platform verification.
