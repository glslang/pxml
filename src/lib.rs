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

pub use config::Config;
pub use event::{Attrs, Event};
pub use parse::RecordReader;
pub use prelude::{Encoding, NamespaceContext, Prelude};
pub use scan::ChunkIndex;

use std::borrow::Cow;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

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

impl ParallelXml {
    /// Memory-map a file as the document buffer.
    pub fn from_path(p: &Path) -> std::io::Result<Self> {
        let file = std::fs::File::open(p)?;
        // SAFETY: the mapping is read-only; the caller is responsible for not
        // mutating or truncating the file while this `ParallelXml` is alive.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
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
        let _ = (&self.config, f);
        todo!("Phase B: rayon par_iter over the chunk index, with small-input fallback")
    }

    /// Parallel map + collect; preserves document order on output.
    pub fn map_collect<T, F>(&self, f: F) -> Result<Vec<T>, XmlError>
    where
        T: Send,
        F: Fn(&Record) -> T + Sync,
    {
        let _ = (&self.config, f);
        todo!("Phase B: indexed par_iter().map().collect(), short-circuit on first error")
    }

    /// Escape hatch: a plain sequential StAX reader over the whole document
    /// (the small-input fallback, and for classic-StAX consumers).
    pub fn sequential(&self) -> SeqReader<'_> {
        SeqReader {
            bytes: self.buf.as_slice(),
        }
    }
}

/// One top-level record: a self-contained pull reader over its slice.
pub struct Record<'doc> {
    bytes: &'doc [u8],
    prelude: Arc<Prelude>,
    index: usize,
}

impl<'doc> Record<'doc> {
    /// A StAX pull cursor over this record's events.
    pub fn events(&self) -> RecordReader<'doc> {
        RecordReader::new(self.bytes, self.prelude.clone())
    }

    /// This record's position in document order.
    pub fn index(&self) -> usize {
        self.index
    }
}

/// A sequential StAX reader over the whole document — the small-input fallback
/// and classic-StAX entry point.
pub struct SeqReader<'doc> {
    bytes: &'doc [u8],
}

impl<'doc> SeqReader<'doc> {
    /// Advance to the next event, or `Ok(None)` at the end of the document.
    pub fn next_event(&mut self) -> Result<Option<Event<'doc>>, XmlError> {
        let _ = self.bytes;
        todo!("sequential StAX pass over the whole buffer")
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
