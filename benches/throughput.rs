//! The repeatable performance suite (`cargo bench`).
//!
//! Every benchmark reports **bytes/second over the uncompressed document**, so
//! numbers are comparable across document shapes, execution paths and thread
//! counts. The suite covers the axes that actually move pxml's numbers:
//!
//! * **document shape** — many small records, few large records,
//!   attribute-heavy records, entity-heavy text (see [`Shape`]);
//! * **execution path** — Phase A alone, the resident parallel path, the
//!   sequential StAX baseline, and the streaming pipeline;
//! * **thread count** — a sweep on an explicit `rayon` pool, to show the serial
//!   fraction (Phase A) flattening the curve;
//! * **streaming pipeline sizing** — batch records / batch bytes / queue
//!   capacity, the knobs on [`Config`];
//! * **compressed input** — resident (decompress-then-scan) vs. streaming
//!   (decompress as you go), under the `zstd` feature.
//!
//! Run everything, or narrow with a filter:
//!
//! ```sh
//! cargo bench
//! cargo bench -- streaming          # one module
//! cargo bench -- resident::parse    # one benchmark
//! ```
//!
//! Corpora are generated once per process and leaked, so a `ParallelXml` can
//! borrow them for `'static` without a per-iteration copy; document setup never
//! lands inside a timed region.

use std::sync::OnceLock;

use divan::Bencher;
use divan::counter::BytesCount;
use pxml::{Config, Event, ParallelXml, Record, StreamReader};

fn main() {
    divan::main();
}

// ---------------------------------------------------------------------------
// Corpora
// ---------------------------------------------------------------------------

/// The document shapes under test. Sizes are chosen so every corpus is a few
/// MiB — large enough to leave the resident path's sequential fallback and to
/// swamp per-call overhead, small enough that a full sweep stays quick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// 200k tiny records: framing-dominated, the hardest case for Phase A's
    /// share of the work.
    Small,
    /// 4k records of ~2 KiB text: parse-dominated, the easiest case to scale.
    Large,
    /// 40k records carrying 12 attributes each: stresses attribute iteration.
    AttrHeavy,
    /// 40k records whose text is mostly entity references: stresses unescaping,
    /// the one place Phase B allocates.
    EntityHeavy,
}

impl Shape {
    const ALL: &'static [Shape] = &[
        Shape::Small,
        Shape::Large,
        Shape::AttrHeavy,
        Shape::EntityHeavy,
    ];

    fn build(self) -> String {
        match self {
            Shape::Small => {
                let mut s = String::from("<trades>");
                for i in 0..200_000 {
                    s.push_str(&format!("<t id=\"{i}\">{i}</t>"));
                }
                s.push_str("</trades>");
                s
            }
            Shape::Large => {
                let body = "lorem ipsum dolor sit amet ".repeat(76); // ~2 KiB
                let mut s = String::from("<docs>");
                for i in 0..4_000 {
                    s.push_str(&format!("<doc id=\"{i}\"><body>{body}</body></doc>"));
                }
                s.push_str("</docs>");
                s
            }
            Shape::AttrHeavy => {
                let mut s = String::from("<rows>");
                for i in 0..40_000 {
                    s.push_str("<row");
                    for a in 0..12 {
                        s.push_str(&format!(" c{a}=\"v{i}-{a}\""));
                    }
                    s.push_str("/>");
                }
                s.push_str("</rows>");
                s
            }
            Shape::EntityHeavy => {
                let mut s = String::from(
                    "<!DOCTYPE notes [<!ENTITY co \"ACME Corp\">]>\
                     <notes>",
                );
                for i in 0..40_000 {
                    s.push_str(&format!(
                        "<note>&co; &amp; &lt;{i}&gt; &quot;quoted&quot; &#233;clair</note>"
                    ));
                }
                s.push_str("</notes>");
                s
            }
        }
    }
}

impl std::fmt::Display for Shape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Shape::Small => "small",
            Shape::Large => "large",
            Shape::AttrHeavy => "attr_heavy",
            Shape::EntityHeavy => "entity_heavy",
        };
        f.write_str(name)
    }
}

/// The generated corpus for `shape`, built once per process and leaked so that
/// `ParallelXml::from_bytes` can borrow it for `'static`.
fn corpus(shape: Shape) -> &'static [u8] {
    static CACHE: OnceLock<Vec<&'static [u8]>> = OnceLock::new();
    let all = CACHE.get_or_init(|| {
        Shape::ALL
            .iter()
            .map(|s| &*Box::leak(s.build().into_bytes().into_boxed_slice()))
            .collect()
    });
    all[Shape::ALL
        .iter()
        .position(|s| *s == shape)
        .expect("known shape")]
}

/// A fresh reader over `shape` — free to build (the buffer is borrowed), so it
/// can be created per iteration to defeat the memoized Phase A scan.
fn doc(shape: Shape) -> ParallelXml {
    ParallelXml::from_bytes(corpus(shape))
}

/// The per-record workload: walk every event and fold it into a number, so the
/// parse cannot be optimized away but the closure itself costs almost nothing.
fn consume(record: &Record) -> usize {
    let mut events = record.events();
    let mut acc = 0usize;
    while let Some(ev) = events.next_event().expect("well-formed corpus") {
        acc += match ev {
            Event::Start { name, attrs } => {
                name.as_ref().len()
                    + attrs
                        .iter()
                        .map(|a| a.expect("valid attr").value.len())
                        .sum::<usize>()
            }
            Event::End { name } => name.as_ref().len(),
            Event::Text(t) => t.len(),
            Event::Cdata(c) => c.len(),
        };
    }
    acc
}

// ---------------------------------------------------------------------------
// Phase A — the boundary scan (the serial fraction)
// ---------------------------------------------------------------------------

mod phase_a {
    use super::*;

    /// Framing only: no record is parsed. This is the ceiling every parallel
    /// number is measured against.
    #[divan::bench(args = Shape::ALL)]
    fn scan(bencher: Bencher, shape: Shape) {
        bencher
            .counter(BytesCount::new(corpus(shape).len()))
            .with_inputs(|| doc(shape))
            .bench_refs(|doc| doc.index().expect("well-formed corpus").len());
    }
}

// ---------------------------------------------------------------------------
// Resident path
// ---------------------------------------------------------------------------

mod resident {
    use super::*;

    /// Scan + parallel parse of every record, ordered collection.
    #[divan::bench(args = Shape::ALL)]
    fn map_collect(bencher: Bencher, shape: Shape) {
        bencher
            .counter(BytesCount::new(corpus(shape).len()))
            .with_inputs(|| doc(shape))
            .bench_refs(|doc| doc.map_collect(consume).expect("well-formed corpus"));
    }

    /// The unordered driver: same work without the ordered `Vec`.
    #[divan::bench(args = Shape::ALL)]
    fn par_for_each(bencher: Bencher, shape: Shape) {
        bencher
            .counter(BytesCount::new(corpus(shape).len()))
            .with_inputs(|| doc(shape))
            .bench_refs(|doc| {
                doc.par_for_each(|rec| {
                    divan::black_box(consume(rec));
                })
                .expect("well-formed corpus")
            });
    }

    /// The whole-document StAX cursor — the single-threaded baseline for the
    /// speedups above.
    #[divan::bench(args = Shape::ALL)]
    fn sequential(bencher: Bencher, shape: Shape) {
        bencher
            .counter(BytesCount::new(corpus(shape).len()))
            .with_inputs(|| doc(shape))
            .bench_refs(|doc| {
                let mut reader = doc.sequential();
                let mut acc = 0usize;
                while let Some(ev) = reader.next_event().expect("well-formed corpus") {
                    acc += match ev {
                        Event::Start { name, .. } | Event::End { name } => name.as_ref().len(),
                        Event::Text(t) => t.len(),
                        Event::Cdata(c) => c.len(),
                    };
                }
                acc
            });
    }

    /// Scaling on an explicit pool. The curve flattens where Phase A (see
    /// `phase_a::scan`) starts to dominate — the point of the whole design.
    #[divan::bench(args = [1, 2, 4, 8, 16], name = "map_collect_threads")]
    fn threads(bencher: Bencher, threads: usize) {
        let shape = Shape::Small;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("pool");
        bencher
            .counter(BytesCount::new(corpus(shape).len()))
            .with_inputs(|| doc(shape))
            .bench_refs(|doc| {
                pool.install(|| doc.map_collect(consume).expect("well-formed corpus"))
            });
    }

    /// Re-parsing the same document: the Phase A scan is memoized, so the
    /// difference against `map_collect` is what the cache saves.
    #[divan::bench(args = Shape::ALL)]
    fn map_collect_reused_index(bencher: Bencher, shape: Shape) {
        let doc = doc(shape);
        doc.index().expect("well-formed corpus"); // warm the memo
        bencher
            .counter(BytesCount::new(corpus(shape).len()))
            .bench_local(|| doc.map_collect(consume).expect("well-formed corpus"));
    }
}

// ---------------------------------------------------------------------------
// Streaming pipeline
// ---------------------------------------------------------------------------

mod streaming {
    use super::*;

    /// The pipeline at its default sizing, over an in-memory source.
    #[divan::bench(args = Shape::ALL)]
    fn par_for_each(bencher: Bencher, shape: Shape) {
        bencher
            .counter(BytesCount::new(corpus(shape).len()))
            .bench_local(|| {
                StreamReader::from_reader(corpus(shape))
                    .par_for_each(|rec| {
                        divan::black_box(consume(rec));
                    })
                    .expect("well-formed corpus")
            });
    }

    /// Records per batch: the channel-send and receiver-mutex amortization
    /// knob. `1` is pathological (a channel round-trip per record) and runs
    /// ~100x slower, hence the reduced sample count for this benchmark.
    #[divan::bench(args = [1, 16, 64, 256, 1024], sample_count = 10)]
    fn batch_records(bencher: Bencher, records: usize) {
        let shape = Shape::Small;
        bencher
            .counter(BytesCount::new(corpus(shape).len()))
            .bench_local(|| {
                StreamReader::from_reader(corpus(shape))
                    .with_config(
                        Config::new()
                            .with_stream_batch_records(records)
                            // Isolate the record cap from the byte cap.
                            .with_stream_batch_bytes(usize::MAX),
                    )
                    .par_for_each(|rec| {
                        divan::black_box(consume(rec));
                    })
                    .expect("well-formed corpus")
            });
    }

    /// Bytes per batch, on the large-record shape where the byte cap is what
    /// actually bounds a batch.
    #[divan::bench(args = [4 * 1024, 64 * 1024, 1024 * 1024, 8 * 1024 * 1024], sample_count = 30)]
    fn batch_bytes(bencher: Bencher, bytes: usize) {
        let shape = Shape::Large;
        bencher
            .counter(BytesCount::new(corpus(shape).len()))
            .bench_local(|| {
                StreamReader::from_reader(corpus(shape))
                    .with_config(Config::new().with_stream_batch_bytes(bytes))
                    .par_for_each(|rec| {
                        divan::black_box(consume(rec));
                    })
                    .expect("well-formed corpus")
            });
    }

    /// Worker scaling for the pipeline, against the same sweep as
    /// `resident::map_collect_threads`. Framing stays on one producer thread,
    /// so this curve flattens for the same reason.
    #[divan::bench(args = [1, 2, 4, 8, 16], sample_count = 30)]
    fn workers(bencher: Bencher, workers: usize) {
        let shape = Shape::Small;
        bencher
            .counter(BytesCount::new(corpus(shape).len()))
            .bench_local(|| {
                StreamReader::from_reader(corpus(shape))
                    .with_config(Config::new().with_stream_workers(workers))
                    .par_for_each(|rec| {
                        divan::black_box(consume(rec));
                    })
                    .expect("well-formed corpus")
            });
    }

    /// Queue depth: how far the producer may run ahead of the workers.
    #[divan::bench(args = [1, 2, 8, 32], sample_count = 30)]
    fn queue_capacity(bencher: Bencher, batches: usize) {
        let shape = Shape::Small;
        bencher
            .counter(BytesCount::new(corpus(shape).len()))
            .bench_local(|| {
                StreamReader::from_reader(corpus(shape))
                    .with_config(Config::new().with_stream_queue_capacity(batches))
                    .par_for_each(|rec| {
                        divan::black_box(consume(rec));
                    })
                    .expect("well-formed corpus")
            });
    }
}

// ---------------------------------------------------------------------------
// Compressed input
// ---------------------------------------------------------------------------

#[cfg(feature = "zstd")]
mod compressed {
    use super::*;

    /// The zstd-compressed corpus for `shape` (level 3, the default), built and
    /// leaked once per process.
    fn compressed(shape: Shape) -> &'static [u8] {
        static CACHE: OnceLock<Vec<&'static [u8]>> = OnceLock::new();
        let all = CACHE.get_or_init(|| {
            Shape::ALL
                .iter()
                .map(|s| {
                    let z = zstd::encode_all(corpus(*s), 3).expect("compressible");
                    &*Box::leak(z.into_boxed_slice())
                })
                .collect()
        });
        all[Shape::ALL
            .iter()
            .position(|s| *s == shape)
            .expect("known shape")]
    }

    /// Decompress the whole document, then scan and parse it in parallel.
    /// Counted over the *uncompressed* bytes, so it is directly comparable to
    /// `resident::map_collect`.
    #[divan::bench(args = Shape::ALL)]
    fn resident(bencher: Bencher, shape: Shape) {
        bencher
            .counter(BytesCount::new(corpus(shape).len()))
            .bench_local(|| {
                ParallelXml::from_zstd_bytes(compressed(shape))
                    .expect("valid zstd")
                    .map_collect(consume)
                    .expect("well-formed corpus")
            });
    }

    /// Decompress as the pipeline frames: bounded memory, and decompression
    /// overlaps parsing.
    #[divan::bench(args = Shape::ALL)]
    fn streaming(bencher: Bencher, shape: Shape) {
        bencher
            .counter(BytesCount::new(corpus(shape).len()))
            .bench_local(|| {
                StreamReader::from_zstd_reader(compressed(shape))
                    .expect("valid zstd")
                    .par_for_each(|rec| {
                        divan::black_box(consume(rec));
                    })
                    .expect("well-formed corpus")
            });
    }
}
