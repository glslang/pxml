//! pxml — a parallel, StAX-style XML reader.
//!
//! Two-phase architecture: a cheap, single-threaded **boundary scan** (Phase A,
//! [`scan`]) frames the top-level records of a document and captures shared
//! prolog context, then an embarrassingly-parallel **per-record parse**
//! (Phase B, [`parse`]) runs on `rayon`. The soundness assumption is that
//! top-level elements (direct children of the root) are independent and may be
//! consumed in any order.
//!
//! See `DESIGN.md` for the full feasibility study and design spec.
//!
//! # Status
//!
//! Scaffold: the type definitions and public API surface are in place; the
//! phase bodies are `todo!()`. Implement in the order `scan.rs` -> `parse.rs`
//! -> `lib.rs`.

mod config;
mod event;
mod parse;
mod prelude;
mod scan;
mod stream;

pub use config::Config;
pub use event::{AttrIter, Attribute, Attrs, Event};
pub use parse::RecordReader;
pub use prelude::{Encoding, NamespaceContext, Prelude};
pub use scan::ChunkIndex;
pub use stream::StreamReader;

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use quick_xml::Reader;
use quick_xml::events::Event as QxEvent;
use rayon::prelude::*;

use crate::parse::map_event;
use crate::scan::parse_doctype_entities;

/// Owns the document buffer (heap `Vec` or `mmap`) plus a [`Config`], and is the
/// entry point to all parsing.
pub struct ParallelXml {
    buf: Buffer,
    config: Config,
}

/// Backing storage for the document bytes.
enum Buffer {
    /// An in-memory buffer (borrowed `'static` or owned).
    Owned(Cow<'static, [u8]>),
    /// A memory-mapped file.
    Mmap(memmap2::Mmap),
}

impl Buffer {
    fn as_slice(&self) -> &[u8] {
        match self {
            Buffer::Owned(b) => b,
            Buffer::Mmap(m) => m,
        }
    }
}

/// zstd frame magic number (`0xFD2FB528`, as it appears on the wire). A
/// well-formed XML document never begins with these bytes, so detection is
/// unambiguous.
#[cfg(feature = "zstd")]
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

#[cfg(feature = "zstd")]
fn is_zstd(bytes: &[u8]) -> bool {
    bytes.starts_with(&ZSTD_MAGIC)
}

impl ParallelXml {
    /// Memory-map a file as the document buffer.
    ///
    /// With the `zstd` feature (on by default), a zstd-compressed file is
    /// detected by its magic number and transparently decompressed into memory;
    /// a plain XML document (which never begins with the zstd magic) is mmap'd
    /// as before.
    pub fn from_path(p: &Path) -> std::io::Result<Self> {
        let file = std::fs::File::open(p)?;
        // SAFETY: the mapping is read-only; the caller is responsible for not
        // mutating or truncating the file while this `ParallelXml` is alive.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        #[cfg(feature = "zstd")]
        if is_zstd(&mmap) {
            let bytes = zstd::decode_all(&mmap[..])
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return Ok(Self::from_owned(bytes));
        }
        Ok(Self {
            buf: Buffer::Mmap(mmap),
            config: Config::default(),
        })
    }

    /// Use an in-memory buffer as the document.
    pub fn from_bytes(b: impl Into<Cow<'static, [u8]>>) -> Self {
        Self {
            buf: Buffer::Owned(b.into()),
            config: Config::default(),
        }
    }

    /// Decompress a zstd-compressed document from a reader into memory.
    ///
    /// Because parallel workers need random access to their slices, the whole
    /// document is decompressed up front. Decompression is sequential and adds
    /// to the serial fraction (see `DESIGN.md`).
    #[cfg(feature = "zstd")]
    pub fn from_zstd_reader(reader: impl std::io::Read) -> Result<Self, XmlError> {
        let bytes = zstd::decode_all(reader).map_err(XmlError::Io)?;
        Ok(Self::from_owned(bytes))
    }

    /// Decompress a zstd-compressed document from an in-memory buffer.
    #[cfg(feature = "zstd")]
    pub fn from_zstd_bytes(compressed: &[u8]) -> Result<Self, XmlError> {
        Self::from_zstd_reader(compressed)
    }

    /// Wrap an owned, decompressed buffer.
    #[cfg(feature = "zstd")]
    fn from_owned(bytes: Vec<u8>) -> Self {
        Self {
            buf: Buffer::Owned(Cow::Owned(bytes)),
            config: Config::default(),
        }
    }

    /// Override the default [`Config`].
    pub fn with_config(mut self, cfg: Config) -> Self {
        self.config = cfg;
        self
    }

    /// Phase A only — cheap; exposes record count / framing.
    pub fn index(&self) -> Result<ChunkIndex, XmlError> {
        scan::scan(self.buf.as_slice())
    }

    /// Unordered parallel map over records (the natural "any order" API).
    ///
    /// Falls back to a sequential pass for small inputs (see [`Config`]).
    pub fn par_for_each<F>(&self, f: F) -> Result<(), XmlError>
    where
        F: Fn(&Record) + Sync,
    {
        let buf = self.buf.as_slice();
        let index = scan::scan(buf)?;
        let prelude = &index.prelude;
        let make = |i: usize, r: &Range<usize>| Record {
            bytes: &buf[r.clone()],
            prelude: prelude.clone(),
            index: i,
        };
        if self.run_sequential(buf.len(), index.records.len()) {
            for (i, r) in index.records.iter().enumerate() {
                f(&make(i, r));
            }
        } else {
            index
                .records
                .par_iter()
                .enumerate()
                .for_each(|(i, r)| f(&make(i, r)));
        }
        Ok(())
    }

    /// Parallel map + collect; preserves document order on output.
    pub fn map_collect<T, F>(&self, f: F) -> Result<Vec<T>, XmlError>
    where
        T: Send,
        F: Fn(&Record) -> T + Sync,
    {
        let buf = self.buf.as_slice();
        let index = scan::scan(buf)?;
        let prelude = &index.prelude;
        let make = |i: usize, r: &Range<usize>| Record {
            bytes: &buf[r.clone()],
            prelude: prelude.clone(),
            index: i,
        };
        let out = if self.run_sequential(buf.len(), index.records.len()) {
            index
                .records
                .iter()
                .enumerate()
                .map(|(i, r)| f(&make(i, r)))
                .collect()
        } else {
            // `IndexedParallelIterator::collect` restores document order
            // regardless of the order records actually finish.
            index
                .records
                .par_iter()
                .enumerate()
                .map(|(i, r)| f(&make(i, r)))
                .collect()
        };
        Ok(out)
    }

    /// Whether to take the sequential path: small buffers or few records don't
    /// repay the thread-pool + indexing overhead (see [`Config`]).
    fn run_sequential(&self, byte_len: usize, record_count: usize) -> bool {
        byte_len < self.config.parallel_threshold || record_count < self.config.min_records
    }

    /// Escape hatch: a plain sequential StAX reader over the whole document
    /// (for classic-StAX consumers). Cheap to create — no Phase A scan.
    pub fn sequential(&self) -> SeqReader<'_> {
        SeqReader::new(self.buf.as_slice())
    }
}

/// One top-level record: a self-contained pull reader over its slice.
pub struct Record<'doc> {
    bytes: &'doc [u8],
    prelude: Arc<Prelude>,
    index: usize,
}

impl<'doc> Record<'doc> {
    pub(crate) fn new(bytes: &'doc [u8], prelude: Arc<Prelude>, index: usize) -> Self {
        Self {
            bytes,
            prelude,
            index,
        }
    }

    /// A StAX pull cursor over this record's events.
    pub fn events(&self) -> RecordReader<'doc> {
        RecordReader::new(self.bytes, self.prelude.clone(), self.index)
    }

    /// This record's position in document order.
    pub fn index(&self) -> usize {
        self.index
    }
}

/// A sequential StAX reader over the whole document — the classic-StAX entry
/// point. Unlike the record API it surfaces every element (including the root
/// and any depth-1 text); internal-subset `<!ENTITY>` definitions are captured
/// lazily from the DOCTYPE as the document is read.
///
/// As with [`RecordReader`], events are tied to the reader and namespace
/// prefixes are surfaced lexically.
pub struct SeqReader<'doc> {
    reader: Reader<&'doc [u8]>,
    current: Option<QxEvent<'doc>>,
    /// Holds the lazily-captured entity map (and otherwise-empty context) used
    /// to resolve entity references via the shared event mapper.
    prelude: Prelude,
}

impl<'doc> SeqReader<'doc> {
    fn new(bytes: &'doc [u8]) -> Self {
        let mut reader = Reader::from_reader(bytes);
        reader.config_mut().expand_empty_elements = true;
        Self {
            reader,
            current: None,
            prelude: Prelude {
                encoding: Encoding::Utf8,
                root_name: Box::default(),
                namespaces: NamespaceContext::new(),
                entities: HashMap::new(),
            },
        }
    }

    /// Advance to the next event, or `Ok(None)` at the end of the document.
    /// Comments, PIs, and the XML declaration are skipped; a DOCTYPE's internal
    /// `<!ENTITY>` definitions are captured for subsequent entity resolution.
    pub fn next_event(&mut self) -> Result<Option<Event<'_>>, XmlError> {
        loop {
            let ev = match self.reader.read_event() {
                Ok(ev) => ev,
                Err(_) => return Err(XmlError::Malformed(self.reader.buffer_position() as usize)),
            };
            match ev {
                QxEvent::Eof => return Ok(None),
                QxEvent::DocType(e) => {
                    parse_doctype_entities(&e, &mut self.prelude.entities);
                }
                QxEvent::Comment(_) | QxEvent::PI(_) | QxEvent::Decl(_) => {}
                keep => {
                    self.current = Some(keep);
                    break;
                }
            }
        }
        let event = map_event(self.current.as_ref().expect("event stored above"), &self.prelude, 0)?;
        Ok(Some(event))
    }
}

/// Errors produced while scanning or parsing.
#[derive(Debug)]
pub enum XmlError {
    /// Framing / well-formedness failure at a byte offset (Phase A).
    Malformed(usize),
    /// The declared encoding could not be resolved or transcoded to UTF-8.
    Encoding,
    /// An underlying I/O failure (e.g. opening or mapping the file).
    Io(std::io::Error),
    /// External DTDs / parameter entities — out of scope for v1.
    UnsupportedDtd,
    /// A failure parsing a single record (Phase B); carries its document index.
    RecordError {
        index: usize,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl fmt::Display for XmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            XmlError::Malformed(pos) => write!(f, "malformed XML at byte {pos}"),
            XmlError::Encoding => write!(f, "unsupported or unresolvable encoding"),
            XmlError::Io(e) => write!(f, "I/O error: {e}"),
            XmlError::UnsupportedDtd => {
                write!(f, "external DTD / parameter entities are not supported")
            }
            XmlError::RecordError { index, source } => {
                write!(f, "error in record {index}: {source}")
            }
        }
    }
}

impl std::error::Error for XmlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            XmlError::Io(e) => Some(e),
            XmlError::RecordError { source, .. } => Some(&**source),
            _ => None,
        }
    }
}

impl From<std::io::Error> for XmlError {
    fn from(e: std::io::Error) -> Self {
        XmlError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `<records><r>0</r><r>1</r>…</records>` with `n` records.
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

    /// Concatenated text of a record.
    fn record_text(rec: &Record) -> String {
        let mut reader = rec.events();
        let mut out = String::new();
        while let Some(ev) = reader.next_event().unwrap() {
            if let Event::Text(t) = ev {
                out.push_str(&t);
            }
        }
        out
    }

    /// Config that forces the parallel path regardless of input size.
    fn force_parallel() -> Config {
        Config {
            parallel_threshold: 0,
            min_records: 0,
            ..Config::default()
        }
    }

    #[test]
    fn map_collect_preserves_document_order() {
        let n = 2000;
        let px = ParallelXml::from_bytes(build_doc(n).into_bytes()).with_config(force_parallel());
        let got: Vec<usize> = px.map_collect(|rec| record_text(rec).parse().unwrap()).unwrap();
        assert_eq!(got, (0..n).collect::<Vec<_>>());
    }

    #[test]
    fn par_for_each_visits_every_record() {
        let n = 1000;
        let px = ParallelXml::from_bytes(build_doc(n).into_bytes()).with_config(force_parallel());
        let sum = AtomicUsize::new(0);
        let count = AtomicUsize::new(0);
        px.par_for_each(|rec| {
            sum.fetch_add(rec.index(), Ordering::Relaxed);
            count.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
        assert_eq!(count.load(Ordering::Relaxed), n);
        assert_eq!(sum.load(Ordering::Relaxed), n * (n - 1) / 2);
    }

    #[test]
    fn small_input_fallback_matches_parallel() {
        let n = 200;
        let xml = build_doc(n);
        // Default config: small buffer takes the sequential fallback.
        let seq: Vec<usize> = ParallelXml::from_bytes(xml.clone().into_bytes())
            .map_collect(|rec| record_text(rec).parse().unwrap())
            .unwrap();
        let par: Vec<usize> = ParallelXml::from_bytes(xml.into_bytes())
            .with_config(force_parallel())
            .map_collect(|rec| record_text(rec).parse().unwrap())
            .unwrap();
        assert_eq!(seq, par);
        assert_eq!(seq, (0..n).collect::<Vec<_>>());
    }

    #[test]
    fn map_collect_reports_scan_error() {
        let px = ParallelXml::from_bytes(&b"<r><a></r>"[..]);
        assert!(px.map_collect(|_| ()).is_err());
    }

    #[test]
    fn index_exposes_record_count() {
        let px = ParallelXml::from_bytes(build_doc(5).into_bytes());
        let idx = px.index().unwrap();
        assert_eq!(idx.len(), 5);
    }

    #[test]
    fn seq_reader_emits_all_events() {
        let px = ParallelXml::from_bytes(&b"<r><a>x</a><b/></r>"[..]);
        let mut sr = px.sequential();
        let mut tags = Vec::new();
        while let Some(ev) = sr.next_event().unwrap() {
            tags.push(match ev {
                Event::Start { name, .. } => {
                    format!("S:{}", std::str::from_utf8(name.as_ref()).unwrap())
                }
                Event::End { name } => {
                    format!("E:{}", std::str::from_utf8(name.as_ref()).unwrap())
                }
                Event::Text(t) => format!("T:{t}"),
                Event::Cdata(c) => format!("C:{}", std::str::from_utf8(c).unwrap()),
            });
        }
        assert_eq!(
            tags,
            ["S:r", "S:a", "T:x", "E:a", "S:b", "E:b", "E:r"]
        );
    }

    #[test]
    fn seq_reader_resolves_doctype_entities() {
        let doc = br#"<!DOCTYPE r [ <!ENTITY foo "BAR"> ]><r>&foo; &amp; baz</r>"#;
        let px = ParallelXml::from_bytes(&doc[..]);
        let mut sr = px.sequential();
        let mut text = String::new();
        while let Some(ev) = sr.next_event().unwrap() {
            if let Event::Text(t) = ev {
                text.push_str(&t);
            }
        }
        assert_eq!(text, "BAR & baz");
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::AtomicUsize;
        static N: AtomicUsize = AtomicUsize::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("pxml-{tag}-{}-{id}.bin", std::process::id()))
    }

    #[test]
    fn from_path_reads_plain_xml() {
        let path = temp_path("plain");
        std::fs::write(&path, build_doc(40)).unwrap();
        let doc = ParallelXml::from_path(&path).unwrap();
        assert_eq!(doc.index().unwrap().len(), 40);
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_bytes_roundtrip() {
        let n = 500;
        let xml = build_doc(n);
        let compressed = zstd::encode_all(xml.as_bytes(), 3).unwrap();
        assert!(compressed.len() < xml.len(), "input should actually compress");

        let doc = ParallelXml::from_zstd_bytes(&compressed).unwrap();
        let got: Vec<usize> = doc.map_collect(|r| record_text(r).parse().unwrap()).unwrap();
        assert_eq!(got, (0..n).collect::<Vec<_>>());
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn from_path_detects_and_decompresses_zstd() {
        let n = 120;
        let compressed = zstd::encode_all(build_doc(n).as_bytes(), 3).unwrap();
        let path = temp_path("zstd");
        std::fs::write(&path, &compressed).unwrap();
        let doc = ParallelXml::from_path(&path).unwrap();
        assert_eq!(doc.index().unwrap().len(), n);
        let _ = std::fs::remove_file(&path);
    }
}
