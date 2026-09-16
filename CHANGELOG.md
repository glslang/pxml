# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **A repeatable benchmark suite** (`cargo bench`, `benches/throughput.rs`, built
  on `divan`). Four document shapes (many tiny records, large records,
  attribute-heavy, entity-heavy) across Phase A alone, the resident drivers, the
  sequential baseline, the streaming pipeline, compressed input, a thread sweep,
  and the streaming sizing knobs — all reported as bytes/s over the uncompressed
  document. `examples/bench.rs` stays as the exploratory driver (corpus
  generation, real files). Per-shape reference numbers are in `DECISIONS.md` §19.
- **Streaming pipeline sizing knobs** on `Config`, plus
  `StreamReader::with_config` / `StreamReader::config` to use them:
  `with_stream_batch_records` (default 256), `with_stream_batch_bytes`
  (default 1 MiB — a batch now closes on whichever cap comes first, so large
  records cannot turn the record cap into a large allocation),
  `with_stream_queue_capacity` and `with_stream_workers` (both default to
  deriving from `rayon`'s current pool). `with_stream_workers` gives streaming an
  explicit worker count rather than only inheriting the ambient pool.
- **`ChunkIndex::prelude_arc`** — a cheap `Arc<Prelude>` clone for keeping the
  shared prolog context after the index is gone. `ChunkIndex` is now `Clone`.

### Changed

- **The Phase A scan is memoized per document.** `index()` and the
  `par_for_each` / `map_collect` / `try_*` drivers share one `ChunkIndex`, so
  indexing and then parsing no longer scans the buffer twice. `with_config`
  drops the memo (a new record path frames different records). Measured 1.16×
  (tiny records) to 1.98× (attribute-heavy) faster on an already-indexed
  document.
- **Breaking:** `ParallelXml::index` returns `Result<&ChunkIndex, XmlError>`
  instead of `Result<ChunkIndex, XmlError>`, so reuse costs nothing. Bind the
  document to a variable before calling it
  (`let doc = ParallelXml::from_bytes(…); let idx = doc.index()?;`) and `clone()`
  the index if it must outlive the document.
- **Breaking:** `ChunkIndex::prelude` returns `&Prelude` instead of
  `&Arc<Prelude>`; use `prelude_arc()` when you want the `Arc`.
- **The streaming consumer no longer goes through `rayon`'s `par_bridge`.** A
  fixed set of worker threads pulls batches off the channel directly (one lock
  per batch, never held while parsing): ~2× faster on entity-heavy records, within
  noise elsewhere, and a closure that itself uses `rayon` can no longer contend
  with the pipeline for pool threads. The default worker count still comes from
  `rayon::current_num_threads()`, so `pool.install(…)` sizes the pipeline as
  before. See `DECISIONS.md` §22.

## [0.2.0] — 2026-09-13

### Changed

- Upgrade `quick-xml` from 0.41 to 0.42 and adapt parsing to its UTF-8 text events.
- **Breaking:** the `QName` exposed by `Event::Start` and `Event::End` now
  returns `&str` from `as_ref()` instead of `&[u8]`. Compare names with string
  literals (for example, `name.as_ref() == "trade"` instead of
  `name.as_ref() == b"trade"`). Use `name.as_ref().as_bytes()` when bytes are
  needed. Attribute keys remain `&[u8]`.

### Maintenance

- Add automatic merging for eligible Dependabot updates after CI passes.

## [0.1.0] — 2026-07-21

Initial release.

**Minimum supported Rust version: 1.88** (edition 2024 plus let-chains, which
stabilized in 1.88). Pre-release docs claimed 1.85; that was never accurate — the
scanner does not compile on 1.85–1.87. CI now verifies the declared MSRV.

### Added

- **Two-phase parallel parsing.** A single-threaded boundary scan (Phase A)
  frames a document's uniform records and captures shared prolog context, then
  a per-record parse (Phase B) runs in parallel on `rayon`.
- **`ParallelXml`** — the resident entry point over a `Vec` or an `mmap`'d file,
  with the `par_for_each` / `map_collect` drivers and their fallible
  `try_par_for_each` / `try_map_collect` counterparts. `map_collect` restores
  document order regardless of completion order.
- **`ParallelXml::index`** — Phase A only, exposing record count and byte ranges
  without parsing.
- **Record paths** — frame the children of a nested container rather than the
  root's direct children, skipping non-matching siblings. Set via
  `Config::with_record_path` on the resident path, and `StreamReader::record_path`
  on the streaming one (that type takes no `Config`). Deliberately a single
  place per reader: an earlier design also had `ParallelXml::record_path`, which
  a later `with_config` would silently discard, framing the wrong records with
  no error.
- **`Config`** — built with chained `with_parallel_threshold`,
  `with_min_records`, and `with_record_path`, and read back with the matching
  getters. The fields are private, so a later release can add a knob without
  breaking callers.
- **`StreamReader`** — a bounded-memory pipeline (producer thread frames,
  `rayon` parses, backpressured channel between them) for documents too large to
  materialize. Records are owned and arrive unordered.
- **`SeqReader`** via `ParallelXml::sequential` — a classic whole-document StAX
  cursor for consumers whose records are not order-independent.
- **Transparent zstd decompression** behind the default `zstd` feature;
  `from_path` detects the zstd magic number. Build with
  `--no-default-features` for a pure-Rust dependency tree.
- **`memchr-framer`** — an opt-in feature swapping the streaming framer's scan
  strategy; can help documents with large text/CDATA spans.
- **Error provenance** — per-record failures surface as
  `XmlError::RecordError { index, source }`, carrying the failing record's
  position. External DTDs and parameter entities are rejected with
  `XmlError::UnsupportedDtd`; non-UTF-8 input with `XmlError::Encoding`.

[Unreleased]: https://github.com/glslang/pxml/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/glslang/pxml/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/glslang/pxml/releases/tag/v0.1.0
