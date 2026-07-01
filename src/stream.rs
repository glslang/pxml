//! Bounded-memory streaming pipeline.
//!
//! For inputs that shouldn't be fully materialized — a multi-GB compressed file,
//! or several at once — [`StreamReader`] decompresses and frames the document on
//! a single producer thread and parses the framed records in parallel on a
//! `rayon` pool. A bounded channel between them provides backpressure, so the
//! producer only runs ahead as far as the workers can drain: resident memory is
//! bounded by the in-flight records (≈ `threads × record_size`) plus one chunk,
//! independent of document size.
//!
//! Trade-offs vs. the resident [`ParallelXml`](crate::ParallelXml) path: records
//! are *owned* (copied out of the decompression buffer rather than borrowed), and
//! output is unordered. Decompression + framing remain sequential, so they bound
//! the achievable speedup (Amdahl).

use std::io::Read;
use std::ops::Range;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::thread;

use rayon::iter::{ParallelBridge, ParallelIterator};

use crate::scan::StreamFramer;
use crate::{Prelude, Record, XmlError};

/// Bytes pulled from the source per read.
const CHUNK: usize = 64 * 1024;

/// Records carried per channel message. Batching amortizes the channel send and
/// the `par_bridge` receiver mutex over many records, and packs a batch's record
/// bytes into a single arena allocation (one alloc per batch, not per record).
const BATCH: usize = 256;

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
pub struct StreamReader<'a> {
    reader: Box<dyn Read + Send + 'a>,
    /// Element-name path from the root to the record container (see
    /// [`ParallelXml::record_path`](crate::ParallelXml::record_path)); empty =
    /// the root's direct children.
    record_path: Vec<Box<str>>,
}

impl<'a> StreamReader<'a> {
    /// Stream over an already-decompressed byte source (any `Read`).
    pub fn from_reader<R: Read + Send + 'a>(reader: R) -> Self {
        Self {
            reader: Box::new(reader),
            record_path: Vec::new(),
        }
    }

    /// Stream over a zstd-compressed byte source, decompressing incrementally.
    #[cfg(feature = "zstd")]
    pub fn from_zstd_reader<R: Read + Send + 'a>(reader: R) -> std::io::Result<Self> {
        let decoder = zstd::Decoder::new(reader)?;
        Ok(Self {
            reader: Box::new(decoder),
            record_path: Vec::new(),
        })
    }

    /// Frame the direct children of the container reached by following `path`,
    /// skipping non-matching siblings — the streaming counterpart of
    /// [`ParallelXml::record_path`](crate::ParallelXml::record_path). Empty =
    /// the root's direct children (the default).
    pub fn record_path<I, S>(mut self, path: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Box<str>>,
    {
        self.record_path = path.into_iter().map(Into::into).collect();
        self
    }

    /// Frame records on a producer thread and apply `f` to each in parallel,
    /// in unordered (completion) order.
    ///
    /// Returns `Err` if framing or I/O fails; records already dispatched are
    /// still processed (siblings are not aborted). Per-record parse errors are
    /// the closure's concern (it drives `record.events()`).
    ///
    /// The workers run on rayon's current pool. To use a specific pool, wrap the
    /// call: `pool.install(|| reader.par_for_each(f))`.
    pub fn par_for_each<F>(self, f: F) -> Result<(), XmlError>
    where
        F: Fn(&Record) + Sync,
    {
        let mut reader = self.reader;
        let mut framer = StreamFramer::with_path(self.record_path);
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

        let capacity = (rayon::current_num_threads() * 2).max(1);
        let (tx, rx) = sync_channel::<Batch>(capacity);

        thread::scope(|scope| {
            let producer = scope.spawn(move || -> Result<(), XmlError> {
                let mut chunk = vec![0u8; CHUNK];
                loop {
                    // Pack up to BATCH records into one arena allocation.
                    let mut data = Vec::new();
                    let mut records = Vec::with_capacity(BATCH);
                    let mut need_more = false;
                    while records.len() < BATCH {
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

            // Workers pull whole batches and parse their records in parallel. The
            // bounded channel throttles the producer when the pool is saturated.
            rx.into_iter().par_bridge().for_each(|batch| {
                for (index, span) in &batch.records {
                    let record =
                        Record::new(&batch.data[span.clone()], batch.prelude.clone(), *index);
                    f(&record);
                }
            });

            producer.join().expect("producer thread panicked")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Event;
    use std::io;
    use std::sync::Mutex;

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
