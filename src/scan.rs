//! Phase A — sequential boundary scan (the one hand-written piece).
//!
//! Walks the buffer once with a tiny state machine to find depth-1 element
//! boundaries and capture shared prolog context. It builds no tree, decodes no
//! entities, and validates nothing beyond framing / well-formedness. Output is a
//! byte [`Range`] per top-level record plus the shared [`Prelude`].

use std::ops::Range;
use std::sync::Arc;

use crate::XmlError;
use crate::prelude::Prelude;

/// Lexical state of the boundary scanner.
///
/// Scaffold placeholder: the transition table is implemented in Phase A.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Character data between tags.
    Text,
    /// Inside a start/end tag, outside any attribute value.
    InTag,
    /// Inside an attribute value; holds the opening quote byte (`"` or `'`).
    InAttrValue(u8),
    /// Inside a `<!-- … -->` comment.
    Comment,
    /// Inside a `<![CDATA[ … ]]>` section.
    Cdata,
    /// Inside a `<? … ?>` processing instruction.
    Pi,
    /// Inside a `<!DOCTYPE … >` declaration (including the internal subset).
    Doctype,
}

/// Phase A output: framing only, no parsing.
#[derive(Debug)]
pub struct ChunkIndex {
    pub(crate) prelude: Arc<Prelude>,
    pub(crate) records: Vec<Range<usize>>,
}

impl ChunkIndex {
    /// Shared, immutable prolog context for every record.
    pub fn prelude(&self) -> &Arc<Prelude> {
        &self.prelude
    }

    /// Byte ranges of the top-level records, in document order.
    pub fn records(&self) -> &[Range<usize>] {
        &self.records
    }

    /// Number of top-level records found.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the document contains no top-level records.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// Run the Phase A boundary scan over the whole document buffer.
///
/// Algorithm (see `DESIGN.md`, "Phase A scanner"):
/// 1. Parse the prolog (`<?xml?>`, optional `<!DOCTYPE>` with internal
///    `<!ENTITY>` defs); stop at the root start tag, capturing its namespace
///    declarations into the [`Prelude`]; mark the prelude end offset.
/// 2. With `depth == 1` inside the root, frame each depth-1 element: remember
///    `start` on `depth 1 -> 2`, emit `start..cursor` when returning to depth 1.
/// 3. Use `memchr3(b'<', b'>', quote)` to jump between delimiters.
/// 4. On EOF expect `depth == 0`, else [`XmlError::Malformed`].
pub fn scan(buf: &[u8]) -> Result<ChunkIndex, XmlError> {
    let _ = (buf, State::Text);
    todo!("Phase A boundary scanner")
}
