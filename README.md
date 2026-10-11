# FSearch

**Fast, precise, lightweight desktop file search — built with Rust and egui.**

[English](README.md) · [简体中文](README.zh-CN.md) · [Development guide](AGENTS.md)

> **Status: early desktop MVP, not an Everything-equivalent indexer yet.** FSearch currently provides an egui desktop interface, asynchronous directory scanning and searching against an in-memory **session snapshot**, plus a compatible CLI. Persistent indexing, NTFS/MFT enumeration, USN Journal updates, true fuzzy matching, and filesystem change tracking are **planned**, not implemented.

## Why FSearch?

- **Fast-feeling UI:** scanning and searching happen on a worker thread, with debounced input and a virtualized result list.
- **Precise search:** explicitly choose exact file name, exact full path, file-name substring, or full-path substring.
- **Small and focused:** native Rust core, egui/eframe desktop shell, straightforward file actions, minimal clutter.
- **Reliable paths:** scanning and matching retain native `PathBuf` / `OsStr` data; failures can be reported without discarding successful results.
- **Cross-platform direction:** Windows, Linux, and macOS are targeted; actual support and verification vary by feature/platform.

We aim to make file-name and path searching fast without treating file-content indexing as part of the default scope.

## Current features

| Feature | Available now | Important detail |
| --- | --- | --- |
| Desktop GUI | Yes | egui / eframe 0.36.2, Glow / OpenGL |
| Recursive directory scan | Yes | Iterative, native paths; does not follow directory symlinks or Windows reparse points by default |
| Exact name / exact path | Yes | Separate literal, case-sensitive modes |
| File-name / full-path substring | Yes | Contiguous literal matches, not fuzzy |
| Background queries | Yes | 175 ms debounce and session scan cache; changed queries do not trigger a new scan |
| Progress / cancellation | Yes | Cooperative; cannot abort an OS operation already in progress |
| Results and navigation | Yes | Sorted by native path in desktop; virtualized list, file/directory filters, open/reveal/copy |
| Interface preferences | Session only | Light/dark theme, UI scale, English/Chinese when a local CJK font is available |
| Persistent disk index | **No** | No cached index survives restarting the app |
| NTFS MFT / USN monitoring | **No** | Future platform-specific indexing backend |
| Real fuzzy matching | **No** | Future scored, non-contiguous matching |
| Live file change updates | **No** | Rescan manually to reflect changes |

The desktop displays at most **20,000 matching rows** while retaining the total match count. **This is a display limit, not an index-memory limit.** The current full scan is held in memory. No broad performance or memory claims have been benchmarked.

## Getting started

### Requirements

- A recent stable Rust toolchain. The desktop crate declares **Rust 1.95+**.
- A supported native desktop/windowing environment with OpenGL/Glow and the usual platform build dependencies.
- Windows, Linux, or macOS is the intended platform set; do not interpret the existence of CI jobs as confirmation of GUI runtime behavior on every platform.

```bash
git clone https://github.com/ananwanan/fsearch.git
cd fsearch
```

### Desktop application

```bash
cargo run -p fsearch-desktop --release --locked
```

With no arguments, FSearch **prefills** the current working directory but **does not scan automatically**. Paste a path or drag/drop a file or directory, then start scanning. There is no native directory-picker dialog yet.

Supply an initial root to scan on launch:

```bash
cargo run -p fsearch-desktop --release --locked -- \
  --directory ./crates \
  --query search \
  --mode name-substring
```

| Argument | Alias | Meaning |
| --- | --- | --- |
| `--directory PATH` | `-d` | Optional scan root; when supplied, the scan starts automatically |
| `--query TEXT` | `-q` | Initial search text (default: empty) |
| `--mode MODE` | — | `exact-name`, `exact-path`, `name-substring` (default), or `path-substring` |

The search modes are always case-sensitive in this version.

**Keyboard and mouse actions**

| Shortcut | Action |
| --- | --- |
| `Ctrl+L` | Focus the search field |
| `↑` / `↓` | Select previous / next result |
| `Enter` or double-click | Open selected file or directory |
| `Ctrl+Shift+C` | Copy the selected path |
| `Ctrl+O` | Reveal/open containing directory |
| `Ctrl+R` | Rescan |
| `Escape` | Cancel current work, or clear the query when idle |

On macOS, relevant command shortcuts use **Cmd**. Results can also be opened or revealed through the UI.

### Command-line application

The original CLI remains available, with package/executable name **`search`**:

```bash
cargo run --release --locked -- --directory ./crates --filename search.rs
```

You may also explicitly select it via `cargo run -p search --release --locked -- ...`. Root `default-members` targets this CLI.

| Required argument | Alias | Current behavior |
| --- | --- | --- |
| `--directory PATH` | `-d` | Start at an existing directory or file |
| `--filename TEXT` | `-f` | **Case-sensitive, contiguous substring of the entire candidate path** |

The `--filename` flag is a legacy name: it does **not** restrict matching to the file name, and the CLI does not currently expose the desktop's four mode choices. Matching paths are printed using Rust's `dbg!` format to **stderr**. Successful empty directories print `File list not found`. Missing/unreadable roots fail (exit 1); recoverable child errors report `Scan incomplete`, keep partial matches, and also exit 1; valid scans with zero matches exit 0; invalid CLI arguments exit 2.

## Search semantics

These are the four **implemented** `fsearch-core` modes:

| Mode | Query example | Matches |
| --- | --- | --- |
| `ExactName` | `report.pdf` | Final component exactly equals `report.pdf` |
| `ExactPath` | `reports/report.pdf` | Entire original path spelling exactly equals the query |
| `NameSubstring` | `port.p` | Contiguous substring in the final name `report.pdf` |
| `PathSubstring` | `reports` | Contiguous substring anywhere in the candidate path |

The core uses native path representations without automatic path canonicalization, Unicode normalization, or case folding. Empty substring queries match all scanned candidates. Wildcards, regular expressions, typo correction, and ranked fuzzy matching are not supported.

A **future fuzzy mode** may match non-adjacent characters, e.g. `rptpdf` → `report.pdf`, with explicit ranking and tie-breaking. It must not replace or silently change the four existing literal modes.

## How it works today

```text
             egui / eframe desktop
                      |
                UI command/events
                      |
             background worker
                 /         \
      iterative scanner   literal matcher
               \          /
                session snapshot
                      |
           filtered/virtualized rows
```

The workspace keeps the filesystem/search core separate from frontends:

```text
crates/
  fsearch-core/       native path scan + literal matching; no external dependencies
  fsearch-cli/        compatible command-line interface (binary: search)
  fsearch-desktop/    egui/eframe GUI, background jobs, platform file actions
```

The desktop scans into an in-memory snapshot, searches it on subsequent queries, and rescans only on request or when a new root is selected. This is **not** a persistent or automatically updated filesystem index.

## Roadmap: toward Everything-style speed

**The target architecture is a persistent/indexed search engine, not a faster `read_dir` loop on every keystroke.** Work will be incremental and benchmark driven.

1. **Index foundation:** separate queryable snapshots from scan reports; define file identity, status/freshness, bounded memory, query workloads, and baseline benchmarks.
2. **Windows NTFS fast path:** evaluate supported volume enumeration using `FSCTL_ENUM_USN_DATA` (backed by NTFS file records/MFT metadata), track updates using `FSCTL_QUERY_USN_JOURNAL` and `FSCTL_READ_USN_JOURNAL`, and reconstruct parent/file relationships safely. Handle journal resets, gaps, renames, hard links and offline volumes with a rebuild path.
3. **Cross-platform fallback:** retain the portable iterative directory scanner for Linux, macOS, non-NTFS volumes, network drives, and cases where Windows volume access is unavailable. Optional native change watchers must support error/overflow reconciliation.
4. **Real fuzzy search:** scored non-contiguous matches, deterministic relevance ranking, explicit case/Unicode behavior, and performance regression tests.
5. **Durability and polish:** atomic, versioned index persistence; exclusions and settings; stale-index indicators, platform-specific testing, installers, and size/latency measurements.

**Privacy and permissions:** volume-level NTFS enumeration may need privileged access. The egui process should not require administrator privileges; any future privileged helper must be opt-in, narrowly scoped, and securely separated from the UI. A normal directory-scanning fallback must remain usable. File-name indexing is not file-content indexing.

See [AGENTS.md](AGENTS.md) for detailed architecture boundaries, indexing correctness, concurrency, testing, and agent instructions.

## Known limitations

- There is no true fuzzy, persistent index, file watcher, configurable case policy, native folder picker, or exclusion/settings persistence.
- Native-path display or text copy may be lossy for malformed Unicode paths; scanning, matching, and file operations retain native path values.
- Scan completeness is best-effort under concurrent filesystem changes. Partially inaccessible subtrees are reported; a completed session snapshot can become stale.
- Scanning and cancellation are cooperative. Very large scans still consume memory proportional to discovered entries; the 20,000-row UI cap does not change that.
- On Linux, “reveal” opens the containing folder without guaranteed item selection. The current Windows item-selection API path has a 259 UTF-16-unit limit.
- Platform-specific behavior needs further real-device validation. Earlier development notes reported local Windows testing on 2026-10-11, but that does not by itself validate the current head on Linux/macOS or establish benchmark numbers.

## Development

To check **all three crates** (root defaults otherwise select only the CLI):

```bash
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --release --locked
```

```text
.github/workflows/rust.yml
```

The workflow defines corresponding Windows, Linux, and macOS jobs. These commands are recommended checks; the README does not assert that they were executed during a documentation-only change.

Please follow [AGENTS.md](AGENTS.md) for agent/contributor constraints and update the English and Chinese READMEs together whenever public behavior changes.

## License

No `LICENSE` file was present in the inspected repository. **No open-source license is implied** by the repository being publicly readable. Choose and add an explicit license before publishing redistribution terms or releases.

---

*FSearch — find what you need, without the clutter.*
