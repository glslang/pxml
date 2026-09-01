//! pxml — a parallel, StAX-style (pull) XML reader.
//!
//! `pxml` targets one shape of document: a single root containing **thousands of
//! uniform, order-independent records** — e.g. `<trades><trade>…</trade>…</trades>`.
//!
//! Two-phase architecture: a cheap, single-threaded **boundary scan** (Phase A)
//! frames the records and captures shared prolog context, then an
//! embarrassingly-parallel **per-record parse** (Phase B) runs on `rayon`. The
//! soundness assumption is that the framed records are independent and may be
//! consumed in any order.
//!
//! # Quick start
//!
//! Parse in parallel, collecting results back into document order:
//!
//! ```
//! use pxml::{Event, ParallelXml};
//!
//! let xml = b"<trades><trade id='1'/><trade id='2'/></trades>".to_vec();
//! let doc = ParallelXml::from_bytes(xml);
//!
//! let ids: Vec<String> = doc.map_collect(|record| {
//!     let mut events = record.events();
//!     let mut id = String::new();
//!     while let Some(ev) = events.next_event().unwrap() {
//!         if let Event::Start { attrs, .. } = ev {
//!             for attr in attrs.iter() {
//!                 let attr = attr.unwrap();
//!                 if attr.key == b"id" {
//!                     id = attr.value.into_owned();
//!                 }
//!             }
//!         }
//!     }
//!     id
//! })?;
//!
//! assert_eq!(ids, ["1", "2"]);
//! # Ok::<(), pxml::XmlError>(())
//! ```
//!
//! # Choosing an entry point
//!
//! | Need | Use |
//! |------|-----|
//! | A file or in-memory buffer, results in document order | [`ParallelXml::map_collect`] |
//! | A file or in-memory buffer, order irrelevant | [`ParallelXml::par_for_each`] |
//! | The fallible variants (closure returns `Result`) | [`ParallelXml::try_map_collect`] / [`ParallelXml::try_par_for_each`] |
//! | Record count / byte ranges only, no parsing | [`ParallelXml::index`] |
//! | Bounded memory over a huge or compressed stream | [`StreamReader`] |
//! | A classic whole-document StAX cursor | [`ParallelXml::sequential`] |
//!
//! Records that are not the root's direct children — e.g. the `<object>`s in
//! `<root><manifest/><objects><object/>…</objects></root>` — are reached by
//! setting [`Config::with_record_path`] and passing it to
//! [`ParallelXml::with_config`].
//!
//! # Limitations
//!
//! Records must be **order-independent**: a worker sees only its own record, so
//! cross-record state (an accumulator, a reference to an earlier record) is not
//! available during the parallel pass. External DTDs and parameter entities are
//! rejected with [`XmlError::UnsupportedDtd`]; non-UTF-8 input is rejected with
//! [`XmlError::Encoding`].
//!
//! The design rationale is in `DESIGN.md`; the decisions and trade-offs actually
//! made are in `DECISIONS.md`, which supersedes it.

#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]
// docs.rs builds with `--cfg docsrs` (see Cargo.toml) so feature-gated items are
// labelled with the feature that enables them. No effect on a stable build.
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Compiles every Rust block in `README.md` as a doctest, so the front-page
/// examples cannot drift from the API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
mod readme {}

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

use crate::parse::{append_run_event, is_text_run, map_event, text_content};
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

    /// How the bytes are held, for `Debug` output.
    fn kind(&self) -> &'static str {
        match self {
            Buffer::Owned(_) => "owned",
            Buffer::Mmap(_) => "mmap",
        }
    }
}

// Documents are large; `Debug` summarizes rather than dumping the buffer.
impl fmt::Debug for ParallelXml {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParallelXml")
            .field("buffer", &self.buf.kind())
            .field("len", &self.buf.as_slice().len())
            .field("config", &self.config)
            .finish()
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
    #[cfg_attr(docsrs, doc(cfg(feature = "zstd")))]
    pub fn from_zstd_reader(reader: impl std::io::Read) -> Result<Self, XmlError> {
        let bytes = zstd::decode_all(reader).map_err(XmlError::Io)?;
        Ok(Self::from_owned(bytes))
    }

    /// Decompress a zstd-compressed document from an in-memory buffer.
    #[cfg(feature = "zstd")]
    #[cfg_attr(docsrs, doc(cfg(feature = "zstd")))]
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

    /// Override the default [`Config`] — the single place to set both the
    /// parallelism thresholds and the [record path](Config::with_record_path).
    ///
    /// Replaces the whole configuration rather than merging into it.
    ///
    /// ```
    /// use pxml::{Config, ParallelXml};
    ///
    /// let xml = b"<root><manifest>meta</manifest>\
    ///             <objects><object/><object/><object/></objects></root>".to_vec();
    ///
    /// // By default the records are the root's direct children:
    /// // <manifest> and <objects>.
    /// assert_eq!(ParallelXml::from_bytes(xml.clone()).index()?.len(), 2);
    ///
    /// // With a record path, <manifest> is skipped and each <object> is a record.
    /// let doc = ParallelXml::from_bytes(xml)
    ///     .with_config(Config::new().with_record_path(["objects"]));
    /// assert_eq!(doc.index()?.len(), 3);
    /// # Ok::<(), pxml::XmlError>(())
    /// ```
    pub fn with_config(mut self, cfg: Config) -> Self {
        self.config = cfg;
        self
    }

    /// Phase A only — cheap; exposes record count / framing.
    ///
    /// Runs the boundary scan without parsing any record, so it is a fast way to
    /// count records or to get their byte ranges for custom dispatch.
    ///
    /// ```
    /// use pxml::ParallelXml;
    ///
    /// let doc = ParallelXml::from_bytes(&b"<rs><r>a</r><r>b</r></rs>"[..]);
    /// let idx = doc.index()?;
    ///
    /// assert_eq!(idx.len(), 2);
    /// assert_eq!(idx.records()[0], 4..12); // the bytes of `<r>a</r>`
    /// assert_eq!(idx.prelude().root_name.as_ref(), "rs");
    /// # Ok::<(), pxml::XmlError>(())
    /// ```
    pub fn index(&self) -> Result<ChunkIndex, XmlError> {
        scan::scan_with(self.buf.as_slice(), &self.config.record_path)
    }

    /// Unordered parallel map over records (the natural "any order" API).
    ///
    /// Falls back to a sequential pass for small inputs (see [`Config`]). The
    /// closure must be `Sync`, so shared state needs a lock or an atomic.
    ///
    /// ```
    /// use pxml::{Event, ParallelXml};
    /// use std::sync::atomic::{AtomicUsize, Ordering};
    ///
    /// let doc = ParallelXml::from_bytes(&b"<rs><r>a</r><r>b</r><r>c</r></rs>"[..]);
    /// let seen = AtomicUsize::new(0);
    ///
    /// doc.par_for_each(|record| {
    ///     let mut events = record.events();
    ///     while let Some(ev) = events.next_event().unwrap() {
    ///         if matches!(ev, Event::Text(_)) {
    ///             seen.fetch_add(1, Ordering::Relaxed);
    ///         }
    ///     }
    /// })?;
    ///
    /// assert_eq!(seen.load(Ordering::Relaxed), 3);
    /// # Ok::<(), pxml::XmlError>(())
    /// ```
    pub fn par_for_each<F>(&self, f: F) -> Result<(), XmlError>
    where
        F: Fn(&Record) + Sync,
    {
        let buf = self.buf.as_slice();
        let index = scan::scan_with(buf, &self.config.record_path)?;
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
        let index = scan::scan_with(buf, &self.config.record_path)?;
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

    /// Like [`par_for_each`](Self::par_for_each), but the closure returns a
    /// `Result`. A record failure is wrapped as
    /// [`XmlError::RecordError`]`{ index, source }`; the call short-circuits on
    /// the first error (in completion order).
    pub fn try_par_for_each<F, E>(&self, f: F) -> Result<(), XmlError>
    where
        F: Fn(&Record) -> Result<(), E> + Sync,
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let buf = self.buf.as_slice();
        let index = scan::scan_with(buf, &self.config.record_path)?;
        let prelude = &index.prelude;
        let one = |i: usize, r: &Range<usize>| {
            let rec = Record {
                bytes: &buf[r.clone()],
                prelude: prelude.clone(),
                index: i,
            };
            f(&rec).map_err(|e| XmlError::RecordError {
                index: i,
                source: e.into(),
            })
        };
        if self.run_sequential(buf.len(), index.records.len()) {
            index
                .records
                .iter()
                .enumerate()
                .try_for_each(|(i, r)| one(i, r))
        } else {
            index
                .records
                .par_iter()
                .enumerate()
                .try_for_each(|(i, r)| one(i, r))
        }
    }

    /// Like [`map_collect`](Self::map_collect), but the closure returns a
    /// `Result`. On success the output is in document order; a record failure is
    /// wrapped as [`XmlError::RecordError`]`{ index, source }` carrying the
    /// failing record's position.
    ///
    /// ```
    /// use pxml::{Event, ParallelXml, XmlError};
    ///
    /// let doc = ParallelXml::from_bytes(&b"<rs><r>1</r><r>oops</r></rs>"[..]);
    ///
    /// let res = doc.try_map_collect(|record| {
    ///     let mut events = record.events();
    ///     let mut text = String::new();
    ///     while let Some(ev) = events.next_event()? {
    ///         if let Event::Text(t) = ev {
    ///             text.push_str(&t);
    ///         }
    ///     }
    ///     text.parse::<u32>()
    ///         .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    /// });
    ///
    /// // The index identifies which record failed.
    /// assert!(matches!(res, Err(XmlError::RecordError { index: 1, .. })));
    /// ```
    ///
    /// # Short-circuiting differs by path
    ///
    /// The **sequential fallback** stops at the first error, but the **parallel
    /// path does not** — because building an ordered `Vec<T>` means processing
    /// every record, rayon runs the closure for all records and then returns one
    /// of the resulting errors. Two consequences: the closure may be called for
    /// records after a failing one, and when several records fail, *which* error
    /// surfaces is not deterministic. Use
    /// [`try_par_for_each`](Self::try_par_for_each) when you need early
    /// termination rather than a collected result.
    pub fn try_map_collect<T, F, E>(&self, f: F) -> Result<Vec<T>, XmlError>
    where
        T: Send,
        F: Fn(&Record) -> Result<T, E> + Sync,
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let buf = self.buf.as_slice();
        let index = scan::scan_with(buf, &self.config.record_path)?;
        let prelude = &index.prelude;
        let one = |i: usize, r: &Range<usize>| {
            let rec = Record {
                bytes: &buf[r.clone()],
                prelude: prelude.clone(),
                index: i,
            };
            f(&rec).map_err(|e| XmlError::RecordError {
                index: i,
                source: e.into(),
            })
        };
        if self.run_sequential(buf.len(), index.records.len()) {
            index
                .records
                .iter()
                .enumerate()
                .map(|(i, r)| one(i, r))
                .collect()
        } else {
            index
                .records
                .par_iter()
                .enumerate()
                .map(|(i, r)| one(i, r))
                .collect()
        }
    }

    /// Whether to take the sequential path: small buffers or few records don't
    /// repay the thread-pool + indexing overhead (see [`Config`]).
    fn run_sequential(&self, byte_len: usize, record_count: usize) -> bool {
        byte_len < self.config.parallel_threshold || record_count < self.config.min_records
    }

    /// Escape hatch: a plain sequential StAX reader over the whole document
    /// (for classic-StAX consumers). Cheap to create — no Phase A scan.
    ///
    /// Unlike the record API this surfaces *every* element, including the root.
    /// Use it when records are not order-independent, or when you want a
    /// conventional single-cursor reader.
    ///
    /// ```
    /// use pxml::{Event, ParallelXml};
    ///
    /// let doc = ParallelXml::from_bytes(&b"<rs><r>a</r></rs>"[..]);
    /// let mut reader = doc.sequential();
    ///
    /// let mut names = Vec::new();
    /// while let Some(ev) = reader.next_event()? {
    ///     if let Event::Start { name, .. } = ev {
    ///         names.push(name.as_ref().to_owned());
    ///     }
    /// }
    ///
    /// assert_eq!(names, ["rs", "r"]); // the root is included
    /// # Ok::<(), pxml::XmlError>(())
    /// ```
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

    /// This record's raw, undecoded bytes — the exact slice framed by Phase A,
    /// from its start tag through its end tag.
    ///
    /// Useful for forwarding a record verbatim (to a queue, or to another
    /// parser) without walking its events.
    ///
    /// ```
    /// use pxml::ParallelXml;
    ///
    /// let doc = ParallelXml::from_bytes(&b"<rs><r>a</r><r>b</r></rs>"[..]);
    /// let raw: Vec<Vec<u8>> = doc.map_collect(|rec| rec.as_bytes().to_vec())?;
    ///
    /// assert_eq!(raw[0], b"<r>a</r>");
    /// # Ok::<(), pxml::XmlError>(())
    /// ```
    pub fn as_bytes(&self) -> &'doc [u8] {
        self.bytes
    }
}

impl fmt::Debug for Record<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Record")
            .field("index", &self.index)
            .field("len", &self.bytes.len())
            .finish()
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
    /// One-slot lookahead: an event already read but not yet surfaced (the
    /// structural event that terminated a coalesced text run).
    pending: Option<QxEvent<'doc>>,
    /// Holds the lazily-captured entity map (and otherwise-empty context) used
    /// to resolve entity references via the shared event mapper.
    prelude: Prelude,
}

impl fmt::Debug for SeqReader<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SeqReader")
            .field("position", &self.reader.buffer_position())
            .finish_non_exhaustive()
    }
}

impl<'doc> SeqReader<'doc> {
    fn new(bytes: &'doc [u8]) -> Self {
        let mut reader = Reader::from_reader(bytes);
        reader.config_mut().expand_empty_elements = true;
        Self {
            reader,
            current: None,
            pending: None,
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
        let Some(ev) = self.next_surfaced()? else {
            return Ok(None);
        };

        if is_text_run(&ev) {
            // Peek the *immediately* following event (raw, skipping nothing): a
            // comment or PI between two text nodes is a boundary, so the run must
            // not coalesce across it. The terminator is buffered in `pending`;
            // the next call's skip loop drops it if it is ignorable markup.
            let next = self.read_raw()?;
            // Fast path: a lone literal text node, decoded straight from the
            // document buffer (zero-copy for UTF-8).
            let lone_literal =
                matches!(ev, QxEvent::Text(_)) && !next.as_ref().is_some_and(is_text_run);
            if lone_literal {
                let QxEvent::Text(e) = ev else {
                    unreachable!("checked Text above")
                };
                let text = text_content(e);
                self.pending = next;
                return Ok(Some(Event::Text(text)));
            }
            // Otherwise coalesce the run into one owned, fully-resolved string.
            let mut out = String::new();
            append_run_event(&mut out, &ev, &self.prelude, 0)?;
            let mut cur = next;
            while let Some(ev) = cur {
                if !is_text_run(&ev) {
                    self.pending = Some(ev);
                    break;
                }
                append_run_event(&mut out, &ev, &self.prelude, 0)?;
                cur = self.read_raw()?;
            }
            return Ok(Some(Event::Text(Cow::Owned(out))));
        }

        self.current = Some(ev);
        let event = map_event(
            self.current.as_ref().expect("event stored above"),
            &self.prelude,
            0,
        )?;
        Ok(Some(event))
    }

    /// Read the next *surfaced* event, draining the lookahead buffer first and
    /// skipping comments, PIs, and the XML declaration, while capturing a
    /// DOCTYPE's internal `<!ENTITY>` definitions. Used only to start an event;
    /// the text-run lookahead uses [`read_raw`](Self::read_raw) so skipped markup
    /// still bounds a text node. `Ok(None)` at end of input.
    fn next_surfaced(&mut self) -> Result<Option<QxEvent<'doc>>, XmlError> {
        loop {
            match self.read_raw()? {
                None => return Ok(None),
                Some(QxEvent::DocType(e)) => {
                    parse_doctype_entities(e.as_bytes(), &mut self.prelude.entities);
                }
                Some(QxEvent::Comment(_) | QxEvent::PI(_) | QxEvent::Decl(_)) => continue,
                Some(keep) => return Ok(Some(keep)),
            }
        }
    }

    /// Read one raw event — from the one-slot lookahead buffer if present,
    /// otherwise straight from the reader, skipping nothing. `Ok(None)` at Eof.
    fn read_raw(&mut self) -> Result<Option<QxEvent<'doc>>, XmlError> {
        if let Some(ev) = self.pending.take() {
            return Ok(Some(ev));
        }
        match self.reader.read_event() {
            Ok(QxEvent::Eof) => Ok(None),
            Ok(ev) => Ok(Some(ev)),
            Err(_) => Err(XmlError::Malformed(self.reader.buffer_position() as usize)),
        }
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
        /// The failing record's position in document order.
        index: usize,
        /// The underlying failure — a parse error, or an error returned by the
        /// user's closure from one of the `try_*` drivers.
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
        Config::new().with_parallel_threshold(0).with_min_records(0)
    }

    /// `<root><manifest>meta</manifest><objects><object>0</object>…</objects></root>`
    /// — records wrapped one level down, alongside a sibling to be skipped.
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
    fn record_path_frames_container_children_in_order() {
        let n = 2000;
        let px = ParallelXml::from_bytes(build_container_doc(n).into_bytes())
            .with_config(force_parallel().with_record_path(["objects"]));
        let got: Vec<usize> = px
            .map_collect(|rec| record_text(rec).parse().unwrap())
            .unwrap();
        assert_eq!(got, (0..n).collect::<Vec<_>>());
    }

    #[test]
    fn record_path_sequential_fallback_matches_parallel() {
        let n = 100; // small: default config takes the sequential fallback
        let xml = build_container_doc(n);
        let seq: Vec<usize> = ParallelXml::from_bytes(xml.clone().into_bytes())
            .with_config(Config::new().with_record_path(["objects"]))
            .map_collect(|rec| record_text(rec).parse().unwrap())
            .unwrap();
        let par: Vec<usize> = ParallelXml::from_bytes(xml.into_bytes())
            .with_config(force_parallel().with_record_path(["objects"]))
            .map_collect(|rec| record_text(rec).parse().unwrap())
            .unwrap();
        assert_eq!(seq, par);
        assert_eq!(seq, (0..n).collect::<Vec<_>>());
    }

    #[test]
    fn map_collect_preserves_document_order() {
        let n = 2000;
        let px = ParallelXml::from_bytes(build_doc(n).into_bytes()).with_config(force_parallel());
        let got: Vec<usize> = px
            .map_collect(|rec| record_text(rec).parse().unwrap())
            .unwrap();
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
    fn try_map_collect_ok_preserves_order() {
        let n = 300;
        let px = ParallelXml::from_bytes(build_doc(n).into_bytes()).with_config(force_parallel());
        let got: Vec<usize> = px
            .try_map_collect(|rec| record_text(rec).parse::<usize>())
            .unwrap();
        assert_eq!(got, (0..n).collect::<Vec<_>>());
    }

    #[test]
    fn try_map_collect_propagates_record_error() {
        let xml = "<records><r>0</r><r>NaN</r><r>2</r></records>";
        let px = ParallelXml::from_bytes(xml.as_bytes().to_vec()).with_config(force_parallel());
        let res = px.try_map_collect(|rec| record_text(rec).parse::<usize>());
        assert!(matches!(res, Err(XmlError::RecordError { index: 1, .. })));
    }

    #[test]
    fn try_par_for_each_surfaces_record_error() {
        let xml = "<records><r>0</r><r>oops</r></records>";
        let px = ParallelXml::from_bytes(xml.as_bytes().to_vec());
        let res = px.try_par_for_each(|rec| record_text(rec).parse::<usize>().map(|_| ()));
        assert!(matches!(res, Err(XmlError::RecordError { index: 1, .. })));
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
                    format!("S:{}", name.as_ref())
                }
                Event::End { name } => {
                    format!("E:{}", name.as_ref())
                }
                Event::Text(t) => format!("T:{t}"),
                Event::Cdata(c) => format!("C:{}", std::str::from_utf8(c).unwrap()),
            });
        }
        assert_eq!(tags, ["S:r", "S:a", "T:x", "E:a", "S:b", "E:b", "E:r"]);
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
        assert!(
            compressed.len() < xml.len(),
            "input should actually compress"
        );

        let doc = ParallelXml::from_zstd_bytes(&compressed).unwrap();
        let got: Vec<usize> = doc
            .map_collect(|r| record_text(r).parse().unwrap())
            .unwrap();
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
