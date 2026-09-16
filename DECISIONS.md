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
`<!ENTITY name "value">` definitions, against the shared `Prelude` (attribute
values via `quick-xml`'s free `escape::unescape_with`; text via the coalescing
cursor — see decision 17). The
materialized `scan()` **rejects** parameter entities (`<!ENTITY % …>`), external
entities (`SYSTEM`/`PUBLIC`), and external DTDs with `XmlError::UnsupportedDtd`
rather than silently skipping them (issue #3) — silently skipping risks
partially interpreting a document that depends on unsupported global entities.
The streaming/`SeqReader` DOCTYPE parse stays best-effort (it captures what it
can and ignores the rest).

**Why.** External DTD resolution is out of scope (and `quick-xml` doesn't do it
either). Predefined-first precedence keeps the reserved entities reserved. The
lookup is shared (`Prelude::resolve_entity`) across text and attribute decoding
and across all three reader types.

**Consequences.** A document that declares external/parameter entities fails fast
on the resident path instead of being half-interpreted. Internal `<!ENTITY>` and
the predefined set work everywhere.

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

> **Superseded as the primary harness by (19).** The `divan` suite in
> `benches/throughput.rs` is now the repeatable measurement; `examples/bench.rs`
> remains the exploratory driver (corpus generation, real `.zst` files). The
> findings below still hold and are the source of the streaming design.

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

## 16. memchr/memmem streaming framer (feature-gated)

**Context.** The streaming framer scans content byte-by-byte (only the text→`<`
hop used `memchr`). A natural optimization: drive the comment/CDATA/PI terminator
scans with `memmem` and the tag-interior scan with `memchr3`, skipping
name/attribute/whitespace bytes in bulk.

**Decision.** Implemented behind an opt-in `memchr-framer` Cargo feature (default
off); the byte-by-byte framer stays the default.

**Why not default.** On the 2M-record file it measured **~5% slower** (~400 ms vs
~380 ms): the records are many tiny tags, where `memchr`'s per-call setup doesn't
beat a short byte loop, and after batching (14) the producer is no longer the sole
bottleneck. It can still help documents with large text/CDATA spans (big bulk
skips), so it's kept available rather than dropped. Both framers share the struct,
prelude parse, compaction and emit; only the state enum and the content loop
differ under `cfg`, and both pass the same chunk-size parity tests (1…1000).

**Re-measured with the suite in (19) — the picture has changed.** On the four
benchmark shapes, streaming `par_for_each` medians, the `memchr` framer is now
**~1.75× faster on attribute-heavy records** (19 ms vs 33 ms — long start tags are
exactly what bulk scanning is for), and within noise on the other three
(`small` ~35 ms vs ~36–47 ms, if anything *less* variable; `large` and
`entity_heavy` tied). Nothing about the framer changed; the comparison did — the
old number came from one tiny-record document on different hardware, and the
batching/consumer work since (14, 22) moved the producer off the critical path.
On this evidence the feature looks like it should become default-on, but the
call belongs on hardware the maintainer trusts: this box's spread is wide enough
that "tied" on the headline small-record shape is not the same as "no
regression". Reproduce with
`cargo bench --features memchr-framer -- streaming` against `cargo bench -- streaming`.

**No fast path is left to add.** With the feature on, every *scanning* state is
bulk: `memchr` for text→`<` and for a quoted attribute value, `memchr3` for the
tag interior, `memmem` for the `-->` / `]]>` / `?>` terminators. What remains
byte-by-byte (`Lt`, `Bang`, `BangDash`, `CDATA[` matching) is O(1)-per-tag
dispatch over one to six bytes, where a bulk search could only lose. The
remaining producer-side work is the prelude parse, not the content loop.

**Bug worth noting.** The first cut had `memmem::find`'s arguments swapped
(`find(haystack, needle)`), so terminators were searched for *inside* the 3-byte
needle and never matched — the comment/CDATA never terminated. Caught by the
chunk-size parity test (chunk=2 on a comment+CDATA input), which is exactly why
that test sweeps many chunk sizes.

---

## 17. Coalescing text + `GeneralRef` events (quick-xml 0.40)

**Context.** quick-xml 0.40 changed its event model: a `Text` event no longer
carries `&…;` references inline. Each character or general entity reference in
element content is surfaced as a standalone `Event::GeneralRef(BytesRef)`, so
`<t>a &lt; b</t>` reads back as `Text("a ")`, `GeneralRef("lt")`, `Text(" b")`.
There is no reader config to opt out. The old code resolved text via the removed
`BytesText::unescape_with`; both reader cursors (`RecordReader`, `SeqReader`) hit
the `unreachable!` arm of `map_event` once a `GeneralRef` reached it.

**Decision.** Keep pxml's public contract — one `Event::Text` per text node, with
all entities resolved — by **coalescing** a maximal run of `Text`/`GeneralRef`
events back into a single event inside each cursor's `next_event`. A one-slot
`pending` lookahead holds the structural event that terminates the run. A `Text`
event now needs only `decode()` (no unescaping — references are separate events);
a `GeneralRef` resolves as a character reference (`BytesRef::resolve_char_ref`)
or a named entity (`Prelude::resolve_entity`), and an unknown name is rejected
with `RecordError` (never silently dropped). `map_event` is now structural-only
(`Start`/`Empty`/`End`/`CData`); the shared leaves (`is_text_run`, `decode_text`,
`append_run_event`) live in `parse.rs`.

**Why.** Surfacing `GeneralRef` to consumers (the other option) would fragment
text and push reassembly onto every caller, defeating the StAX-convenience the
crate sells. Coalescing keeps that contract.

**Consequences.** The **zero-copy lending invariant (decision 4) is preserved for
the common case**: a lone literal text node still decodes borrowed straight from
the document buffer, so no allocation. Only entity-bearing or multi-segment text
allocates an owned `String` — which the old `unescape_with` path did anyway as
soon as it saw an entity.

**Text-node boundaries are preserved.** Coalescing only joins *immediately*
adjacent `Text`/`GeneralRef` events: the run lookahead reads the next event raw
(`read_raw`), so a CDATA, child element, comment, or PI between two text nodes
ends the run. Comments/PIs are still not surfaced — the terminator is buffered
and the next call's skip loop (`next_surfaced`) drops it if ignorable — but
`<t>a<!--c-->&amp;</t>` stays `Text("a")`, `Text("&")` rather than collapsing to
one event. Covered by unit tests for char refs, adjacent/boundary entities, empty
expansion, CDATA/comment/PI/element run terminators, and unknown-entity errors,
plus a `coalesced_text_roundtrips` property test over random literal/entity/
char-ref interleavings.

---

## 18. Records under a nested container (`record_path`)

**Context.** The record model assumed the records are the root's direct children
(`<trades><trade/>…</trades>`). Real documents often wrap the uniform records one
or more levels down, next to sibling nodes that should be ignored:
`<root><manifest/><objects><object/>…</objects></root>`. Callers wanted to skip
the siblings and parallelise the container's children.

**Decision.** Add `Config::record_path` (and `ParallelXml::record_path` /
`StreamReader::record_path` builders): an element-name path from the root to the
**container** whose direct children are the records. Empty (the default) = the
root, i.e. today's behaviour. Framing generalises to a single rule with
`target = path.len() + 1`: while not inside a record, an element at `depth <
target` is *descended into* if its name matches the next path step (its `xmlns`
is accumulated into the shared `Prelude`) or its whole subtree is *skipped* if it
doesn't; elements at `depth == target` are records. This provably reduces to the
old depth-1 framing when `target == 1`, so the default path is unchanged.
Children of *every* matching container are framed, and leading/trailing siblings
are skipped, because matching is re-evaluated each time framing returns to a
descent level. A container name nested inside a skipped sibling is never matched
(the sibling is skipped whole).

**Why.** Naming the *container* (rather than the repeated record element)
generalises the existing "root is the container" model with the least new
concept: the rule "records = the container's direct children" is unchanged; only
the container moves. Skipping non-matching siblings falls out for free.

**Namespace scope is flat (accepted limitation).** The `Prelude` holds one
shared, immutable `NamespaceContext` (decision 6), so a descended container's
`xmlns` is *merged* into it rather than scoped per container. When `record_path`
matches multiple containers that redeclare the same prefix (or the default
namespace) to different URIs — `<objects xmlns:p="urn:1">…</objects><objects
xmlns:p="urn:2">…</objects>` — every record sees the last-writer-wins value, not
its own container's. Per-container scoping would require per-record preludes,
which contradicts the shared-`Prelude` architecture; the multi-container-conflict
case is outside the uniform-records target (where containers agree on their
declarations, or there is only one). Root- and ancestor-declared namespaces — the
common case — are always correct. This is consistent with namespaces already being
a lexical, best-effort, manual-resolution facility in v1 (decision 5).

**Consequences.** The resident scanner (`scan_with`) accumulates ancestor +
container `xmlns` into the `Prelude` for correct isolated parsing. The streaming
framer applies the same rule in both variants (default and `memchr-framer`),
gated so `target == 1` is byte-identical: descent name-matching and `xmlns`
capture happen at tag completion (the whole start tag is in `carry`); a
`skip_depth` counter skips non-matching subtrees using the existing incremental
machinery, so a huge skipped sibling stays carry-bounded (tested). Because a
container's `xmlns` is discovered mid-stream, the streaming `Prelude` is carried
**per batch** (`Batch { …, prelude }`) rather than fixed before the
producer/worker split. A property test asserts the streaming framer frames the
same records as the resident scanner under a container path, over generated
documents with leading/trailing siblings.

---

## 19. A repeatable benchmark suite (`benches/throughput.rs`, divan)

**Context.** Performance work so far leaned on `examples/bench.rs`: one synthetic
document shape, manual `Instant` timing, and setup/timing boundaries that differ
between the baseline and parallel cases (15). That is enough to spot a 2× shift
and not enough to *tune* — it cannot say whether a knob helped by 5%, and it
covers neither attribute-heavy nor entity-heavy records, the two shapes where
Phase B's cost is not proportional to bytes.

**Decision.** Add a `divan` benchmark suite (`cargo bench`, `benches/throughput.rs`)
as the canonical performance harness, and keep `examples/bench.rs` as the
exploratory driver (it also generates `.zst` corpora and runs real files, which a
benchmark harness should not). Every benchmark reports bytes/s over the
*uncompressed* document, so numbers compare across shapes, paths and thread
counts. The axes are: four document shapes (`small` 200k tiny records, `large` 4k
× ~2 KiB, `attr_heavy` 40k × 12 attributes, `entity_heavy` 40k entity-dense
records); execution path (Phase A alone, resident ordered/unordered, `SeqReader`
baseline, streaming, compressed resident vs compressed streaming); a thread sweep
on an explicit `rayon` pool; and the streaming sizing knobs (20).

**Why divan over Criterion.** One dev-dependency with a small tree, built-in
`counter` support for bytes/s, and argument sweeps (`args = […]`) that express
the "same benchmark across shapes/threads/knobs" structure directly. Criterion's
extra statistics don't pay for the dependency weight here; its MSRV would also
outrank the crate's.

**Consequences.** Corpora are generated once per process and leaked, so a
`ParallelXml` can borrow them for `'static` and setup never lands in a timed
region. Benchmarks that must defeat the memoized scan (20) build a fresh
`ParallelXml` per iteration via `with_inputs`, which is free (the buffer is
borrowed). Reference numbers on a 4-core cloud VM (medians; this machine is
noisy, so treat ratios, not absolutes, as the signal):

| Benchmark | small | large | attr_heavy | entity_heavy |
|---|---|---|---|---|
| `phase_a::scan` | 329 MB/s | 10.6 GB/s | 635 MB/s | 1.14 GB/s |
| `resident::map_collect` | 122 MB/s | 4.45 GB/s | 308 MB/s | 65.5 MB/s |
| `resident::map_collect` (memoized index) | 142 MB/s | 7.56 GB/s | 612 MB/s | 67.1 MB/s |
| `compressed::resident` | 101 MB/s | 2.41 GB/s | 204 MB/s | 62.3 MB/s |
| `compressed::streaming` | 142 MB/s | 1.98 GB/s | 174 MB/s | 176 MB/s |

Thread sweep (`resident::map_collect_threads`, `small`): 55.6 / 82.7 / 122 / 122 /
120 MB/s at 1 / 2 / 4 / 8 / 16 threads — **~2.2× at 4 threads on 4 cores**, flat
after, consistent with (15) and with Phase A's share of the work (`scan` is only
2.7× the full parallel parse on this shape).

---

## 20. The Phase A scan is memoized per document

**Context.** `index()`, `par_for_each`, `map_collect` and the `try_*` drivers each
called `scan_with` directly, so a program that counted records and then parsed
them scanned the buffer twice — paying the *serial* fraction twice, the one cost
the whole architecture is organized around.

**Decision.** `ParallelXml` memoizes its `ChunkIndex` in a `OnceLock`, filled on
first use and shared by every driver. `with_config` drops the memo, since a new
`record_path` frames different records. `index()` now returns `&ChunkIndex`
(**breaking**; clone it to outlive the document) so reuse is genuinely free rather
than a `Vec<Range>` copy per call.

**Why a `OnceLock` and not a `Mutex`/`RwLock`.** The drivers take `&self` and run
on a thread pool; `OnceLock` gives interior mutability with no lock on the read
path. Two threads racing the first call both scan and one result is dropped — the
scan is pure, so either is correct, and the race window is one scan.

**Errors are not memoized.** `XmlError` is not `Clone` (it carries a boxed source),
and a failing scan is fail-fast — it stops at the first malformed byte instead of
walking the document. A failed `index()` therefore re-scans on the next call.

**Consequences.** `map_collect` on an already-indexed document measures 1.16×
(small) to 1.98× (attribute-heavy) faster than the cold call (19) — the memo is
worth the most exactly where Phase A's share is largest. `ChunkIndex` is now
`Clone`, and gained `prelude_arc()` (a cheap `Arc` clone for keeping the shared
context after the index is gone) while `prelude()` returns `&Prelude` rather than
`&Arc<Prelude>`.

---

## 21. Streaming pipeline sizing is configurable (and byte-bounded)

**Context.** `StreamReader` hard-coded `BATCH = 256` records per message and a
channel capacity of `2 × threads`, and took no `Config` at all. Two problems: the
sizing is workload-dependent (19 shows batch size moving throughput by 100×), and
a record cap alone does not bound memory — 256 × 1 MiB records is a 256 MiB batch
in a pipeline whose entire selling point is bounded memory.

**Decision.** Move the sizing into `Config` — `with_stream_batch_records`
(default 256), `with_stream_batch_bytes` (default 1 MiB), `with_stream_queue_capacity`
and `with_stream_workers` (both default `0` = derive from `rayon`'s current pool)
— and give `StreamReader` a `with_config`. A batch closes when *either* cap is
reached; the byte check runs after a push, so a single record larger than the cap
is dispatched alone rather than stalling the producer. `0` for either cap means
one record per batch (a cap of `0` would otherwise be unsatisfiable).

**Why these defaults.** The measured knee is broad: on the large-record shape,
batch-byte caps of 64 KiB, 1 MiB and 8 MiB are within noise of each other
(~1.6–1.9 GB/s) while 4 KiB collapses to 264 MB/s; on tiny records, batch-record
caps of 256 and 1024 are within ~10% while 16 costs 8× and 1 costs **100×**
(3.7 s vs 37 ms — a channel round-trip per record). So 256 records / 1 MiB sits
safely on the plateau for both, and the byte cap only binds for records large
enough that per-record cost dominates the handoff anyway.

**Consequences.** `StreamReader` now carries a `Config` instead of a bare
`record_path`, and `record_path()` is the shorthand for setting just that field.
`with_stream_workers` also answers "no way to use a specific `ThreadPool`" (issue
#5): the worker count is explicit, and `pool.install(…)` still sets it by default.

---

## 22. A fixed-worker consumer replaces `par_bridge`

**Context.** The streaming consumer was `rx.into_iter().par_bridge().for_each(…)`.
`par_bridge` bridges a sequential iterator into `rayon` by having pool threads
pull items under a shared mutex — and with a *blocking* iterator (a channel
`recv`), the puller blocks while holding it. The calling thread also participates,
so the pipeline runs `producer + pool threads + caller` on the machine.

**Decision.** Run a fixed set of worker threads that pull batches off the channel
themselves: a `Mutex<Receiver<Batch>>` locked only across `recv` (never while
parsing), `workers` threads in a `thread::scope`. Worker count still comes from
`rayon::current_num_threads()` by default, so `pool.install(…)` sizes the pipeline
as before, and `Config::with_stream_workers` sets it directly (21).

**Why.** Measured with the new suite (19), A/B over three runs each on a 4-core
VM, streaming `par_for_each` medians:

| Shape | `par_bridge` | fixed workers |
|---|---|---|
| entity_heavy | ~34–36 ms | **~17 ms** |
| small | ~38–50 ms | ~40–47 ms |
| attr_heavy | ~32 ms | ~34 ms |
| large | ~4.4–5.4 ms | ~4.5–5.1 ms |

A ~2× win on the parse-heavy shape (where per-record work is largest and
oversubscription hurts most), and a 0–10% loss elsewhere that sits inside this
box's run-to-run spread. The design is also simpler to reason about — N workers,
one batch each, one lock per *batch* — and it removes a deadlock hazard: the
workers are plain threads, so a closure that itself uses `rayon` is not competing
for the pool threads that the pipeline is blocking.

**Consequences.** Lock poisoning is handled explicitly: a poisoned lock means
another worker panicked, so the remaining workers stop, which drops the receiver,
releases the producer, and lets the original panic propagate out of the scope
(the same observable behaviour as `par_bridge` unwinding). `rayon` is still used
for the resident path and for the default worker count.

---

## Future work

- **Reduce streaming overhead further.** Batching + arena are done (15), the
  batch/queue/worker sizing is now tunable (21), and the consumer no longer goes
  through `par_bridge` (22). A faster prelude parse is the remaining
  producer-side knob — and deciding, on trusted hardware, whether the `memchr`
  framer should become default-on (16).
- **Parallel decompression / parallel Phase A.** The single sequential producer is
  now the ceiling. zstd multi-frame decode or a speculative chunk-and-verify scan
  would attack the remaining serial fraction.
- **Namespace resolution.** Optionally resolve prefixes per event using
  `Prelude::namespaces` plus record-local declarations.
- **Ordered streaming.** A bounded reorder buffer for `map_collect`-style streaming
  output, if a positional consumer needs it.
