//! Bounded-memory streaming pipeline.
//!
//! For inputs that shouldn't be fully materialized — a multi-GB compressed file,
//! or several at once — [`StreamReader`] decompresses and frames the document on
//! a single producer thread and parses the framed records on a fixed set of
//! worker threads. A bounded channel between them provides backpressure, so the
//! producer only runs ahead as far as the workers can drain: resident memory is
//! bounded by the live batches — `queue capacity + workers + 1` of them, since a
//! worker holds the batch it is parsing and the producer is filling the next —
//! plus the chunk being framed, independent of document size. Batch size, queue
//! capacity and worker count are [`Config`] knobs; see
//! [`Config::with_stream_batch_records`].
//!
//! Trade-offs vs. the resident [`ParallelXml`](crate::ParallelXml) path: records
//! are *owned* (copied out of the decompression buffer rather than borrowed), and
//! output is unordered. Decompression + framing remain sequential, so they bound
//! the achievable speedup (Amdahl).

use std::io::Read;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::scan::StreamFramer;
use crate::{Config, Prelude, Record, XmlError};

/// Bytes pulled from the source per read.
const CHUNK: usize = 64 * 1024;

/// A batch of framed records sharing one arena allocation. `records` holds each
/// record's document index and its byte span within `data`. `prelude` is the
/// shared context as of when the batch was framed — carried per batch so a
/// container's `xmlns`, captured during descent, reaches the workers.
struct Batch {
    data: Vec<u8>,
    records: Vec<(usize, Range<usize>)>,
    prelude: Arc<Prelude>,
}

/// A streaming, bounded-memory parser over a (decompressing) byte source.
///
/// Build one with [`StreamReader::from_reader`] or
/// [`StreamReader::from_zstd_reader`], then drive it with
/// [`par_for_each`](StreamReader::par_for_each).
///
/// Use this instead of [`ParallelXml`](crate::ParallelXml) when the document
/// does not comfortably fit in memory — typically a multi-GB *compressed* file,
/// which cannot be mmap'd in its decompressed form. Resident memory is bounded
/// by `(queue capacity + workers + 1) × batch size` plus the chunk being framed
/// (see [`Config::with_stream_queue_capacity`]), independent of document size.
///
/// ```
/// use pxml::{Event, StreamReader};
/// use std::sync::atomic::{AtomicUsize, Ordering};
///
/// // Any `Read` source — a File, a socket, or here a plain byte slice.
/// let xml = &b"<trades><trade>1</trade><trade>2</trade></trades>"[..];
/// let total = AtomicUsize::new(0);
///
/// StreamReader::from_reader(xml).par_for_each(|record| {
///     let mut events = record.events();
///     while let Some(ev) = events.next_event().unwrap() {
///         if let Event::Text(t) = ev {
///             total.fetch_add(t.parse::<usize>().unwrap(), Ordering::Relaxed);
///         }
///     }
/// })?;
///
/// assert_eq!(total.load(Ordering::Relaxed), 3);
/// # Ok::<(), pxml::XmlError>(())
/// ```
///
/// # Trade-offs vs. the resident path
///
/// Records arrive **unordered** and are **owned** (copied out of the decode
/// buffer rather than borrowed from it), so there is no streaming equivalent of
/// [`map_collect`](crate::ParallelXml::map_collect). In exchange memory is
/// constant, and on large documents throughput is often *better*, because the
/// pipeline overlaps decompression with parsing and keeps each batch
/// cache-resident.
pub struct StreamReader<'a> {
    reader: Box<dyn Read + Send + 'a>,
    /// Framing (the record path) plus the pipeline sizing knobs — batch caps,
    /// channel capacity and worker count. The parallelism thresholds do not
    /// apply here.
    config: Config,
}

impl std::fmt::Debug for StreamReader<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamReader")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<'a> StreamReader<'a> {
    /// Stream over an already-decompressed byte source (any `Read`).
    pub fn from_reader<R: Read + Send + 'a>(reader: R) -> Self {
        Self {
            reader: Box::new(reader),
            config: Config::default(),
        }
    }

    /// Stream over a zstd-compressed byte source, decompressing incrementally.
    #[cfg(feature = "zstd")]
    #[cfg_attr(docsrs, doc(cfg(feature = "zstd")))]
    pub fn from_zstd_reader<R: Read + Send + 'a>(reader: R) -> std::io::Result<Self> {
        let decoder = zstd::Decoder::new(reader)?;
        Ok(Self {
            reader: Box::new(decoder),
            config: Config::default(),
        })
    }

    /// Frame the direct children of the container reached by following `path`,
    /// skipping non-matching siblings — the streaming counterpart of
    /// [`Config::with_record_path`](crate::Config::with_record_path). Empty =
    /// the root's direct children (the default).
    ///
    /// A shorthand for the record path alone; use
    /// [`with_config`](Self::with_config) to set it together with the pipeline
    /// sizing knobs.
    pub fn record_path<I, S>(mut self, path: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Box<str>>,
    {
        self.config.record_path = path.into_iter().map(Into::into).collect();
        self
    }

    /// Override the whole [`Config`] — the record path plus the streaming
    /// pipeline sizing:
    /// [batch records](Config::with_stream_batch_records),
    /// [batch bytes](Config::with_stream_batch_bytes),
    /// [queue capacity](Config::with_stream_queue_capacity) and
    /// [worker count](Config::with_stream_workers).
    ///
    /// Replaces the configuration rather than merging into it, so a later
    /// [`record_path`](Self::record_path) call still applies but an earlier one
    /// is discarded. The parallelism thresholds
    /// ([`parallel_threshold`](Config::parallel_threshold) /
    /// [`min_records`](Config::min_records)) are ignored: streaming always runs
    /// the pipeline.
    ///
    /// ```
    /// use pxml::{Config, StreamReader};
    ///
    /// // Smaller batches and a shallower queue: less memory in flight, at the
    /// // cost of more channel traffic.
    /// let config = Config::new()
    ///     .with_stream_batch_records(32)
    ///     .with_stream_batch_bytes(64 * 1024)
    ///     .with_stream_queue_capacity(4);
    ///
    /// let xml = &b"<trades><trade>1</trade><trade>2</trade></trades>"[..];
    /// StreamReader::from_reader(xml)
    ///     .with_config(config)
    ///     .par_for_each(|record| drop(record.as_bytes()))?;
    /// # Ok::<(), pxml::XmlError>(())
    /// ```
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// The configuration this reader will run with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Frame records on a producer thread and apply `f` to each in parallel,
    /// in unordered (completion) order.
    ///
    /// Returns `Err` if framing or I/O fails; records already dispatched are
    /// still processed (siblings are not aborted). Per-record parse errors are
    /// the closure's concern (it drives `record.events()`).
    ///
    /// Records are parsed on a fixed set of worker threads — by default as many
    /// as `rayon`'s current pool has, so `pool.install(|| reader.par_for_each(f))`
    /// still sizes the pipeline from that pool; set
    /// [`Config::with_stream_workers`] to size it without involving `rayon` at
    /// all. The workers are plain threads rather than pool tasks, so a closure
    /// that itself uses `rayon` cannot deadlock against the pipeline.
    pub fn par_for_each<F>(self, f: F) -> Result<(), XmlError>
    where
        F: Fn(&Record) + Sync,
    {
        let mut reader = self.reader;
        // `0` caps would stall the producer (a batch could never fill), so they
        // mean "one record per batch".
        let batch_records = self.config.stream_batch_records.max(1);
        let batch_bytes = self.config.stream_batch_bytes.max(1);
        let queue_capacity = self.config.stream_queue_capacity;
        let mut framer = StreamFramer::with_path(self.config.record_path);
        let mut chunk = vec![0u8; CHUNK];

        // Parse the prolog on this thread before splitting into producer/workers.
        // The prelude is carried per batch (the framer augments it as it descends
        // into a container), so the base returned here is only used to detect a
        // prolog error / drive the loop.
        loop {
            if framer.try_prelude()?.is_some() {
                break;
            }
            let n = reader.read(&mut chunk).map_err(XmlError::Io)?;
            if n == 0 {
                return Err(XmlError::Malformed(0)); // no root element
            }
            framer.push(&chunk[..n]);
        }

        let workers = match self.config.stream_workers {
            0 => rayon::current_num_threads().max(1),
            n => n,
        };
        let capacity = match queue_capacity {
            0 => workers * 2,
            n => n,
        };
        let (tx, rx) = sync_channel::<Batch>(capacity);

        thread::scope(|scope| {
            let producer = scope.spawn(move || -> Result<(), XmlError> {
                let mut chunk = vec![0u8; CHUNK];
                loop {
                    // Pack records into one arena allocation, up to whichever
                    // cap — records or bytes — is reached first. The byte check
                    // runs after a push, so one record larger than the cap is
                    // dispatched on its own instead of stalling the pipeline.
                    let mut data = Vec::new();
                    let mut records = Vec::with_capacity(batch_records.min(1024));
                    let mut need_more = false;
                    while records.len() < batch_records && data.len() < batch_bytes {
                        match framer.next_record_into(&mut data)? {
                            Some(record) => records.push(record),
                            None => {
                                need_more = true;
                                break;
                            }
                        }
                    }
                    if !records.is_empty() {
                        // Read the prelude after framing, so it reflects any
                        // container `xmlns` captured while producing this batch.
                        let prelude = framer.prelude();
                        if tx
                            .send(Batch {
                                data,
                                records,
                                prelude,
                            })
                            .is_err()
                        {
                            return Ok(()); // consumer dropped
                        }
                    }
                    if need_more {
                        framer.compact();
                        let n = reader.read(&mut chunk).map_err(XmlError::Io)?;
                        if n == 0 {
                            framer.finish()?;
                            return Ok(());
                        }
                        framer.push(&chunk[..n]);
                    }
                }
            });

            // A fixed set of workers pulls whole batches off the channel and
            // parses their records. The bounded channel throttles the producer
            // when the workers fall behind.
            //
            // This deliberately does not use `rayon`'s `par_bridge`: bridging a
            // sequential iterator into the pool costs more than it buys here —
            // measurably so on parse-heavy records (see DECISIONS.md §22). The
            // worker count still defaults to the current pool's, so
            // `pool.install(…)` sizes the pipeline as before.
            let rx = Mutex::new(rx);
            consume_batches(&rx, workers, &f);

            producer.join().expect("producer thread panicked")
        })
    }
}

/// Run `workers` threads that pull batches off `rx` until the producer hangs up,
/// applying `f` to every record of every batch.
///
/// The receiver is shared behind a `Mutex` held only across `recv` — never while
/// parsing — so the workers contend once per *batch*, not once per record.
///
/// # A panicking closure must not drain the document first
///
/// `f` runs outside the lock, so a panic in it cannot poison the mutex — without
/// the `stop` flag the surviving workers would keep pulling, and the panic would
/// not surface until the *whole* document had been framed and parsed. [`StopOnExit`]
/// sets the flag as the panicking worker unwinds, so the others leave the loop
/// after at most the batch already in hand; the scope then joins them and resumes
/// the panic, and unwinding drops the receiver, which releases a producer blocked
/// on `send`.
///
/// Setting the flag on a *normal* exit too is harmless and costs nothing: a
/// worker only exits normally on `RecvError`, which `std` returns once the
/// channel is both empty and disconnected, so there is nothing left to pull.
///
/// What this cannot shorten is a producer blocked inside `Read::read` on a source
/// that yields nothing further: the scope must join that thread, so the panic
/// surfaces when the read returns. See [`StreamReader::par_for_each`].
fn consume_batches<F>(rx: &Mutex<Receiver<Batch>>, workers: usize, f: &F)
where
    F: Fn(&Record) + Sync,
{
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        for _ in 0..workers {
            let stop = &stop;
            scope.spawn(move || {
                let _stop_siblings = StopOnExit(stop);
                while !stop.load(Ordering::Relaxed) {
                    let Ok(guard) = rx.lock() else { break };
                    let batch = guard.recv();
                    drop(guard);
                    let Ok(batch) = batch else { break }; // producer finished
                    for (index, span) in &batch.records {
                        let record =
                            Record::new(&batch.data[span.clone()], batch.prelude.clone(), *index);
                        f(&record);
                    }
                }
            });
        }
    });
}

/// Signals the sibling workers to stop as soon as one leaves its loop — including
/// by unwinding out of a panicking closure, which is the case that matters (see
/// [`consume_batches`]).
struct StopOnExit<'a>(&'a AtomicBool);

impl Drop for StopOnExit<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Event;
    use std::io;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    fn build_doc(n: usize) -> String {
        let mut s = String::from("<records>");
        for i in 0..n {
            s.push_str("<r>");
            s.push_str(&i.to_string());
            s.push_str("</r>");
        }
        s.push_str("</records>");
        s
    }

    fn record_value(rec: &Record) -> usize {
        let mut reader = rec.events();
        let mut text = String::new();
        while let Some(ev) = reader.next_event().unwrap() {
            if let Event::Text(t) = ev {
                text.push_str(&t);
            }
        }
        text.parse().unwrap()
    }

    /// Drain a stream into a sorted vec of per-record values (unordered output).
    fn collect_sorted(reader: StreamReader) -> Vec<usize> {
        let out = Mutex::new(Vec::new());
        reader
            .par_for_each(|rec| out.lock().unwrap().push(record_value(rec)))
            .unwrap();
        let mut values = out.into_inner().unwrap();
        values.sort_unstable();
        values
    }

    /// A reader that yields at most `step` bytes per `read`, to stress the
    /// producer's chunk boundaries through the real pipeline.
    struct Chunky<'a> {
        data: &'a [u8],
        pos: usize,
        step: usize,
    }

    impl io::Read for Chunky<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let remaining = &self.data[self.pos..];
            let k = remaining.len().min(buf.len()).min(self.step);
            buf[..k].copy_from_slice(&remaining[..k]);
            self.pos += k;
            Ok(k)
        }
    }

    #[test]
    fn streaming_matches_materialized_plain() {
        let n = 500;
        let xml = build_doc(n);
        let got = collect_sorted(StreamReader::from_reader(xml.as_bytes()));
        assert_eq!(got, (0..n).collect::<Vec<_>>());
    }

    #[test]
    fn streaming_survives_tiny_chunks() {
        let n = 50;
        let xml = build_doc(n);
        let reader = Chunky {
            data: xml.as_bytes(),
            pos: 0,
            step: 3,
        };
        let got = collect_sorted(StreamReader::from_reader(reader));
        assert_eq!(got, (0..n).collect::<Vec<_>>());
    }

    /// Every batch shape frames and parses the same records: one record per
    /// batch, a byte cap that trips first, a one-slot queue, and the defaults.
    #[test]
    fn streaming_batch_sizing_does_not_change_results() {
        let n = 300;
        let xml = build_doc(n);
        let expected: Vec<usize> = (0..n).collect();

        let configs = [
            Config::new().with_stream_batch_records(1),
            Config::new().with_stream_batch_bytes(1), // one record per batch
            Config::new()
                .with_stream_batch_records(7)
                .with_stream_batch_bytes(32),
            Config::new().with_stream_queue_capacity(1),
            Config::new()
                .with_stream_batch_records(usize::MAX)
                .with_stream_batch_bytes(usize::MAX),
            // `0` must not stall the producer: it means one record per batch.
            Config::new()
                .with_stream_batch_records(0)
                .with_stream_batch_bytes(0),
        ];

        for config in configs {
            let reader = StreamReader::from_reader(xml.as_bytes()).with_config(config.clone());
            assert_eq!(collect_sorted(reader), expected, "config: {config:?}");
        }
    }

    /// An explicit worker count drives the pipeline without consulting rayon,
    /// down to a single worker.
    #[test]
    fn streaming_honors_an_explicit_worker_count() {
        let n = 400;
        let xml = build_doc(n);
        let expected: Vec<usize> = (0..n).collect();

        for workers in [1, 2, 3] {
            let reader = StreamReader::from_reader(xml.as_bytes())
                .with_config(Config::new().with_stream_workers(workers));
            assert_eq!(collect_sorted(reader), expected, "workers: {workers}");
        }
    }

    /// A record larger than the byte cap is dispatched on its own rather than
    /// stalling the producer waiting for a batch that can never fill.
    #[test]
    fn streaming_record_larger_than_the_byte_cap_still_flows() {
        let big = "x".repeat(8 * 1024);
        let xml = format!("<rs><r>{big}</r><r>{big}</r></rs>");
        let lens = Mutex::new(Vec::new());

        StreamReader::from_reader(xml.as_bytes())
            .with_config(Config::new().with_stream_batch_bytes(64))
            .par_for_each(|rec| lens.lock().unwrap().push(rec.as_bytes().len()))
            .unwrap();

        let lens = lens.into_inner().unwrap();
        assert_eq!(lens.len(), 2);
        assert!(lens.iter().all(|&l| l == big.len() + 7)); // <r>…</r>
    }

    /// `with_config` carries the record path; a later `record_path` overrides it.
    #[test]
    fn stream_config_and_record_path_compose() {
        let n = 100;
        let xml = build_container_doc(n);
        let expected: Vec<usize> = (0..n).collect();

        let from_config = StreamReader::from_reader(xml.as_bytes())
            .with_config(Config::new().with_record_path(["objects"]));
        assert_eq!(collect_sorted(from_config), expected);

        let overridden = StreamReader::from_reader(xml.as_bytes())
            .with_config(Config::new().with_record_path(["nope"]))
            .record_path(["objects"]);
        assert_eq!(collect_sorted(overridden), expected);
    }

    /// A panicking closure propagates *and* stops the pipeline: without the stop
    /// flag the surviving workers keep pulling and the panic does not surface
    /// until the whole document has been parsed, so this asserts on how few
    /// records were seen, not just that the panic arrived.
    #[test]
    fn streaming_worker_panic_stops_the_pipeline() {
        const RECORDS: usize = 100_000;
        const WORKERS: usize = 4;

        let xml = build_doc(RECORDS);
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);

        // The default hook symbolizes a backtrace under `RUST_BACKTRACE=1`,
        // which runs *before* unwinding starts — long enough for the siblings to
        // drain a small document and turn this into a timing test. Silence it
        // for the duration, so what is measured is the stop flag.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(move || {
            // Only *one* record panics, so the other workers are alive and
            // pulling — exactly the case the flag has to interrupt.
            StreamReader::from_reader(xml.as_bytes())
                .with_config(Config::new().with_stream_workers(WORKERS))
                .par_for_each(move |rec| {
                    counter.fetch_add(1, Ordering::Relaxed);
                    assert!(rec.index() != 0, "closure failed");
                })
        });
        std::panic::set_hook(hook);

        assert!(result.is_err(), "the panic must reach the caller");
        // Each worker stops after at most the batch it already holds, so the
        // ceiling is a few batches — orders of magnitude below draining 100k.
        let seen = seen.load(Ordering::Relaxed);
        assert!(seen < 10_000, "drained {seen} records after the panic");
    }

    #[test]
    fn streaming_reports_unclosed_root() {
        let res = StreamReader::from_reader(&b"<r><a></a>"[..]).par_for_each(|_| {});
        assert!(res.is_err());
    }

    /// `<root><manifest>meta</manifest><objects><object>0</object>…</objects></root>`.
    fn build_container_doc(n: usize) -> String {
        let mut s = String::from("<root><manifest>meta</manifest><objects>");
        for i in 0..n {
            s.push_str("<object>");
            s.push_str(&i.to_string());
            s.push_str("</object>");
        }
        s.push_str("</objects></root>");
        s
    }

    #[test]
    fn streaming_record_path_matches_materialized() {
        let n = 500;
        let xml = build_container_doc(n);
        let got =
            collect_sorted(StreamReader::from_reader(xml.as_bytes()).record_path(["objects"]));
        assert_eq!(got, (0..n).collect::<Vec<_>>());
    }

    #[test]
    fn streaming_record_path_survives_tiny_chunks() {
        let n = 40;
        let xml = build_container_doc(n);
        let reader = Chunky {
            data: xml.as_bytes(),
            pos: 0,
            step: 3,
        };
        let got = collect_sorted(StreamReader::from_reader(reader).record_path(["objects"]));
        assert_eq!(got, (0..n).collect::<Vec<_>>());
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn streaming_zstd_matches_materialized() {
        let n = 800;
        let xml = build_doc(n);
        let compressed = zstd::encode_all(xml.as_bytes(), 3).unwrap();
        let reader = StreamReader::from_zstd_reader(&compressed[..]).unwrap();
        assert_eq!(collect_sorted(reader), (0..n).collect::<Vec<_>>());
    }
}
