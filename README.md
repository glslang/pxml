# pxml

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

```
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
pxml = { path = "." } # or a version once published
```

Requires a toolchain with **edition 2024** support (Rust 1.85+).

```rust
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
                            let attr = attr.unwrap();
                            // attr.key: &[u8], attr.value: Cow<str> (entity-decoded)
                        }
                    }
                }
                Event::Text(text) => { /* … */ }
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
// `doc: ParallelXml`, inside a function returning `Result<_, XmlError>`
let values: Vec<u64> = doc.map_collect(|record| {
    // parse the record and return a typed value
    record.index() as u64
})?;
```

### Just the framing

`index()` runs Phase A only — cheap, and exposes the record count and byte ranges
without parsing anything:

```rust
let idx = doc.index()?; // Phase A only
println!("{} records", idx.len());
```

### Compressed input

With the default `zstd` feature, `from_path` transparently decompresses a
zstd-compressed document (detected by its magic number); plain XML is mmap'd as
usual. For in-memory or streamed compressed data:

```rust
use std::fs::File;

let doc = ParallelXml::from_zstd_bytes(&compressed)?;        // &[u8]
let doc = ParallelXml::from_zstd_reader(File::open(path)?)?; // any Read
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

```rust
use pxml::StreamReader;
use std::fs::File;

StreamReader::from_zstd_reader(File::open("trades.xml.zst")?)?
    .par_for_each(|record| {
        // drive record.events(); results arrive unordered
    })?;
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
| `Config` | Tuning: `parallel_threshold`, `min_records`. |
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

```rust
use pxml::{Config, ParallelXml};

let doc = ParallelXml::from_bytes(bytes).with_config(Config {
    parallel_threshold: 1 << 20, // 1 MiB
    min_records: 32,
    ..Config::default()
});
```

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
- **Namespaces** — `xmlns` / `xmlns:prefix` on the root are captured into the
  shared `Prelude` (see Limitations for how they're surfaced).
- **Entities** — internal-subset `<!ENTITY>` definitions are captured in Phase A
  and resolved (alongside the predefined XML entities) when decoding text and
  attribute values. External DTDs and parameter entities are **rejected** with
  `XmlError::UnsupportedDtd` rather than silently skipped.
- **Comments, CDATA, PIs** — correctly skipped during framing, so
  record-lookalike text inside them never mis-frames a record. CDATA is surfaced
  raw; comments and PIs are not surfaced as events.
- **Well-formedness** — Phase A checks depth, that the root end tag's name
  matches the root, and that only whitespace appears directly under the root
  (non-whitespace text between records is rejected). Nested-element name matching
  is delegated to the per-record `quick-xml` parse; per-record parse errors carry
  the record's `index`.
- **Fallible record work** — `try_par_for_each` / `try_map_collect` take closures
  returning `Result`, surfacing failures as `XmlError::RecordError { index, .. }`.
- **Compressed input** — zstd-compressed documents are transparently
  decompressed into memory (default `zstd` feature).

## Limitations

v1, by design (see [`DESIGN.md`](DESIGN.md) for the full non-goals):

- **Lexical namespaces.** Element/attribute names are surfaced as written
  (`QName`, prefix intact). Root-declared namespaces are captured in
  `Prelude::namespaces` for manual resolution, but are not auto-applied per
  event.
- **Whole document resident** on the `ParallelXml` path — workers need random
  access to their slices, so the document is read into a `Vec` or `mmap`'d. Use
  [`StreamReader`](#streaming-bounded-memory) for bounded-memory parallel
  parsing (at the cost of unordered, owned records).
- **No external DTDs / parameter entities**, and no schema/DTD validation.
- **Sequential Phase A.** The boundary scan is single-threaded (a speculative
  parallel scan is a possible future optimization).
- Parallelism is at depth 1 only; nested content within a record is parsed
  sequentially (fine for the uniform-records target).

## Development

```sh
cargo test     # 34 unit tests across scan / parse / lib
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
