# pxml — a parallel, StAX-style XML reader for Rust

> **Status:** design + feasibility spec. No implementation yet. This document is
> the seed of a standalone crate (`pxml`). It is self-contained — read it top to
> bottom and you have everything needed to start building.

## Goal

An **event-based (StAX / pull) XML reader** that **parses in parallel**, under one
assumption: *top-level elements (direct children of the root) are independent and
may be consumed in any order.*

Primary target workload: a single root containing **thousands of uniform records**
— e.g. `<trades><trade>…</trade><trade>…</trade>…</trades>`. This is the best case
for parallelism: the unit of work (one top-level record subtree) is well-defined,
repeated, and order-independent.

---

## Part 1 — Feasibility study

### 1. What "StAX + parallel" means here

StAX = a **pull** parser: the consumer repeatedly asks for the next event
(`StartElement`, `EndElement`, `Text`, …) and the parser advances. Rust's de-facto
equivalent is `quick-xml`'s `Reader::read_event`. "Parallel StAX" cannot mean *one*
linear event cursor advanced by many threads — events are inherently ordered and
stateful. It must mean: **partition the document into independent event substreams
and parse those concurrently.** The "order doesn't matter" assumption is exactly
what makes this sound — each top-level record becomes its own mini event stream, and
records can be produced/consumed out of order.

So the realistic API is **not** "one parallel `next_event()`". It is a **parallel
iterator of per-record readers** (a `rayon` `ParallelIterator` of `Record`s), with
an optional ordered-collect mode.

### 2. The hard part: XML is not randomly splittable

You cannot cut the byte buffer at an arbitrary offset and start parsing, because a
`<` or `>` at that offset may be:

- inside an attribute value (`attr="a < b"`),
- inside a comment (`<!-- … -->`),
- inside CDATA (`<![CDATA[ <foo> ]]>`),
- inside a processing instruction (`<?…?>`) or the DTD internal subset.

From a cold offset you genuinely don't know which lexical state you're in. Pure
"split at N byte boundaries and resync" is therefore **unsafe in general** — XML has
no reliable self-synchronizing marker.

### 3. The approach that works: scan-then-parse (two phase)

**Phase A — sequential boundary scan (cheap, single-threaded).** Walk the buffer
once with a tiny state machine that tracks only enough to find **depth-1 element
boundaries**: in-tag vs in-text, quote state, and the special spans
(`<!-- -->`, `<![CDATA[ ]]>`, `<? ?>`, `<!DOCTYPE … >`). It does **not** build a
tree, decode entities, or validate. Output: a `(start, end)` byte range per
top-level record, plus the byte range of the **prolog / root-open prelude** (§5).

**Phase B — parallel full parse.** Hand each record's byte slice to a worker on a
`rayon` pool. Each worker runs a normal `quick-xml` reader over *just that slice*,
producing that record's events / a typed value. Workers are fully independent.

Why this shape is right:

- Phase A is **O(n) but does almost no work per byte** (state transitions on a few
  delimiter bytes); it's memory-bandwidth bound and `memchr`/SIMD-accelerable on the
  `<` / `>` / `"` / `'` delimiters.
- Phase B does the expensive work (entity decode, attribute parsing, UTF-8
  validation, building typed records) and is embarrassingly parallel.

### 4. Amdahl / expected speedup

Phase A is the irreducible sequential fraction. Because it only touches delimiter
bytes and tracks a handful of states, it is far cheaper than full parsing — on
typical record-dump XML the full parse is well over 10× the cost of the scan. That
keeps the serial fraction low enough for meaningful scaling, but **not** linear:

- Realistic expectation: **~3–6× wall-clock** on a many-core machine for large files
  (hundreds of MB), with diminishing returns past ~8 cores as Phase A and memory
  bandwidth dominate.
- For **small** documents the thread-pool + chunk-index overhead loses to a plain
  sequential `quick-xml` pass → need a size threshold (fall back to sequential under
  a few MB or under N records).
- Phase A can itself be parallelized later (speculative chunking + a verification
  pass), but that's a second-order optimization; start with a sequential scan.

### 5. Correctness constraints that survive even "independent records"

The independence assumption is *mostly* true but has real exceptions, handled by
**propagating root/prolog context** into every worker:

- **Encoding & BOM.** Declared once in `<?xml encoding=…?>`. Decode/normalize to
  UTF-8 up front (or assert UTF-8) before slicing. Slices stay on valid char
  boundaries because we only ever cut at `>`/`<` between records.
- **Namespaces.** `xmlns` / `xmlns:prefix` declared on the **root** (or prolog) apply
  to every record; a record parsed in isolation would resolve prefixes wrong. Fix:
  Phase A captures the root start-tag's namespace declarations into a shared,
  immutable `NamespaceContext`; each worker seeds its reader with it. Namespaces
  declared *inside* a record are local and need no sharing.
- **DTD internal subset / entity definitions.** `<!ENTITY …>` in the prolog is
  document-global; a worker that hits `&foo;` must know the definition. Fix: Phase A
  parses the internal subset once into a shared entity map; workers consult it.
  External DTDs / parameter entities → **out of scope** (`quick-xml` doesn't resolve
  external DTDs either).
- **`xml:lang` / `xml:base` / `xml:space`** inheritance from ancestors → captured in
  the same shared prelude context if needed.
- **Well-formedness across records** (e.g. mismatched root close) is checked in
  Phase A's depth tracking, not by the workers.

These are exactly the things a naive "just split and parse" gets wrong; scan-then-
parse absorbs them via the shared immutable prelude.

### 6. Memory model

Parallel workers need random access to their slices, so the design assumes the
**whole document is resident**: read fully into a `Vec<u8>`, or **`mmap`** the file
(`memmap2`) for zero-copy and OS-managed paging. True *streaming* (bounded memory) +
parallel is a much harder problem (pipeline a sequential splitter feeding a worker
queue) and is **out of scope for v1**. For the target "large file of records", mmap
is the pragmatic choice.

### 7. Reuse — what we should *not* write ourselves

- **`quick-xml`** — fast, zero-copy, StAX-style `read_event`. Use it for **Phase B**
  per-record parsing instead of writing a conformant XML tokenizer. Gives us
  namespace resolution (`NsReader`) and entity hooks. Biggest reuse win; a spec-
  correct XML lexer from scratch is the costly, bug-prone path we avoid.
- **`rayon`** — work-stealing pool + `ParallelIterator`; ideal for "map over N record
  slices, collect/reduce".
- **`memchr` / `memmap2`** — Phase A delimiter scanning and zero-copy file mapping.
- We **do** hand-write the small Phase A boundary scanner — it's deliberately *not*
  full XML, just depth-1 framing + prelude capture, and no crate exposes "give me the
  byte ranges of the top-level elements" cheaply.

### 8. Verdict

**Feasible and well-matched to the target workload.** Architecture: one cheap
sequential **boundary-scan** pass that frames top-level records and captures shared
prolog context, then **embarrassingly-parallel** per-record parsing on `rayon` via
`quick-xml`. The "any order" assumption is what makes it sound. Honest caveats:
speedup is sub-linear, bounded by scan + memory bandwidth (~3–6× typical, not Nx);
it needs the whole document resident; small docs fall back to sequential; and a few
document-global features (namespaces, internal entities, encoding) must be propagated
from the prolog rather than assumed away. None is a blocker.

---

## Part 2 — Design spec (ready to implement)

### Crate layout

```
pxml/
  Cargo.toml
  src/
    lib.rs        // public API: ParallelXml, Record, Event, Config
    scan.rs       // Phase A: boundary scanner -> ChunkIndex + Prelude
    prelude.rs    // shared immutable context: encoding, NamespaceContext, entity map
    parse.rs      // Phase B: per-record reader over a &[u8] slice (wraps quick-xml)
    event.rs      // StAX-style Event enum surfaced to consumers
    config.rs     // thresholds, ordered/unordered mode
  tests/
    fixtures/     // splitter stress cases (see verification)
  examples/
    bench.rs      // synthetic N-record benchmark
```

### `Cargo.toml`

```toml
[package]
name = "pxml"
version = "0.1.0"
edition = "2024"

[dependencies]
quick-xml = "0.37"
rayon     = "1"
memmap2   = "0.9"
memchr    = "2"

[dev-dependencies]
criterion = "0.5"  # optional, for examples/bench.rs

[[example]]
name = "bench"
```

### Core types

```rust
/// Immutable, shared across all workers. Built once in Phase A.
pub struct Prelude {
    encoding: Encoding,                       // resolved; buffer normalized to UTF-8
    root_name: Box<str>,
    namespaces: NamespaceContext,             // xmlns decls on root/prolog
    entities: HashMap<Box<str>, Box<str>>,    // internal DTD <!ENTITY> only
}

/// Phase A output: framing only, no parsing.
pub struct ChunkIndex {
    prelude: Arc<Prelude>,
    records: Vec<Range<usize>>,               // byte range per top-level element
}

/// StAX-style event surfaced to a record consumer (borrowed, zero-copy where possible).
pub enum Event<'a> {
    Start { name: QName<'a>, attrs: Attrs<'a> },
    End   { name: QName<'a> },
    Text(Cow<'a, str>),
    Cdata(&'a [u8]),
    // comments / PIs optional, filterable via config
}

/// One top-level record: a self-contained pull reader over its slice.
pub struct Record<'doc> {
    bytes: &'doc [u8],
    prelude: Arc<Prelude>,
    index: usize,                             // position in document order
}
impl<'doc> Record<'doc> {
    pub fn events(&self) -> RecordReader<'doc>;  // StAX pull cursor
    pub fn index(&self) -> usize;
}
```

### Public API surface

```rust
pub struct ParallelXml { /* owns buffer (Vec or Mmap) + Config */ }

impl ParallelXml {
    pub fn from_path(p: &Path) -> io::Result<Self>;                 // mmap
    pub fn from_bytes(b: impl Into<Cow<'static, [u8]>>) -> Self;
    pub fn with_config(self, cfg: Config) -> Self;

    /// Phase A only — cheap; exposes record count / framing.
    pub fn index(&self) -> Result<ChunkIndex, XmlError>;

    /// Unordered parallel map over records (the natural "any order" API).
    pub fn par_for_each<F: Fn(&Record) + Sync>(&self, f: F) -> Result<(), XmlError>;

    /// Parallel map + collect; preserves document order on output.
    pub fn map_collect<T: Send, F: Fn(&Record) -> T + Sync>(&self, f: F)
        -> Result<Vec<T>, XmlError>;

    /// Escape hatch: a plain sequential StAX reader over the whole doc
    /// (the small-input fallback, and classic-StAX users).
    pub fn sequential(&self) -> SeqReader<'_>;
}
```

- **Unordered consumption** → `par_for_each` (workers fire as records complete).
- **Ordered output** → `map_collect` (results slotted by record index).
- **Small-input fallback**: if `buffer.len() < cfg.parallel_threshold` or
  `records.len() < cfg.min_records`, both methods transparently run sequentially.

### Phase A scanner (`scan.rs`) — the one hand-written piece

Single pass, explicit state machine. States: `Text`, `InTag`, `InAttrValue(quote)`,
`Comment`, `Cdata`, `Pi`, `Doctype`. Maintain `depth`.

1. Parse the prolog: `<?xml?>` (encoding), optional `<!DOCTYPE>` (capture internal
   `<!ENTITY>` defs), comments/PIs. Stop at the **root start tag**; record its
   namespace declarations into `Prelude`. Mark prelude end offset.
2. Now `depth == 1` inside root. On each depth-1 **start** (`depth 1 -> 2`) remember
   `start`; when returning to `depth 1` (matching end tag, or a self-closing depth-1
   tag) emit `start..cursor` as a record. Skip whitespace-only text between records.
3. Use `memchr3(b'<', b'>', quote)`-style scanning to jump between delimiters instead
   of byte-by-byte; only meaningful bytes drive transitions.
4. On EOF expect `depth == 0` after the root end tag, else `XmlError::Malformed`.

Output: `ChunkIndex { prelude: Arc<Prelude>, records }`. No allocation per record
beyond the `Range` push.

### Phase B parser (`parse.rs`)

`RecordReader` wraps `quick_xml::NsReader` (or borrowed `Reader`) over `&[u8]`. Seed
it with `prelude.namespaces` and `prelude.entities` so prefix resolution and
`&entity;` expansion are correct in isolation. Map `quick_xml::events::Event` → our
`Event`. Thin adapter; `quick-xml` does the heavy lifting.

### Concurrency (`lib.rs`)

```rust
index.records.par_iter().enumerate().for_each(|(i, r)| {
    let rec = Record { bytes: &buf[r.clone()], prelude: index.prelude.clone(), index: i };
    f(&rec);            // par_for_each
});
```
`map_collect` uses `.map(...).collect::<Vec<_>>()` on the indexed parallel iterator
so output order == document order regardless of completion order.

### Error model

`XmlError { Malformed(pos), Encoding, Io, UnsupportedDtd, RecordError { index, source } }`.
A failure in one record carries its `index` and does not abort siblings in
`par_for_each` (errors collected); `map_collect` short-circuits on first error.

### Explicit non-goals for v1

- External DTD / parameter-entity resolution; validation against a schema/DTD.
- Bounded-memory streaming + parallel (requires whole doc resident or mmap).
- Parallelizing Phase A itself (future: speculative chunk + verify).
- Parallelism below depth 1 (nested content is parsed sequentially within its record
  — fine for the uniform-records target).
  > Superseded by `DECISIONS.md` §18: records may live under a configurable
  > container (`record_path`), not only at depth 1. Parallelism is still at the
  > record level; content nested *within* a record is parsed sequentially.

---

## Verification plan

1. **Correctness vs reference.** Parse fixtures with both `pxml` and a plain
   sequential `quick-xml` pass; assert identical per-record event sequences
   (normalized; order-independent across records). Stress the splitter with: `<`
   inside attributes, comments/CDATA containing `<record>`-looking text, self-closing
   depth-1 records, namespaced records, internal `<!ENTITY>`.
2. **Framing unit tests** in `scan.rs`: assert exact record byte ranges for hand-built
   inputs.
3. **Malformed inputs** → expected `XmlError` (unclosed tag, mismatched root).
4. **Benchmark** (`examples/bench.rs`): generate a synthetic N-record file (10k–1M
   `<trade>` records); compare `sequential()` vs `par_for_each` across thread counts;
   confirm the small-input fallback and the ~3–6× ceiling.
5. `cargo test` and `cargo build` clean.

## Bootstrap

```sh
cd pxml
git init                 # make it a standalone repo
cargo init --lib --name pxml --vcs none   # or hand-create src/lib.rs per the layout
# then implement scan.rs -> parse.rs -> lib.rs in that order
```
