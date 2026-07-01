# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`pxml` is a parallel, StAX-style (pull) XML reader for Rust (edition 2024, Rust 1.85+),
specialized for one document shape: a single root containing thousands of
**uniform, order-independent records** (e.g. `<trades><trade>…</trade>…</trades>`).
The records may be the root's direct children (default) or the children of a
nested container named via `Config::record_path` (e.g. the `<object>`s in
`<root><manifest/><objects><object/>…</objects></root>`).
It is a library crate, not a binary.

## Commands

```sh
cargo build
cargo test                              # full unit + property suite (default features)
cargo test --no-default-features        # without the C-backed zstd dependency
cargo test --features memchr-framer     # opt-in memchr/memmem streaming framer

cargo test scan::                       # run one module's tests (also: parse::, tests:: for lib)
cargo test test_name_substring          # run a single test by (sub)name

cargo run --release --example bench                       # in-memory throughput sweep (release is essential)
cargo run --release --example bench -- 500000 1,4,8       # explicit record count + thread list
cargo run --release --example bench -- gen 1000000 trades.xml.zst  # generate a zstd test file
cargo run --release --example bench -- file trades.xml.zst         # resident vs streaming on a real file
```

Tests live inline (`#[cfg(test)]` modules) in each source file, not a separate `tests/` dir.
Property tests use `proptest`; regression seeds are checked in under `proptest-regressions/`
— do not delete them.

## Architecture: two-phase scan-then-parse

The core idea (full rationale in `DESIGN.md`, decisions/trade-offs in `DECISIONS.md`):
XML cannot be split at an arbitrary byte offset because a `<`/`>` may sit inside an
attribute value, comment, CDATA, PI, or DTD. So work is split in two:

- **Phase A — boundary scan (`src/scan.rs`, single-threaded).** A hand-written
  `memchr`-driven state machine walks the buffer once to find the record
  boundaries — the direct children of the root, or, when `Config::record_path`
  is set, of a container reached by descending that element-name path (skipping
  non-matching siblings). It builds no tree, decodes no entities, and validates
  only structural well-formedness (depth, matching root end-tag, whitespace-only
  text between records/descent-level siblings). It also captures the shared
  **prelude** (encoding, root name/namespaces plus any descended container's
  namespaces, internal-subset `<!ENTITY>` definitions). Output is a `ChunkIndex`:
  a `Range<usize>` per record plus an `Arc<Prelude>`. This is the irreducible
  sequential fraction and is memory-bandwidth bound.
- **Phase B — per-record parse (`src/parse.rs`, parallel on rayon).** Each record's
  byte slice is handed to a worker running a normal `quick-xml` `Reader` over *just
  that slice*, seeded with the shared `Prelude` so entity expansion is correct in
  isolation. Workers are fully independent — this is what makes the
  "records are order-independent" assumption sound.

### Module map

- `src/lib.rs` — public entry point. `ParallelXml` (owns the buffer: `Vec`/`Cow` or
  `mmap`), the `par_for_each` / `map_collect` / `try_*` drivers, the small-input
  sequential fallback, `Record`, `SeqReader` (whole-document StAX escape hatch),
  and the `XmlError` enum.
- `src/scan.rs` — Phase A. The bulk of the complexity and tests lives here.
- `src/parse.rs` — Phase B. `RecordReader` pull cursor + `map_event` (quick-xml
  `Event` → pxml `Event`, with entity decoding).
- `src/event.rs` — public `Event`, `Attrs`/`Attribute` attribute iteration.
- `src/prelude.rs` — `Prelude`, `Encoding`, `NamespaceContext` (shared immutable context).
- `src/stream.rs` — `StreamReader`: bounded-memory pipeline (producer thread frames +
  rayon parses with a backpressured channel); records are **owned and unordered**.
- `src/config.rs` — `Config` (`parallel_threshold`, `min_records`).

### Two execution paths — keep them in mind when changing behavior

1. **Resident** (`ParallelXml::from_path`/`from_bytes`/`from_zstd_*`): whole document
   in memory; workers borrow slices; supports ordered `map_collect`. Below
   `Config::parallel_threshold` bytes **or** `Config::min_records` records, the
   drivers transparently fall back to a single sequential pass — a behavior any change
   to the drivers must preserve.
2. **Streaming** (`StreamReader`): constant memory, but records are owned (copied) and
   results arrive unordered.

## Behavioral invariants to preserve

- **Lending pull cursors:** `next_event()` borrows the reader, so an event must be
  consumed/copied before the next call. This is what keeps parsing zero-copy.
- **Lexical correctness in Phase A:** comments / CDATA / PIs / DTD must be skipped so a
  record-lookalike `<trade>` inside them never mis-frames a record. Any change to the
  scanner state machine needs property-test coverage.
- **`record_path` container descent:** the framing rule is generalized around
  `target = path.len() + 1` in both the resident scanner (`scan_content`) and the
  streaming framer (`on_tag_complete`), and must stay behavior-equivalent to the old
  depth-1 framing when `target == 1` (empty path). The streaming framer and resident
  scanner must frame the *same* records under a path (property-tested); a skipped
  non-matching sibling subtree must stay carry-bounded in streaming.
- **Error provenance:** per-record (Phase B) failures surface as
  `XmlError::RecordError { index, source }`; the `try_*` drivers carry user closure
  errors the same way. Keep the `index` accurate.
- **Rejected, not silently skipped:** external DTDs / parameter entities →
  `XmlError::UnsupportedDtd`; non-UTF-8 / UTF-16 BOM → `XmlError::Encoding`.
- **Feature gating:** `zstd` is default-on but optional (pure-Rust build via
  `--no-default-features`); `memchr-framer` swaps the streaming framer's scan strategy.
  Anything touching these must compile and test under all three feature combinations
  shown in Commands.

## Docs

`DESIGN.md` is the pre-implementation feasibility spec; `DECISIONS.md` records what was
actually built (Context → Decision → Why → Consequences) and **supersedes** `DESIGN.md`
where they differ — notably streaming, which `DESIGN.md` listed as out of scope.
