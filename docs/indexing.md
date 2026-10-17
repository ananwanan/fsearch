# Indexing foundation

## Scope and acceptance

The existing CLI retains its scanner, four literal matching semantics, order and exit codes. The desktop now uses an independent `fsearch-core::IndexSnapshot`, without egui/clap dependencies in core. Acceptance covers bounded batched construction and publication, explicit incomplete outcomes, same-root fallback, generation/cancellation races, and repeatable first-build/query/memory measurements. No persistence, watchers, NTFS backend, fuzzy search or new dependencies are added.

## Identity, revisions and query contract

`IndexedEntry` retains one native `PathBuf`, kind and `EntryId { generation, ordinal }`. The caller assigns a distinct build generation; ordinals identify observations within it. A hard link name is a separate observation. IDs do not survive rebuilds or represent stable filesystem objects. Native values remain authoritative; lossy display conversion happens for visible rows only.

`build_index` streams filesystem observations directly into immutable batches. Default batches hold 2,048 entries. `IndexSnapshot` revisions increase on publication, including a final health-only publication and an empty tree. Entries are shared through a persistent fanout-32 tree: publication copies at most 32 pointers per tree level rather than cloning earlier paths or an ever-growing batch table. Final publication is informational; cooperative cancellation is checked before it.

`query_with` reads the snapshot only. Four literal/case-sensitive/native matching modes are unchanged. It counts every match in that snapshot, selects the global first native-path rows with a bounded heap, and sorts only the selected rows (20,000 in desktop). Equal native paths break ties by ordinal. Query cancellation yields no partial count. Snapshot ordinals stay valid in later revisions of the same build, but desktop results must match the exact published revision before display.

## State and rebuild protocol

- `Building`: published preview; always incomplete.
- `Current`: traversal finished without observed errors. It is a best-effort completed scan, without any continuing freshness guarantee.
- `Partial`: child errors, cooperative cancellation or a declared build limit. The full issue count and explicit stop reason are retained even when error details are sampled.
- Desktop stale state is independent of snapshot completeness. Starting a same-original-root rebuild marks the old snapshot stale; it remains queryable. A failed, cancelled or incomplete replacement keeps that old snapshot. Starting another raw root spelling clears it. A first partial build can remain usable without claiming completeness.

Progress and result events have separate build/query generations. Results additionally carry snapshot generation/revision. The eight-event queue has backpressure; cancellation, superseding requests or shutdown release stale senders. One scan and one latest query occupy the replaceable mailbox. Query changes never cancel a build. During rebuilds, the background worker services queries against the retained snapshot between filesystem operations; an OS call already blocked cannot be interrupted.

The UI preserves warnings while queries run, rejects queued previews after cancellation, and remembers build cancellation even if a query resumes before its queued completion arrives. Retired snapshot references are handed back to the worker for deallocation. A superseded terminal event does not discard an applicable worker fallback.

## Allocation policy

| Per-build default | Limit | On exhaustion |
| --- | ---: | --- |
| Retained entries | 1,000,000 | Stop; Partial / EntryLimit |
| Retained native path capacity | 128 MiB | Stop; Partial / PathBytesLimit |
| Queued directory paths | 16,384 | Stop; Partial / PendingDirectoryLimit |
| Queued native directory path capacity | 8 MiB | Stop; Partial / PendingPathBytesLimit |
| Error details | 64 | Keep full count, omit excess details |
| Retained diagnostic path capacity | 256 KiB | Keep full count, omit excess details |
| Result ordinals | 20,000 | Continue counting all snapshot matches |

Desktop startup flags `--max-index-entries N` and `--max-index-path-bytes N` override the first two limits. Core callers can supply `IndexLimits` for all limits; batch size is normalized to 1..=16,384. A zero entry/path budget still permits an actually empty directory to complete. Exactly reaching the entry limit is complete only if there is no further candidate.

Path accounting uses native retained `PathBuf` capacities after compaction; entry structures are separately bounded by entry count. `entry_storage_bytes` is a lower bound, not measured RAM. Additional allocations include tree nodes, root, frontier vector, sampled errors, query heaps/results, bounded queued snapshot generations, the allocator, and OS buffers. A rebuild normally retains old and new builds simultaneously. Published revisions share entries; queued events from superseded builds can retain other generations until drained. These are explicit allocation bounds, not a hard process working-set or OOM guarantee. The compatible CLI still accumulates its original unbounded `ScanReport`.

The builder avoids following observed links/reparse points and rechecks queued directories before opening them, matching the portable scanner policy. Path-based checks and opens are not atomic no-follow operations. Concurrent filesystem changes can still make any completed observation obsolete.

## Validation boundaries

Core tests cover native identities (including hard links and malformed Unicode), literal queries, stable global selection/counts, tree growth and sharing, final short batches, budgets, deleted entries, structured diagnostics, cancellation, and no-disk cached queries. Desktop tests cover same/different scope reuse, partial rebuilds, query replacement, scan/query cancellation, snapshot revisions and saturated event delivery. See [performance baseline](index-baseline.md) for actual measurements and reproducible commands. Windows host evidence does not prove Linux/macOS GUI behavior, network/long-path behavior, or million-entry workloads.

## Observed validation on 2026-10-17

Windows 11 x64 / Rust 1.99.0 local checks passed:

- `cargo fmt --all -- --check`
- `cargo test --workspace --all-targets --locked`: 87 tests (44 core, 32 desktop, 11 CLI); benchmark example target also compiles.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo build --workspace --release --locked`
- `git diff --check`

The standalone release benchmark ran the generated 50,200-entry fixture and an explicitly selected existing repository build tree, with live assertions for cancellation, limits, capped counts and queries after removing the original fixture. Exact workloads, memory counters, environment and raw results are in the companion baseline report.

Release desktop smoke verification used a newly owned 20-file CJK fixture and a three-entry startup budget. The UI displayed a partial index with three retained matches, the settings explained its budget, and temporarily renaming the fixture made refresh fail while preserving the old snapshot. A subsequent query reduced the rows without clearing stale/failure warnings. Selecting the renamed directory built a new-generation partial snapshot and cleared the old warning. Native path sorting, CJK display and Ctrl+L/Ctrl+R were exercised in that flow.

This turn did not re-verify drag/drop, high DPI, all themes, open/reveal, long/network paths, or Linux/macOS runtime/CI. Build cancellation and generation races were validated by deterministic worker/model tests and the core harness rather than timing-dependent GUI clicks. The measured memory is for the core harness, not a desktop-process peak.
