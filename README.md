# pxml

[![crates.io](https://img.shields.io/crates/v/pxml.svg)](https://crates.io/crates/pxml)
[![docs.rs](https://docs.rs/pxml/badge.svg)](https://docs.rs/pxml)
[![CI](https://github.com/glslang/pxml/actions/workflows/ci.yml/badge.svg)](https://github.com/glslang/pxml/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/glslang/pxml/graph/badge.svg)](https://codecov.io/gh/glslang/pxml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org)
[![edition 2024](https://img.shields.io/badge/edition-2024-orange.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/index.html)

A parallel, **StAX-style (pull) XML reader** for Rust, built for one shape of
document: a single root containing **thousands of uniform, order-independent
records** — e.g. `<trades><trade>…</trade>…</trades>`.

`pxml` frames the top-level records with one cheap sequential pass, then parses
them **in parallel** on a [`rayon`] pool. The soundness assumption is that the
direct children of the root are independent and may be consumed in any order.

> Status: v1. The full architecture from [`DESIGN.md`](DESIGN.md) is implemented
> and tested. See [Limitations](#limitations) for the honest caveats, and
> [`DECISIONS.md`](DECISIONS.md) for the design decisions, trade-offs, and
> benchmark analysis behind the implementation.

## Why

A single linear `next_event()` cursor cannot be advanced by many threads — XML
events are inherently ordered and stateful. And you cannot cut the byte buffer
at an arbitrary offset and resume parsing, because a `<` or `>` may sit inside an
attribute value, comment, CDATA section, or processing instruction.

`pxml` resolves both problems with a **two-phase, scan-then-parse** design:

```text
            ┌─────────────────────── whole document (Vec or mmap) ──────────────────────┐
Phase A     │ <?xml?> <!DOCTYPE…> <trades>  <trade>…</trade> <trade>…</trade>  </trades> │
(sequential)│ └──────── prelude ────────┘   └── record 0 ──┘ └── record 1 ──┘            │
            └───────────────────────────────────────────────────────────────────────────┘
                                                 │ byte ranges + shared prelude
                                                 ▼
Phase B (parallel, rayon): record 0 ─▶ worker        record 1 ─▶ worker        …
                           each runs quick-xml over just its slice
```

- **Phase A** walks the buffer once with a tiny `memchr`-driven state machine,
  finding depth-1 element boundaries and capturing shared prolog context
  (encoding, root namespaces, internal-subset `<!ENTITY>` definitions). It builds
  no tree and decodes no entities — it is memory-bandwidth bound.
- **Phase B** hands each record's slice to a worker that runs a normal
  [`quick-xml`] reader over *just that slice*, seeded with the shared prelude so
  entity expansion is correct in isolation. Workers are fully independent.

## Quick start

```toml
[dependencies]
pxml = "0.1"
```

Requires **Rust 1.88+** (edition 2024, plus let-chains used by the scanner).

```rust,no_run
use pxml::{Event, ParallelXml};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // mmap a file (or `ParallelXml::from_bytes(...)` for in-memory data)
    let doc = ParallelXml::from_path(Path::new("trades.xml"))?;

    // Unordered parallel pass — workers fire as records complete.
    doc.par_for_each(|record| {
        let mut events = record.events();
        while let Some(ev) = events.next_event().unwrap() {
            match ev {
                Event::Start { name, attrs } => {
                    if name.as_ref() == b"trade" {
                        for attr in attrs.iter() {
                            // key: &[u8], value: Cow<str> (entity-decoded)
                            let attr = attr.unwrap();
                            println!("{:?} = {}", attr.key, attr.value);
                        }
                    }
                }
                Event::Text(text) => println!("text: {text}"),
                _ => {}
            }
        }
    })?;
    Ok(())
}
```

### Ordered results

`map_collect` runs in parallel but slots results back into **document order**:

```rust
# use pxml::ParallelXml;
# let doc = ParallelXml::from_bytes(&b"<rs><r>a</r><r>b</r></rs>"[..]);
let values: Vec<u64> = doc.map_collect(|record| {
    // parse the record and return a typed value
    record.index() as u64
})?;
assert_eq!(values, [0, 1]);
# Ok::<(), pxml::XmlError>(())
```

### Just the framing

`index()` runs Phase A only — cheap, and exposes the record count and byte ranges
without parsing anything:

```rust
# use pxml::ParallelXml;
# let doc = ParallelXml::from_bytes(&b"<rs><r>a</r><r>b</r></rs>"[..]);
let idx = doc.index()?; // Phase A only
println!("{} records", idx.len());
assert_eq!(idx.records()[0], 4..12); // byte range of `<r>a</r>`
# Ok::<(), pxml::XmlError>(())
```

### Records under a nested container

By default the records are the root's direct children. When they instead live
inside a wrapper element — alongside siblings that should be ignored — name the
container with `Config::with_record_path`. `pxml` skips the non-matching
siblings, descends into the container (accumulating any `xmlns` it declares),
and frames its direct children:

```rust
# use pxml::{Config, ParallelXml};
let xml = b"<root><manifest>meta</manifest>\
            <objects><object/><object/></objects></root>".to_vec();

// skip <manifest>, frame each <object>
let config = Config::new().with_record_path(["objects"]);

let doc = ParallelXml::from_bytes(xml).with_config(config);
assert_eq!(doc.index()?.len(), 2);
# Ok::<(), pxml::XmlError>(())
```

The path may descend several levels (`.with_record_path(["body", "objects"])`),
and the children of *every* matching container are framed. An empty path (the
default) means the root itself.

The streaming reader takes no `Config` — the parallelism thresholds only apply
to the resident path — so it sets the path directly:
`StreamReader::from_reader(r).record_path(["objects"])`.

The container's namespace declarations are merged into the one shared `Prelude`
context (see [Limitations](#limitations) for the multi-container caveat).

### Compressed input

With the default `zstd` feature, `from_path` transparently decompresses a
zstd-compressed document (detected by its magic number); plain XML is mmap'd as
usual. For in-memory or streamed compressed data:

```rust,no_run
# // Gated so this block still compiles with --no-default-features.
# #[cfg(feature = "zstd")]
# fn demo() -> Result<(), pxml::XmlError> {
use pxml::ParallelXml;
use std::fs::File;

# let compressed: Vec<u8> = Vec::new();
# let path = "trades.xml.zst";
let doc = ParallelXml::from_zstd_bytes(&compressed)?;        // &[u8]
let doc = ParallelXml::from_zstd_reader(File::open(path)?)?; // any Read
# Ok(())
# }
```

The whole document is decompressed up front (workers need random access to
their slices), so decompression is sequential and adds to the serial fraction.
Build with `default-features = false` for a pure-Rust crate without the
C-backed `zstd` dependency.

### Streaming (bounded memory)

`from_path` / `from_bytes` materialize the document — fine for a single file via
mmap, but a problem for a multi-GB *compressed* file (you can't mmap the
decompressed form), or for many large files at once. `StreamReader` runs the
pipeline without holding the whole document: a single producer thread
decompresses and frames records incrementally, and a `rayon` pool parses them in
parallel, with a bounded channel providing backpressure. Resident memory is
bounded by the in-flight records (≈ `threads × record size`) plus one chunk —
**independent of document size**.

```rust,no_run
# // Gated so this block still compiles with --no-default-features.
# #[cfg(feature = "zstd")]
# fn demo() -> Result<(), Box<dyn std::error::Error>> {
use pxml::StreamReader;
use std::fs::File;

StreamReader::from_zstd_reader(File::open("trades.xml.zst")?)?
    .par_for_each(|record| {
        // drive record.events(); results arrive unordered
        let mut events = record.events();
        while let Some(ev) = events.next_event().unwrap() {
            // …
        }
    })?;
# Ok(())
# }
```

`from_reader(impl Read)` streams an already-decompressed source. Records are
framed and parsed in batches (one arena allocation each), which keeps the
producer→worker handoff cheap. The trade-offs vs. the resident path: output is
**unordered** and records are **owned** (copied out of the decode buffer rather
than borrowed). In exchange you get constant memory — and, for large documents,
often *better* throughput, because the pipeline overlaps decompression with
parsing and keeps each batch cache-resident instead of materializing the whole
document. On a 2M-record / 184 MiB-decompressed file the streaming path measured
~2.2× faster than `from_path`; see [`DECISIONS.md`](DECISIONS.md) §15.

## API at a glance

| Type | Purpose |
|------|---------|
| `ParallelXml` | Owns the buffer (`Vec` or `mmap`) + `Config`; entry point. |
| `Config` | Tuning: `parallel_threshold`, `min_records`, `record_path`. |
| `ChunkIndex` | Phase A output: per-record byte ranges + shared `Prelude`. |
| `Prelude` | Immutable shared context: encoding, root name, namespaces, entities. |
| `StreamReader` | Bounded-memory streaming pipeline over a `Read` / zstd source. |
| `Record` | One top-level record; `events()` returns a pull cursor, `index()` its position. |
| `RecordReader` / `SeqReader` | StAX pull cursors (`next_event()`). |
| `Event` | `Start { name, attrs }` · `End { name }` · `Text(Cow<str>)` · `Cdata(&[u8])`. |
| `Attrs` / `Attribute` | Iterate a start tag's attributes (key + entity-decoded value). |
| `XmlError` | `Malformed(pos)` · `Encoding` · `Io` · `UnsupportedDtd` · `RecordError { index, source }`. |

`SeqReader` (via `doc.sequential()`) is a classic whole-document StAX reader —
the escape hatch for consumers who don't want the record model.

> **Pull cursors are lending:** `next_event()` borrows the reader, so process (or
> copy out of) each event before requesting the next. This keeps parsing
> zero-copy where possible.

## Configuration & the small-input fallback

Below `Config::parallel_threshold` bytes **or** `Config::min_records` records,
both `par_for_each` and `map_collect` transparently run a sequential pass — the
thread-pool and indexing overhead doesn't repay itself on small inputs.

`Config` is built with chained `with_*` methods:

```rust
use pxml::{Config, ParallelXml};

# let bytes = b"<rs><r>a</r></rs>".to_vec();
let config = Config::new()
    .with_parallel_threshold(1 << 20) // 1 MiB
    .with_min_records(32);

let doc = ParallelXml::from_bytes(bytes).with_config(config);
```

Its fields are private, so a future release can add a knob without breaking
callers. Read them back with the matching getters (`config.min_records()`).

Defaults: `parallel_threshold = 4 MiB`, `min_records = 64`.

## Performance

Expect **sub-linear** scaling, not Nx. Phase A is the irreducible sequential
fraction, and both phases are ultimately memory-bandwidth bound. Realistic gains
are **~3–6×** wall-clock on large files (hundreds of MB) with substantial
per-record work, with diminishing returns past ~8 cores. Light records
(small fields) bottleneck on bandwidth sooner and scale less.

Run the included benchmark (release is essential):

```sh
cargo run --release --example bench                 # 200k records, auto thread sweep
cargo run --release --example bench -- 500000 1,4,8 # 500k records, explicit threads

cargo run --release --example bench -- gen 1000000 trades.xml.zst  # write a .zst
cargo run --release --example bench -- file trades.xml.zst         # resident vs streaming
```

The in-memory mode prints a sequential baseline and `par_for_each` across thread
counts (throughput + speedup) plus a small-input fallback demonstration. The
`file` mode compares the resident `from_path` path against the streaming
`from_zstd_reader` path on a real file. See [`DECISIONS.md`](DECISIONS.md) §15 for
measured numbers and analysis (notably: with batching, streaming is both
bounded-memory *and* ~2.2× faster than resident on a large file).

## What's handled

- **Encoding / BOM** — UTF-8 (with or without BOM) is asserted up front; a UTF-16
  BOM or a non-UTF-8 declared encoding is rejected as `XmlError::Encoding`.
- **Namespaces** — `xmlns` / `xmlns:prefix` on the root (and on any container
  descended into via `record_path`) are captured into the shared `Prelude` (see
  Limitations for how they're surfaced).
- **Entities** — internal-subset `<!ENTITY>` definitions are captured in Phase A
  and resolved (alongside the predefined XML entities) when decoding text and
  attribute values. External DTDs and parameter entities are **rejected** with
  `XmlError::UnsupportedDtd` rather than silently skipped.
- **Comments, CDATA, PIs** — correctly skipped during framing, so
  record-lookalike text inside them never mis-frames a record. CDATA is surfaced
  raw; comments and PIs are not surfaced as events.
- **Well-formedness** — Phase A checks depth, that the root end tag's name
  matches the root, and that only whitespace appears between sibling records and
  descent-level elements (non-whitespace text there is rejected; content inside a
  skipped sibling is not inspected). Nested-element name matching is delegated to
  the per-record `quick-xml` parse; per-record parse errors carry the record's
  `index`.
- **Fallible record work** — `try_par_for_each` / `try_map_collect` take closures
  returning `Result`, surfacing failures as `XmlError::RecordError { index, .. }`.
- **Compressed input** — zstd-compressed documents are transparently
  decompressed into memory (default `zstd` feature).

## Limitations

v1, by design (see [`DESIGN.md`](DESIGN.md) for the full non-goals):

- **Records must be order-independent.** This is the load-bearing assumption: a
  worker sees only its own record, so cross-record state — an accumulator, a
  lookup into an earlier record, anything positional beyond `record.index()` —
  is not available during the parallel pass. If your records are *not*
  independent, use `doc.sequential()` (a classic whole-document StAX cursor) and
  skip the parallelism.
- **`try_map_collect` does not short-circuit on the parallel path.** Building an
  ordered `Vec<T>` means visiting every record, so the closure still runs for
  records after a failing one, and when several fail, which error surfaces is
  not deterministic. Use `try_par_for_each` when you need early termination.
  (The sequential fallback *does* stop at the first error — so this differs
  between small and large inputs.)
- **Lexical namespaces.** Element/attribute names are surfaced as written
  (`QName`, prefix intact). Root- and container-declared namespaces are captured
  in `Prelude::namespaces` for manual resolution, but are not auto-applied per
  event. `Prelude::namespaces` is a single shared context, so if `record_path`
  matches **multiple** containers that redeclare the same prefix to *different*
  URIs, the merge is last-writer-wins (a non-issue for uniform containers; root
  and ancestor declarations are always correct).
- **Whole document resident** on the `ParallelXml` path — workers need random
  access to their slices, so the document is read into a `Vec` or `mmap`'d. Use
  [`StreamReader`](#streaming-bounded-memory) for bounded-memory parallel
  parsing (at the cost of unordered, owned records).
- **No external DTDs / parameter entities**, and no schema/DTD validation.
- **Sequential Phase A.** The boundary scan is single-threaded (a speculative
  parallel scan is a possible future optimization).
- Parallelism is at the record level — the root's direct children by default, or
  the children of a container named via `record_path`. Content nested *within* a
  record is parsed sequentially (fine for the uniform-records target).

## Development

```sh
cargo test     # full unit + property suite across scan / parse / lib
cargo build
```

Built on [`quick-xml`] (Phase B parsing), [`rayon`] (work-stealing pool),
[`memchr`] (delimiter scanning), [`memmap2`] (zero-copy file mapping), and
[`zstd`] (optional decompression).

```sh
cargo test --no-default-features   # build/test without the zstd C dependency
cargo test --features memchr-framer # opt-in memchr/memmem streaming framer (see DECISIONS.md §16)
```

## License

Licensed under the [MIT License](LICENSE).

[`quick-xml`]: https://crates.io/crates/quick-xml
[`rayon`]: https://crates.io/crates/rayon
[`memchr`]: https://crates.io/crates/memchr
[`memmap2`]: https://crates.io/crates/memmap2
[`zstd`]: https://crates.io/crates/zstd
