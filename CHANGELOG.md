# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/glslang/pxml/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/glslang/pxml/releases/tag/v0.1.0
