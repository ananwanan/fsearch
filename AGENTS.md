# FSearch — Agent Development Guide

This file defines the engineering contract for AI coding agents (including Codex) and contributors working in this repository. **Read this file, `README.md`, relevant source files, and tests before changing code.** Repository state is authoritative; this guide is not a substitute for inspecting implementations.

## 1. Mission and non-negotiable priorities

FSearch is a **small, polished, fast desktop file-name and path search application**, built in Rust with **egui/eframe**. It targets Windows, Linux, and macOS, subject to actual build and runtime verification.

1. **Correctness and filesystem safety first.** Never invent results, discard errors silently, or confuse a partial scan with a complete index.
2. **Responsiveness.** Scanning, indexing, matching large datasets, metadata I/O, and OS actions must stay off the GUI render path.
3. **Measured performance.** Favor low-latency searches, predictable RAM/CPU usage, quick startup, and small distributables, but benchmark before claiming improvements.
4. **Exact and fuzzy search with explicit semantics.** Literal substring matching is **not** fuzzy matching.
5. **Minimalism.** Keep UX clean, keyboard-friendly, accessible, and dependencies justified. Do not replace egui with Qt, Flutter, Electron, Tauri, or a WebView.

Never advertise an unimplemented feature as shipping. Avoid unrelated refactors, artificial abstractions, or a wholesale rewrite.

## 2. Repository snapshot (2026-10-11)

The repository is a Rust **2021 Cargo virtual workspace** with `resolver = "2"` and three crates. Root `default-members` selects only the CLI, not the desktop app.

```text
Cargo.toml                      workspace, shared dependencies, release profile
Cargo.lock                      committed application lockfile
.github/workflows/rust.yml      Windows/Linux/macOS fmt, tests, Clippy, release builds
crates/
  fsearch-core/
    src/lib.rs                  public, UI-independent scan/match API
    src/scanner.rs              iterative scan, native paths, issues, progress, cancellation
    src/search.rs               four literal search modes, unit tests
  fsearch-cli/
    src/args.rs                 legacy CLI args
    src/main.rs                 executable: search
    tests/cli.rs                CLI regressions
  fsearch-desktop/
    src/main.rs                 eframe/Glow launcher and startup flags
    src/app.rs                  egui UI, preferences, keyboard/mouse interactions
    src/model.rs                view state, generation checks, selection/cancel logic
    src/worker.rs               background scanning, session cache, filtering, queries
    src/platform.rs             background file-open/reveal actions, optional system fonts
README.md                       English user documentation
README.zh-CN.md                 Chinese user documentation
AGENTS.md                       this development guide
```

Current manifests specify `eframe 0.36.2` with default features disabled and `accesskit`, `default_fonts`, `glow`, `wayland`, `x11` enabled; there is no WGPU renderer configuration. The desktop crate declares `rust-version = "1.95"`. `fsearch-core` has no external dependencies. Changes to MSRV, graphics backend, or dependencies require separate justification and lockfile maintenance.

### What exists today

- Iterative, one-off scanning via `scan_path` / `scan_path_with`. `ScanReport` preserves native `PathBuf` entries and structured `ScanIssue` records; `ScanProgress` and `ScanOutcome` support cooperative cancellation. Roots that are individual files/links are accepted. A directory root itself is not included. Symlinks and Windows reparse points (including junctions) are not traversed by default. No atomic no-follow guarantee exists across metadata checks and subsequent opens.
- `SearchMode::{ExactName, ExactPath, NameSubstring, PathSubstring}`, `find_paths`, and `matches_path`. **All four modes are literal, case-sensitive, non-normalizing, and non-fuzzy.** Name modes operate on the final path component; path modes operate on the complete, original path spelling. Substrings are contiguous matches using native encoded bytes. Empty substring queries match all applicable entries; empty exact queries match no normal entries. Core and CLI preserve scan order.
- Legacy `search` CLI: required `-d/--directory` and `-f/--filename`; its default behavior is **full-path substring**, despite the historic flag name. Root failure is an error (exit 1), recoverable child failures print `Scan incomplete` and also exit 1 while preserving partial results, empty/no matches exit 0, and clap argument errors exit 2. Matches still use `dbg!` on stderr; debug builds print timing on stdout when matches exist.
- `fsearch-desktop`: optional `-d/--directory`, `-q/--query`, `--mode` = `exact-name` / `exact-path` / `name-substring` / `path-substring` (default). A specified root triggers scanning; otherwise the working directory is prefilled but scanning waits for user action. There is no native folder picker; users may paste or drop paths.
- Desktop has a UI for the four modes, root and file/directory filtering; a **175 ms** query debounce; one background scan/query worker; separate scan/query generations, cooperative cancellation, an **8-event bounded channel**, approximately 100 ms progress updates, an `Arc`-shared in-session snapshot, stable native-path ordering, virtualized visible rows, and a **20,000-result display cap** with a full match count. The display cap does **not** cap scan/cache memory. New queries reuse the current snapshot rather than re-scanning.
- Actions run on a separate background worker: Windows Shell/PIDL open and reveal, macOS `open` / `open -R`, Linux `xdg-open`. Linux reveal opens the containing directory but may not select the item; Windows item selection currently has a 259 UTF-16-unit API limit. The app offers Ctrl+L, arrows, Enter/double-click, Ctrl+Shift+C, Ctrl+O, Ctrl+R, Escape; equivalent command shortcuts are used on macOS where appropriate. Light/dark, scale, and English/Chinese are **session-only**. Chinese availability depends on a discoverable local CJK font. Non-UTF-8 display/copy may be lossy even though core filesystem operations preserve native paths.

**Not implemented:** native MFT enumeration, USN Journal tracking, persistent or incremental indices, filesystem watchers, true fuzzy search/ranking, configurable case folding, glob/regex/content search, persistent settings, exclusion rules, native folder selection, or comprehensive large-tree benchmarks. Do not imply these exist.

## 3. Layering rules and evolution

Keep `fsearch-core` independent of `clap` and `egui/eframe`; it must remain testable without a GUI. CLI and desktop adapt their own input/output to core APIs. Keep platform-specific privileged access and file reveal/open behavior behind narrow interfaces. Avoid creating empty crates or introducing database/async runtimes until an incremental working slice requires them.

**Target architecture (planned; not the existing implementation):**

```text
egui/eframe UI ── commands/events ── application/search coordinator
                                        │
                                in-memory index snapshot
                                        │
                      ┌─────────────────┴──────────────────┐
                      │                                    │
             Windows NTFS backend                  portable scan backend
             MFT metadata enumeration             directory traversal
             USN Journal delta feed               optional watch + reconciliation
                      └─────────────────┬──────────────────┘
                             query / filtering / ranking
                                        │
                              bounded result batches
```

Use explicit concepts when needed: `SearchQuery`, `SearchMode`, `SearchOptions`, `IndexedEntry`, `IndexSnapshot`, `IndexProgress`, and `IndexHealth`. Do **not** introduce a speculative public trait hierarchy before implementing a second backend. Preserve the working scanner and CLI as fallback and regression checks.

## 4. Everything-inspired filesystem indexing (future work)

The high-performance goal is to **scan once, query an index repeatedly, and apply changes incrementally**. This is a design plan, not a claim of Everything compatibility or equivalent speed.

### 4.1 NTFS fast path on Windows

- Prefer documented Windows filesystem control APIs. Explore opening a supported NTFS volume with `CreateFileW`, invoking `DeviceIoControl` / `FSCTL_ENUM_USN_DATA` to enumerate file records efficiently, querying journal information using `FSCTL_QUERY_USN_JOURNAL`, and reading subsequent updates via `FSCTL_READ_USN_JOURNAL`.
- The NTFS Master File Table (MFT) underlies file records. `FSCTL_ENUM_USN_DATA` is a supported filesystem enumeration interface; **do not describe it as parsing raw MFT bytes**. The USN Journal is for changes, not an everlasting, lossless event queue.
- Build parent/child relationships from volume identity, file reference number (FRN/file ID), parent FRN, original name, and entry flags. Resolve paths correctly, including moves/renames, multiple names/hard links, reparse points, and roots. Treat ID reuse and varying file-ID widths/formats explicitly.
- Associate index snapshots with volume/journal identity and an observed USN checkpoint. Handle journal reset, deleted/rotated records, checkpoints older than the journal's first valid USN, volume remount/identity changes, record-version differences, and sequence gaps by marking the index **stale** and doing a safe rebuild/reconciliation. Never continue silently with a supposedly current index.
- Define a crash-safe protocol: initial enumeration + journal catch-up without losing concurrent changes, generation-tagged snapshots, ordered delta application, durable checkpoint written with the snapshot atomically, recovery after crash/power loss, and a visible freshness state.
- Volume/device access may require elevated privileges. **Do not require the egui UI to run as administrator.** Design optional least-privilege background service + authenticated local IPC only if justified and reviewed; otherwise fall back to a user-accessible directory scanner. Never elevate silently or disable security controls.
- Scope this backend to *supported NTFS volumes* after explicit filesystem/capability detection. Do not claim this API path works on exFAT, ReFS, SMB/NAS, Linux, or macOS. Handle unplugged volumes, removable disks, and inaccessible volumes gracefully.

### 4.2 Portable fallback

- Retain safe iterative `std::fs` directory traversal, native `PathBuf`s, per-entry error accounting, and cooperative cancellation for other filesystems/platforms or denied NTFS access.
- Future change monitors may use OS-native facilities, but never assume watchers are lossless; watch overflow, restarts, permission changes, and missing events must trigger reconciliation or rescans.
- Make symlink/reparse-point following an explicit policy; default to avoiding traversal cycles. Do not unconditionally crawl network mounts, huge volumes, or sensitive directories without user scope/consent.
- Index only file/directory **names, paths, and necessary metadata by default**; full-text file content indexing is a separate opt-in feature and performance/privacy domain.

### 4.3 Index shape and lifecycle

- Preserve native names and paths for matching and OS operations: `OsStr`, `OsString`, `PathBuf`. Keep display strings separate. Avoid `to_string_lossy()` for identity, persistence keys, or destructive actions.
- Use stable **volume-scoped IDs where available**; account for hard links and renames. Do not assume a path uniquely or permanently identifies a file. Portable scanners may need a different identity strategy.
- Choose efficient lookup structures based on measured workloads: contiguous entries, directory parent references, exact-name lookup, optional token/n-gram candidate acceleration, and memory-bounded snapshots. Do not assert a trie, database, or mmap is faster without experiments.
- Track index state (`not indexed / building / current / updating / stale / failed`) and error counts in the UI. Preserve a usable last-known snapshot with a clear stale warning when safe. Keep memory bounded for million-entry directories; batch work and publish snapshots incrementally where semantics permit.
- If persistence is added, version and validate the format, write atomically, recover from corruption, and guard against cross-version or cross-volume reuse. User-selected roots and filenames can be sensitive; choose a user-appropriate cache location and avoid exporting/indexing extra paths without consent.

## 5. Matching and ranking contract

The existing four modes must keep their behavior unless a change is explicitly requested, tested, and documented:

| Mode | Current rule |
| --- | --- |
| `ExactName` | Final native filename equals the query |
| `ExactPath` | Entire original native path spelling equals the query |
| `NameSubstring` | Contiguous literal query in native filename |
| `PathSubstring` | Contiguous literal query in entire native path |

All are currently **case-sensitive**, with **no canonicalization, Unicode normalization, wildcard interpretation, or regular expressions**. Case folding and filesystem case sensitivity are separate issues; Windows vs Unix path separators also need careful treatment.

**Future `Fuzzy` mode** must actually match out-of-order *gaps* (ordered subsequence or an explicitly chosen edit-distance model): e.g. `rptpdf` may rank `report.pdf`, which substring search will not match. Specify candidate selection, Unicode behavior, optional case sensitivity, file-name vs path scope, scoring (contiguous/boundary/basename bonuses), deterministic tie-breaking, and a strict cost/latency budget. Test ranking with Unicode, CJK, accents, non-UTF-8 entries, large candidate sets, and duplicate names. Match against native identity where possible; any textual fuzzy representation must not replace the original operating-system path.

Clearly distinguish **precise/exact**, **literal substring**, **fuzzy**, and **file content** searches. No fake claims of typo tolerance, instant search, or content indexing.

## 6. Concurrency, UI, and platform safety

- Never perform blocking filesystem traversal, OS actions, persistent index I/O, or whole-index sorting inside egui's per-frame `update`. Preserve the existing background worker model while improving it deliberately.
- Keep input debounce, generation checks, cooperative cancellation, bounded queues/backpressure, and delivery of only current results. Old scan/query events must not overwrite a newer root or query. Repaint on meaningful changes rather than continuously.
- Keep result virtualization; do not clone an entire million-row list per frame. Account for both **display limits and total index memory**. Maintain predictable selection when results change.
- Follow system-native open/reveal APIs and robust argument quoting (do not build shell command text). Treat executable names, filenames, paths, and icon loading as untrusted data.
- Maintain keyboard equivalents, accessibility, scaling, light/dark themes, concise status/progress, and reliable English/Chinese display. Avoid shipping heavyweight font assets without size/license review.
- Handle I/O failures with context. Avoid routine `unwrap`/`expect`/`panic!`, swallowing read errors, or reporting partial results as complete. Respect privilege boundaries and keep every file action user initiated.

## 7. Quality gates and benchmarking

Run when applicable (the root default command does **not** cover the whole workspace):

```bash
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --release --locked
```

The CI workflow defines this matrix for Windows, Linux, and macOS. Configuration is not evidence that all jobs passed. Do not claim commands were run unless they were actually executed; provide failures and limitations. GUI smoke testing must separately exercise drag/drop, fonts, high DPI, sorting, keyboard handling, open/reveal, scan cancellation, and long/network paths. Existing CLI behavior is a compatibility contract: add tests before changing it.

Required test categories as relevant:
- Four literal search modes; future fuzzy score/rank determinism; empty/long inputs; CJK, Unicode, invalid UTF-8/UTF-16, spaces and metacharacters.
- Directory/file/link roots, deep trees, cycles, junctions, concurrent deletions/renames, permission failures and structured partial reports.
- Background cancellation, generation races, event queue saturation, bounded memory and results, stale-index signaling and snapshot recovery.
- NTFS (when implemented): initial-enumeration/change race, create/delete/rename/move, FRN reuse, hard links, journal rotation/reset/gap, inaccessible volumes, restart recovery.
- Platform-specific checks on supported systems. Never treat an untested platform as verified.

**Measure before optimizing.** Record OS, filesystem, medium (SSD/HDD/network), CPU, Rust version, release profile, dataset size, cold/warm cache, and test queries. Compare first scan, index rebuild, live-update lag, p50/p95 query latency, RAM peak/idle, CPU idle, binary size, and perceived UI response. Do not invent performance budgets or benchmark numbers. Benchmark on repeatable fixtures and real large trees; prevent query scans from silently re-reading the disk.

## 8. Delivery roadmap

This is an incremental plan, not a license for autonomous scope expansion:

- **Foundation (largely implemented)**: separate core/CLI/desktop workspace; protect legacy CLI behavior; iterative native-path scanning, four literal modes, structured errors and tests. Remaining work: CLI mode/case options and deeper edge-case validation.
- **Desktop MVP (implemented, needs wider verification)**: egui/eframe UI, debounced asynchronous queries, session scan cache, progress/cancellation, virtualized results, keyboard/platform actions, session settings.
- **Indexing foundation (next)**: isolate a queryable index from the scan report; define identity, snapshot generation, health/freshness, batch publishing, memory policy, and repeatable baseline benchmarks. Keep generic scanner usable.
- **Fast Windows NTFS backend**: prototype `FSCTL_ENUM_USN_DATA` enumeration; robust journal checkpoints and `FSCTL_READ_USN_JOURNAL` delta handling, fallbacks and service/access policy; test correctness before optimizing.
- **True fuzzy search**: explicit algorithm, ranked output, deterministic scores, optional normalization/case policy, benchmarks and UI mode; preserve literal modes.
- **Portable updates and persistence**: optional watchers with recovery/rescan, versioned crash-safe index cache, settings/exclusion policy and user-visible stale states.
- **Product hardening**: cross-platform integration tests, accessibility, installer/package/release engineering and measured performance regression gates.

Each milestone must ship a runnable, tested vertical slice and a concise account of what still does not work.

## 9. Agent workflow and documentation policy

1. Read `AGENTS.md` and both README files, manifests, affected code, tests, and CI before planning changes; inspect the **latest** branch state.
2. Write down existing behavior, acceptance criteria, and the smallest necessary files to edit. Prefer a focused slice over a broad redesign.
3. Preserve public APIs, defaults, executable names, CLI exit semantics, and UI shortcuts unless explicitly tasked otherwise. Add tests for changes in behavior.
4. Add dependencies or services only when justified by measured need, maintenance/license review, platform support, MSRV, and package footprint. Maintain `Cargo.lock`.
5. Update `README.md` (English) and `README.zh-CN.md` (Chinese) together for user-visible behavior, supported platforms, command examples, and feature status; keep `AGENTS.md` technical and **English**.
6. State modified files, observed tests/CI status, unverified platforms, known limitations, and remaining work. No fabricated results or benchmark claims.
7. Do not commit secrets, local cache/index databases, sensitive filesystem paths, personal files, or generated binaries.

### Historical verification boundary

The previous repository guide records a Windows local development pass on 2026-10-11 using Rust/Cargo 1.99: 62 total tests (29 core, 22 desktop and 11 CLI), fmt, Clippy, release build, and selected GUI interactions. **This is historical project documentation, not a new validation of the current head or of Linux/macOS.** Re-run checks after code changes and verify CI independently.

## 10. License and releases

No license file is present in the inspected repository snapshot. Do not assert a project license or publish a release with assumptions about redistribution rights. Select and add a license deliberately before advertising an open-source distribution.
