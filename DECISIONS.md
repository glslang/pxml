# Design decisions and analysis

This document records the decisions made while implementing `pxml`, with the
reasoning and trade-offs behind each. It complements [`DESIGN.md`](DESIGN.md)
(the pre-implementation feasibility spec): where the two differ, this document
reflects what was actually built and supersedes it — most notably on streaming,
which `DESIGN.md §6` listed as out of scope but which was later implemented.

Each entry follows **Context → Decision → Why → Consequences**.

---

## 1. Two-phase scan-then-parse

**Context.** "Parallel StAX" can't mean one event cursor advanced by many threads —
events are ordered and stateful. And XML can't be split at an arbitrary byte
offset, because a `<`/`>` may sit inside an attribute value, comment, CDATA
section, PI, or the DTD.

**Decision.** Split the work in two: a cheap, single-threaded **boundary scan**
(Phase A) that frames the depth-1 records and captures shared prolog context,
then an **embarrassingly parallel per-record parse** (Phase B) on `rayon`.

**Why.** Phase A does almost no work per byte (delimiter transitions only) and is
memory-bandwidth bound; Phase B does the expensive work (entity decode, attribute
parsing, UTF-8 validation) and is independent per record. The "records are
order-independent" assumption is what makes parallelism sound.

**Consequences.** Speedup is sub-linear (Phase A is the irreducible serial
fraction). The model fits "one root of many uniform records" and not arbitrary
XML.

---

## 2. Phase A is hand-written

**Context.** Phase A needs the byte ranges of the top-level elements plus the
prolog context.

**Decision.** Hand-write the boundary scanner; do **not** reach for a general XML
parser here.

**Why.** No crate cheaply exposes "give me the depth-1 element byte ranges." The
scanner is deliberately *not* a conformant parser — it tracks just enough lexical
state (text / tag / quote / comment / CDATA / PI / doctype) and depth to frame
records, using `memchr` to jump between delimiters.

**Consequences.** A small, focused, testable state machine. It is validated for
exact byte-range parity against hand-built inputs and (later) against the
streaming framer.

---

## 3. Phase B reuses `quick-xml`

**Decision.** Per record, run `quick-xml`'s reader over just that record's slice;
don't write a second XML tokenizer.

**Why.** A spec-correct XML lexer is the costly, bug-prone path. `quick-xml` is
fast, zero-copy, and handles attributes, entity hooks, and CDATA. This is the
biggest reuse win.

**Consequences.** We inherit `quick-xml`'s API constraints — which directly drove
decisions 4 and 5.

---

## 4. Events are a *lending* pull cursor (`Event<'_>` tied to the reader)

**Context.** The spec sketched `next_event() -> Event<'doc>` borrowing from the
document (full zero-copy). But `quick-xml`'s `BytesStart::name()` returns a
`QName` borrowed from the **event object**, not from the input buffer
(`fn name(&self) -> QName` — the lifetime is `&self`, not the input `'a`). So a
freshly read `Event<'doc>` cannot hand out an element name with `'doc` lifetime.

**Decision.** Make the cursor *lending*: store the current `quick-xml` event in
the reader and return `Event<'_>` tied to `&mut self`. Process (or copy out of)
each event before requesting the next.

**Why.** The alternatives were worse: an `unsafe` lifetime transmute (we know the
borrowed buffer outlives the event, but it's unsafe and fragile), or eagerly
owning every name (an allocation per event). Text and CDATA still borrow the
input directly; only names are bound to the event.

**Consequences.** Still zero-copy in practice (no allocations for names/text). The
ergonomic cost is the standard lending-iterator one: you can't hold two events at
once. `RecordReader` and `SeqReader` share one event-mapping function.

---

## 5. Namespaces are surfaced lexically

**Context.** `xmlns` declarations on the root apply to every record, but a record
parsed in isolation doesn't see them. `quick-xml`'s `NsReader` resolves
namespaces as it reads, but offers no way to *seed* it with declarations that
live outside the slice.

**Decision.** Surface element/attribute names **lexically** (the `QName` exactly
as written, prefix intact). Capture the root/prolog `xmlns` declarations into
`Prelude::namespaces` for callers that want to resolve prefixes themselves.

**Why.** The `Event::Start { name: QName }` API already carries lexical names, so
this is consistent. Seeding `NsReader` externally isn't supported, and writing a
full resolution layer was deferred.

**Consequences.** Prefix → URI resolution is the caller's job for now (the data is
available on the prelude). Full per-event resolution is future work.

---

## 6. A shared, immutable `Prelude`

**Context.** A few document-global features survive the "records are independent"
assumption: encoding, root namespaces, and internal-subset entity definitions.

**Decision.** Phase A builds one immutable `Prelude { encoding, root_name,
namespaces, entities }`, wrapped in `Arc`, and every worker is seeded with it.

**Why.** These are exactly the things a naive "split and parse" gets wrong.
Capturing them once and sharing immutably (cheap `Arc` clones) fixes correctness
without per-record cost.

**Consequences.** Workers parse correctly in isolation. The same `Prelude` type is
reused (with a lazily-filled entity map) by the streaming and sequential readers.

---

## 7. Entity resolution: predefined + internal subset only

**Decision.** Resolve the five predefined XML entities plus internal-subset
`<!ENTITY name "value">` definitions, via `quick-xml`'s `unescape_with`. Skip
parameter entities (`<!ENTITY % …>`) and external/SYSTEM/PUBLIC entities.

**Why.** External DTD resolution is out of scope (and `quick-xml` doesn't do it
either). Predefined-first precedence keeps the reserved entities reserved. The
lookup is shared (`Prelude::resolve_entity`) across text and attribute decoding
and across all three reader types.

**Consequences.** Documents relying on external DTDs won't expand those entities.
Internal `<!ENTITY>` and the predefined set work everywhere.

---

## 8. Error model

**Decision.** `XmlError { Malformed(pos), Encoding, Io, UnsupportedDtd,
RecordError { index, source } }`. `RecordError` carries the failing record's
document index.

**Why.** Callers need to know *which* record failed in a parallel run.
`par_for_each` returns `Err` for the Phase A scan failure but does not abort
siblings (its closure returns `()`, so per-record parse errors are the consumer's
concern as it drives `events()`). `map_collect` collects and surfaces the scan
error.

**Consequences.** A clean, small error surface that threads the record index
through both the resident and streaming paths.

---

## 9. Small-input sequential fallback

**Context.** For small documents the thread-pool + chunk-index overhead loses to a
plain sequential pass.

**Decision.** Below `Config::parallel_threshold` bytes **or** `Config::min_records`
records, `par_for_each` / `map_collect` transparently run a sequential loop over
the same framed records.

**Why.** Same code path, same results, just no pool. Verified by a test asserting
the fallback and parallel paths produce identical output.

**Consequences.** Two tuning knobs (defaults 4 MiB / 64 records). The streaming
path skips this (it targets large inputs by definition).

---

## 10. Unordered vs ordered parallel APIs

**Decision.** Two entry points: `par_for_each` (unordered — workers fire as
records complete) and `map_collect` (an *indexed* `par_iter().map().collect()`
that restores document order on output).

**Why.** Unordered matches the "any order" assumption and is the natural API;
ordered is needed when downstream consumes results positionally. rayon's indexed
collect gives document order for free regardless of completion order.

**Consequences.** `map_collect` holds all results (`Vec<T>`) in memory; fine when
`T` is small.

---

## 11. Memory model: whole document resident (Vec or mmap)

**Decision.** The `ParallelXml` path reads the whole document into a `Vec` or
`mmap`s it; workers borrow zero-copy slices.

**Why.** Parallel workers need random, concurrent access to their record slices,
and `map_collect` reorders by index — both require a stable, contiguous,
resident buffer.

**Consequences.** For a *plain* large file, `mmap` keeps physical memory bounded
via OS paging (touched pages in, evicted under pressure), so this path already
handles big uncompressed files. The problem is *compressed* input — see 12–14.

---

## 12. zstd decompression for the resident path

**Decision.** A default-on `zstd` feature (optional, C-backed). `from_path`
auto-detects the zstd magic number (`28 B5 2F FD`) and decompresses the whole
document into memory; plain XML (which never starts with that magic) is mmap'd.
`from_zstd_bytes` / `from_zstd_reader` cover in-memory and streamed compressed
input.

**Why.** Auto-detection is unambiguous (no XML document starts with the magic) and
ergonomic. A feature flag lets users avoid the C dependency
(`default-features = false`).

**Consequences.** The whole decompressed document is resident (no mmap of the
decompressed form), and decompression is sequential — it adds to the serial
fraction. This motivated the streaming path.

---

## 13. Can "zstd streaming" reduce memory for the parallel path? (analysis)

This question deserves its own entry because the answer is subtle. "Use
streaming" can mean three different things:

1. **Stream the compressed *input*.** Already done. `zstd::decode_all` *is* a
   streaming `Decoder` + `io::copy`: it pulls compressed bytes incrementally and
   never buffers the whole compressed file. `from_zstd_reader(File)` already
   streams off disk.

2. **Don't materialize the decompressed *output*.** Not possible for the parallel
   path. Workers need random, concurrent access to slices and `map_collect`
   reorders — both require the whole decompressed document resident.
   `read_to_end` on a streaming decoder yields the *same* `Vec` as `decode_all`;
   streaming the output doesn't reduce memory. (Worse: parsing from a still-growing
   `Vec` would invalidate borrowed slices on reallocation.)

3. **Pipeline decompress → frame → parse.** This is the real opportunity, and what
   was built (14). A single sequential producer doesn't need random access — it
   only needs to find record boundaries — and can feed already-framed records to a
   parallel consumer, overlapping the sequential decode/scan with the parallel
   parse and keeping memory bounded.

**Key realization (from discussion).** The parallel parser's *working set* is
O(in-flight records), not O(file). The resident `Vec` is the only O(file) cost.
If a bounded queue limits how many records are in flight, memory is bounded by the
threads, independent of file size. There's no need for random access — only for
knowing where the object boundaries are, to feed `rayon`.

---

## 14. Streaming pipeline (`StreamReader`)

**Decision.** A new `StreamReader` type (kept distinct from `ParallelXml`, since
one borrows slices and the other owns them). A single **producer thread**
decompresses and frames records, packs them into **batches** (each backed by one
**arena** allocation), and sends batches into a **bounded channel**; the workers
drain it via `rayon`'s `par_bridge` under `thread::scope`, parsing a whole batch
each. Output is **unordered**.

**Why these specifics.**
- *Batches + arena (`B = 256` records)*: this is the throughput lever (see 15).
  Sending one batch instead of `B` records amortizes the channel send and the
  `par_bridge` receiver mutex over `B` records, and the framer appends each
  record's bytes into a single per-batch `Vec<u8>` (one allocation per batch, not
  per record). Records are *owned* because a worker may outlive the producer's
  view of those bytes (the producer compacts its buffer), and the copy is tiny
  next to decompress + parse — and it makes each batch arena cache-resident during
  parsing.
- *Bounded channel = backpressure*: the producer blocks when the queue is full, so
  it only runs ahead `≈ 2 × threads` records. This is the knob that bounds memory.
  Unbounded, the producer would race ahead and re-materialize the whole file.
- *Unordered*: matches the "any order" assumption and the aggregation/side-effect
  use case for huge files; ordered streaming would need a reorder buffer that can
  stall. (User-selected.)
- *No new dependencies*: `std::sync::mpsc::sync_channel` + `std::thread::scope` +
  `rayon::par_bridge`.

**Incremental framer.** The materialized `scan()` consumes a whole `&[u8]`.
Streaming needed a **resumable** Phase A: a NeedMore-aware prolog/root parser
(re-run on the growing buffer until complete) plus a byte-state-machine content
framer that emits owned records at depth-1 boundaries and `compact()`s consumed
bytes. It's byte-by-byte (so split tokens across chunk boundaries resume
cleanly), with a `memchr` fast-path for the text scan. It is tested for exact
parity with the materialized scanner across chunk sizes 1…1000.

**Memory bound.**
```
prelude (shared, small)
+ producer carry buffer        (one in-progress record + one decompress chunk)
+ batch being built            (≤ B records)
+ queue_depth batches in flight (capacity × B records; capacity ≈ 2×threads)
+ results retained             (none for par_for_each)
```
≈ **O(threads × B × record_size)**, still independent of document size (`B`
trades a little memory + first-batch latency for throughput). The per-record floor
is **O(max_record_size)**: a record can't be split across workers, so the producer
must buffer a whole record before emitting it. Fine for many small uniform
records; pathological for a single multi-GB element.

**Consequences.** Constant memory regardless of document size. With batching this
is *not* at a throughput cost — for large documents the streaming path is actually
**faster** than resident (15), because it pipelines decompression with parsing,
keeps each batch arena cache-resident, and never materializes the whole document.
Decompression + framing are still sequential (a single producer), which is the
remaining serial-fraction ceiling.

---

## 15. Benchmark methodology and findings

**Methodology.** `examples/bench.rs` generates a synthetic `<trades>` document of
N uniform records (each with attributes, fields, and an `&amp;` entity so decoding
is exercised) and a per-record workload that drives the full `events()` API and
folds into a checksum (so work isn't optimized away). Timing is manual
(`Instant`); thread counts are swept via `rayon` local pools. A `file` mode
benchmarks the resident vs. streaming paths over a real `.zst` file.

**In-memory parallel scaling** (light records, ~27 MiB, machine-dependent):
peaks around **~1.8× at 4 threads**, then declines — memory-bandwidth/scan-bound,
below the 3–6× ceiling that larger records would reach. Honest sub-linear scaling,
as `DESIGN.md` predicts.

**Compressed-file resident vs streaming** (2,000,000 records; 3.4 MiB on disk →
183.7 MiB decompressed, 54× ratio):

| Path | Wall time | Throughput |
|---|---|---|
| resident (`from_path`) | ~840 ms | ~2.4 M rec/s, ~219 MiB/s |
| streaming, per-record handoff (initial) | ~2120 ms | ~0.94 M rec/s, ~87 MiB/s |
| **streaming, batched + arena** | **~380 ms** | **~5.3 M rec/s, ~490 MiB/s** |

**Initial finding.** With a per-record handoff, streaming was **~2.5× slower**: the
single producer (decompress + byte-by-byte framing + a `Box` per record + channel
send, drained by `par_bridge`'s mutex-synchronized pull) was the bottleneck for
millions of tiny records, and the workers starved.

**After batching + arena (13/14).** Packing `B = 256` records per channel message
and per arena allocation cut streaming to **~380 ms — a 5.5× speedup, and ~2.2×
*faster* than resident.** The gap didn't just close, it inverted. Three effects
compound:

1. **Amortized handoff** — one `send` / `par_bridge` pull per 256 records collapses
   the channel + mutex traffic that dominated before.
2. **One allocation per batch**, not per record.
3. **Pipelining + cache locality** — the producer's decompression overlaps the
   parallel parse, and each ~25 KiB batch arena stays cache-resident while a worker
   parses it. The resident path instead makes several passes over a 184 MiB buffer
   that doesn't fit in cache, and pays to materialize all 184 MiB up front.

**Takeaway.** For large documents the streaming pipeline is both bounded-memory
*and* faster — it can be the preferred path, not just a memory fallback. The
caveats: this is a uniform-small-record, highly-compressible, machine-specific
benchmark; for inputs that fit in cache or heavier per-record parse work the
advantage shrinks, and the single sequential producer remains the ceiling.

---

## Future work

- **Reduce streaming overhead further.** Batching + arena are done (15). A
  `memchr`-driven streaming framer (the content scan is still byte-by-byte) and a
  tunable `B` would chip at the remaining producer cost.
- **Parallel decompression / parallel Phase A.** The single sequential producer is
  now the ceiling. zstd multi-frame decode or a speculative chunk-and-verify scan
  would attack the remaining serial fraction.
- **Namespace resolution.** Optionally resolve prefixes per event using
  `Prelude::namespaces` plus record-local declarations.
- **Ordered streaming.** A bounded reorder buffer for `map_collect`-style streaming
  output, if a positional consumer needs it.
