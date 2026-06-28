//! Phase A — sequential boundary scan (the one hand-written piece).
//!
//! Walks the buffer once, jumping between delimiters with `memchr`, to find
//! depth-1 element boundaries and capture shared prolog context. It builds no
//! tree, decodes no entities, and validates nothing beyond framing /
//! well-formedness. Output is a byte [`Range`] per top-level record plus the
//! shared [`Prelude`].
//!
//! The conceptual lexical states described in `DESIGN.md` (text, in-tag,
//! in-attr-value, comment, CDATA, PI, doctype) are handled by the dedicated
//! `skip_*` / `scan_*` helpers below rather than a single state variable.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use memchr::{memchr, memchr3, memmem};

use crate::XmlError;
use crate::prelude::{Encoding, NamespaceContext, Prelude};

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
///    declarations into the [`Prelude`].
/// 2. With `depth == 1` inside the root, frame each depth-1 element: remember
///    `start` on `depth 1 -> 2`, emit `start..cursor` when returning to depth 1.
/// 3. Use `memchr` to jump between delimiters.
/// 4. On EOF expect the root to be closed, else [`XmlError::Malformed`].
pub fn scan(buf: &[u8]) -> Result<ChunkIndex, XmlError> {
    Scanner { buf, pos: 0 }.run()
}

struct Scanner<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn run(mut self) -> Result<ChunkIndex, XmlError> {
        let encoding = self.handle_bom_and_decl()?;
        let mut entities: HashMap<Box<str>, Box<str>> = HashMap::new();
        self.skip_prolog_misc(&mut entities)?;

        // Cursor is now at the root start tag's '<'.
        let (root_name, namespaces, self_closing) = self.parse_root()?;

        let mut records = Vec::new();
        if !self_closing {
            self.scan_content(&mut records, root_name.as_bytes())?;
        }
        self.skip_trailing_misc()?;

        let prelude = Arc::new(Prelude {
            encoding,
            root_name,
            namespaces,
            entities,
        });
        Ok(ChunkIndex { prelude, records })
    }

    // --- Prolog -----------------------------------------------------------

    /// Skip a leading BOM and parse the optional XML declaration; returns the
    /// resolved encoding (v1 asserts UTF-8 rather than transcoding).
    fn handle_bom_and_decl(&mut self) -> Result<Encoding, XmlError> {
        if self.buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
            self.pos = 3; // UTF-8 BOM
        } else if self.buf.starts_with(&[0xFF, 0xFE]) || self.buf.starts_with(&[0xFE, 0xFF]) {
            return Err(XmlError::Encoding); // UTF-16 — not transcoded in v1
        }

        let after = self.buf.get(self.pos + 5).copied();
        let is_decl = self.buf[self.pos..].starts_with(b"<?xml")
            && after.is_some_and(|c| is_xml_ws(c) || c == b'?');
        if is_decl {
            let start = self.pos + 5;
            let end_off = memmem::find(&self.buf[start..], b"?>")
                .ok_or(XmlError::Malformed(self.pos))?;
            let decl = &self.buf[start..start + end_off];
            if let Some(enc) = pseudo_attr(decl, b"encoding")
                && !enc.eq_ignore_ascii_case(b"utf-8")
                && !enc.eq_ignore_ascii_case(b"us-ascii")
            {
                return Err(XmlError::Encoding);
            }
            self.pos = start + end_off + 2;
        }
        Ok(Encoding::Utf8)
    }

    /// Skip whitespace / comments / PIs and parse an optional DOCTYPE until the
    /// cursor reaches the root start tag.
    fn skip_prolog_misc(&mut self, entities: &mut HashMap<Box<str>, Box<str>>) -> Result<(), XmlError> {
        loop {
            self.skip_ws();
            let rest = &self.buf[self.pos..];
            if rest.is_empty() {
                return Err(XmlError::Malformed(self.pos)); // no root element
            }
            if rest.starts_with(b"<!--") {
                self.skip_comment()?;
            } else if rest.starts_with(b"<!DOCTYPE") {
                self.parse_doctype(entities)?;
            } else if rest.starts_with(b"<?") {
                self.skip_pi()?;
            } else if rest[0] == b'<' && rest.len() >= 2 && is_name_start(rest[1]) {
                return Ok(()); // at the root start tag
            } else {
                return Err(XmlError::Malformed(self.pos));
            }
        }
    }

    /// Parse a DOCTYPE, capturing internal-subset `<!ENTITY>` definitions. An
    /// external DTD (`SYSTEM`/`PUBLIC`) is rejected with
    /// [`XmlError::UnsupportedDtd`] rather than silently skipped, since we can't
    /// resolve the global entities it may declare.
    fn parse_doctype(&mut self, entities: &mut HashMap<Box<str>, Box<str>>) -> Result<(), XmlError> {
        let n = self.buf.len();
        let mut i = self.pos + b"<!DOCTYPE".len();
        let mut in_subset = false;
        while i < n {
            let b = self.buf[i];
            if b == b'"' || b == b'\'' {
                i += 1;
                let off = memchr(b, &self.buf[i..]).ok_or(XmlError::Malformed(i))?;
                i += off + 1;
            } else if self.buf[i..].starts_with(b"<!--") {
                let start = i + 4;
                let off = memmem::find(&self.buf[start..], b"-->").ok_or(XmlError::Malformed(i))?;
                i = start + off + 3;
            } else if !in_subset
                && (self.buf[i..].starts_with(b"SYSTEM") || self.buf[i..].starts_with(b"PUBLIC"))
            {
                return Err(XmlError::UnsupportedDtd); // external DTD
            } else if in_subset && self.buf[i..].starts_with(b"<!ENTITY") {
                i = parse_entity_decl(self.buf, i, entities)?;
            } else if b == b'[' {
                in_subset = true;
                i += 1;
            } else if b == b']' {
                in_subset = false;
                i += 1;
            } else if b == b'>' && !in_subset {
                self.pos = i + 1;
                return Ok(());
            } else {
                i += 1;
            }
        }
        Err(XmlError::Malformed(self.pos))
    }

    // --- Root -------------------------------------------------------------

    /// Parse the root start tag: extract its qualified name and the namespace
    /// declarations applied to every record. Returns whether the root is
    /// self-closing (an empty document with no records).
    fn parse_root(&mut self) -> Result<(Box<str>, NamespaceContext, bool), XmlError> {
        let lt = self.pos;
        let name_start = lt + 1;
        let n = self.buf.len();
        let mut j = name_start;
        while j < n && is_name_char(self.buf[j]) {
            j += 1;
        }
        let name = &self.buf[name_start..j];
        if name.is_empty() {
            return Err(XmlError::Malformed(lt));
        }
        let root_name: Box<str> = utf8(name)?.into();
        let (end, self_closing, namespaces) = self.parse_start_tag_attrs(j)?;
        self.pos = end;
        Ok((root_name, namespaces, self_closing))
    }

    /// Parse attributes from `i` (just after the element name) to the tag's `>`,
    /// capturing `xmlns` / `xmlns:prefix` declarations. Returns the offset just
    /// past `>`, whether the tag is self-closing, and the namespace context.
    fn parse_start_tag_attrs(
        &self,
        mut i: usize,
    ) -> Result<(usize, bool, NamespaceContext), XmlError> {
        let n = self.buf.len();
        let mut ns = NamespaceContext::new();
        loop {
            skip_ws_at(self.buf, &mut i);
            if i >= n {
                return Err(XmlError::Malformed(i));
            }
            match self.buf[i] {
                b'>' => return Ok((i + 1, false, ns)),
                b'/' => {
                    return if self.buf.get(i + 1) == Some(&b'>') {
                        Ok((i + 2, true, ns))
                    } else {
                        Err(XmlError::Malformed(i))
                    };
                }
                _ => {
                    let astart = i;
                    while i < n && is_name_char(self.buf[i]) {
                        i += 1;
                    }
                    let aname = &self.buf[astart..i];
                    if aname.is_empty() {
                        return Err(XmlError::Malformed(i));
                    }
                    skip_ws_at(self.buf, &mut i);
                    if i >= n || self.buf[i] != b'=' {
                        return Err(XmlError::Malformed(i));
                    }
                    i += 1;
                    skip_ws_at(self.buf, &mut i);
                    if i >= n || (self.buf[i] != b'"' && self.buf[i] != b'\'') {
                        return Err(XmlError::Malformed(i));
                    }
                    let q = self.buf[i];
                    i += 1;
                    let off = memchr(q, &self.buf[i..]).ok_or(XmlError::Malformed(i))?;
                    let value = &self.buf[i..i + off];
                    i += off + 1;

                    if aname == b"xmlns" {
                        ns.insert(utf8(b"")?, utf8(value)?);
                    } else if let Some(prefix) = aname.strip_prefix(b"xmlns:") {
                        ns.insert(utf8(prefix)?, utf8(value)?);
                    }
                }
            }
        }
    }

    // --- Content framing --------------------------------------------------

    /// Frame depth-1 records, starting with the cursor just past the root start
    /// tag (`depth == 1`). Returns with the cursor just past the root end tag.
    fn scan_content(
        &mut self,
        records: &mut Vec<Range<usize>>,
        root_name: &[u8],
    ) -> Result<(), XmlError> {
        let mut depth: usize = 1;
        let mut record_start: Option<usize> = None;

        loop {
            let lt = match memchr(b'<', &self.buf[self.pos..]) {
                Some(off) => self.pos + off,
                None => return Err(XmlError::Malformed(self.pos)), // EOF before root close
            };
            // Only whitespace is allowed directly under the root, between records.
            if depth == 1 && !self.buf[self.pos..lt].iter().all(|&b| is_xml_ws(b)) {
                return Err(XmlError::Malformed(self.pos));
            }
            self.pos = lt;
            let rest = &self.buf[lt..];

            if rest.starts_with(b"<!--") {
                self.skip_comment()?;
            } else if rest.starts_with(b"<![CDATA[") {
                self.skip_cdata()?;
            } else if rest.starts_with(b"<?") {
                self.skip_pi()?;
            } else if rest.starts_with(b"</") {
                let end = self.scan_tag_end(lt + 2)?;
                depth = depth.checked_sub(1).ok_or(XmlError::Malformed(lt))?;
                if depth == 0 {
                    // Root end tag. A record left open, or a name that doesn't
                    // match the root start tag, is malformed.
                    if record_start.is_some() || end_tag_name(self.buf, lt + 2) != root_name {
                        return Err(XmlError::Malformed(lt));
                    }
                    self.pos = end;
                    return Ok(());
                } else if depth == 1 {
                    let start = record_start.take().ok_or(XmlError::Malformed(lt))?;
                    records.push(start..end);
                }
                self.pos = end;
            } else if rest.len() >= 2 && is_name_start(rest[1]) {
                let (end, self_closing) = self.scan_start_tag(lt + 1)?;
                if depth == 1 {
                    if self_closing {
                        records.push(lt..end); // complete one-tag record
                    } else {
                        record_start = Some(lt);
                        depth = 2;
                    }
                } else if !self_closing {
                    depth += 1;
                }
                self.pos = end;
            } else {
                return Err(XmlError::Malformed(lt));
            }
        }
    }

    /// Allow only whitespace / comments / PIs between the root end tag and EOF.
    fn skip_trailing_misc(&mut self) -> Result<(), XmlError> {
        loop {
            self.skip_ws();
            let rest = &self.buf[self.pos..];
            if rest.is_empty() {
                return Ok(());
            }
            if rest.starts_with(b"<!--") {
                self.skip_comment()?;
            } else if rest.starts_with(b"<?") {
                self.skip_pi()?;
            } else {
                return Err(XmlError::Malformed(self.pos));
            }
        }
    }

    // --- Low-level span skippers (cursor at the opening delimiter) ---------

    fn skip_comment(&mut self) -> Result<(), XmlError> {
        let start = self.pos + 4; // past "<!--"
        let off = memmem::find(&self.buf[start..], b"-->").ok_or(XmlError::Malformed(self.pos))?;
        self.pos = start + off + 3;
        Ok(())
    }

    fn skip_cdata(&mut self) -> Result<(), XmlError> {
        let start = self.pos + 9; // past "<![CDATA["
        let off = memmem::find(&self.buf[start..], b"]]>").ok_or(XmlError::Malformed(self.pos))?;
        self.pos = start + off + 3;
        Ok(())
    }

    fn skip_pi(&mut self) -> Result<(), XmlError> {
        let start = self.pos + 2; // past "<?"
        let off = memmem::find(&self.buf[start..], b"?>").ok_or(XmlError::Malformed(self.pos))?;
        self.pos = start + off + 2;
        Ok(())
    }

    /// Scan a tag body from `i` (just past `<`) to its closing `>`, respecting
    /// quoted attribute values (a `>` inside a value is not the tag end).
    /// Returns the offset just past `>` and whether the tag is self-closing.
    fn scan_start_tag(&self, mut i: usize) -> Result<(usize, bool), XmlError> {
        loop {
            let off = memchr3(b'>', b'"', b'\'', &self.buf[i..]).ok_or(XmlError::Malformed(i))?;
            match self.buf[i + off] {
                b'>' => {
                    let gt = i + off;
                    let self_closing = off > 0 && self.buf[gt - 1] == b'/';
                    return Ok((gt + 1, self_closing));
                }
                q => {
                    let qstart = i + off + 1;
                    let qoff = memchr(q, &self.buf[qstart..]).ok_or(XmlError::Malformed(qstart))?;
                    i = qstart + qoff + 1;
                }
            }
        }
    }

    fn scan_tag_end(&self, i: usize) -> Result<usize, XmlError> {
        Ok(self.scan_start_tag(i)?.0)
    }

    fn skip_ws(&mut self) {
        while self.pos < self.buf.len() && is_xml_ws(self.buf[self.pos]) {
            self.pos += 1;
        }
    }
}

// --- Free helpers ---------------------------------------------------------

fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b':' || b >= 0x80
}

fn is_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-' | b'.') || b >= 0x80
}

/// The name of an end tag, given the offset just past `</`.
fn end_tag_name(buf: &[u8], start: usize) -> &[u8] {
    let mut j = start;
    while j < buf.len() && is_name_char(buf[j]) {
        j += 1;
    }
    &buf[start..j]
}

fn is_xml_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

fn skip_ws_at(buf: &[u8], i: &mut usize) {
    while *i < buf.len() && is_xml_ws(buf[*i]) {
        *i += 1;
    }
}

/// Parse internal-subset `<!ENTITY>` declarations out of a DOCTYPE body (the
/// bytes between `<!DOCTYPE` and its closing `>`, e.g. as surfaced by
/// `quick_xml`'s `DocType` event). Best-effort: malformed declarations are
/// skipped. Used by the sequential reader, which has no Phase A [`Prelude`].
pub(crate) fn parse_doctype_entities(body: &[u8], out: &mut HashMap<Box<str>, Box<str>>) {
    let mut i = 0;
    while let Some(off) = memmem::find(&body[i..], b"<!ENTITY") {
        let start = i + off;
        i = match parse_entity_decl(body, start, out) {
            Ok(next) => next,
            Err(_) => start + b"<!ENTITY".len(), // skip the token, keep going
        };
    }
}

/// Parse one `<!ENTITY …>` declaration starting at `i`, capturing general
/// internal entities (`<!ENTITY name "value">`). Parameter entities
/// (`<!ENTITY % …>`) and external entities (`SYSTEM`/`PUBLIC`) are unsupported
/// and rejected with [`XmlError::UnsupportedDtd`] rather than silently skipped.
/// Returns the offset just past the declaration's `>`.
fn parse_entity_decl(
    buf: &[u8],
    i: usize,
    entities: &mut HashMap<Box<str>, Box<str>>,
) -> Result<usize, XmlError> {
    let n = buf.len();
    let mut j = i + b"<!ENTITY".len();
    skip_ws_at(buf, &mut j);

    // Parameter entity (`<!ENTITY % …>`) — unsupported.
    if j < n && buf[j] == b'%' {
        return Err(XmlError::UnsupportedDtd);
    }

    let name_start = j;
    while j < n && is_name_char(buf[j]) {
        j += 1;
    }
    let name = &buf[name_start..j];
    if name.is_empty() {
        return Err(XmlError::Malformed(i));
    }
    skip_ws_at(buf, &mut j);

    // Internal entity: a quoted replacement value. Anything else (`SYSTEM` /
    // `PUBLIC`) is an external entity, which we don't resolve.
    if j < n && (buf[j] == b'"' || buf[j] == b'\'') {
        let q = buf[j];
        j += 1;
        let off = memchr(q, &buf[j..]).ok_or(XmlError::Malformed(j))?;
        let value = &buf[j..j + off];
        j += off + 1;
        entities.insert(utf8(name)?.into(), utf8(value)?.into());
        skip_decl_to_gt(buf, j)
    } else {
        Err(XmlError::UnsupportedDtd)
    }
}

/// Find the `>` that closes a markup declaration starting/continuing at `k`,
/// respecting quoted strings. Returns the offset just past `>`.
fn skip_decl_to_gt(buf: &[u8], mut k: usize) -> Result<usize, XmlError> {
    let n = buf.len();
    while k < n {
        match buf[k] {
            b'"' | b'\'' => {
                let q = buf[k];
                k += 1;
                let off = memchr(q, &buf[k..]).ok_or(XmlError::Malformed(k))?;
                k += off + 1;
            }
            b'>' => return Ok(k + 1),
            _ => k += 1,
        }
    }
    Err(XmlError::Malformed(k))
}

fn utf8(bytes: &[u8]) -> Result<&str, XmlError> {
    std::str::from_utf8(bytes).map_err(|_| XmlError::Encoding)
}

fn pseudo_attr<'b>(decl: &'b [u8], name: &[u8]) -> Option<&'b [u8]> {
    let off = memmem::find(decl, name)?;
    let mut i = off + name.len();
    skip_ws_at(decl, &mut i);
    if i >= decl.len() || decl[i] != b'=' {
        return None;
    }
    i += 1;
    skip_ws_at(decl, &mut i);
    let q = *decl.get(i)?;
    if q != b'"' && q != b'\'' {
        return None;
    }
    i += 1;
    let vstart = i;
    let off = memchr(q, &decl[i..])?;
    Some(&decl[vstart..vstart + off])
}

// --- Streaming (incremental) framer ---------------------------------------
//
// A resumable variant of Phase A for the streaming pipeline: bytes are fed in
// chunks, the prolog is parsed once it is fully present, and depth-1 records are
// emitted as *owned* byte buffers as their boundaries are crossed. The consumed
// prefix is compacted away so resident memory stays bounded by the largest
// in-flight record plus a chunk — independent of document size.

/// Result of a successful streaming prolog parse.
pub(crate) struct PreludeParse {
    pub prelude: Prelude,
    pub content_start: usize,
    pub self_closing: bool,
}

/// NeedMore-aware prolog + root-tag parse. `Ok(None)` means "feed more bytes";
/// it is re-run from scratch on the growing buffer until it succeeds.
pub(crate) fn try_parse_prelude(buf: &[u8]) -> Result<Option<PreludeParse>, XmlError> {
    const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
    let mut i = 0usize;

    if buf.starts_with(&UTF8_BOM) {
        i = 3;
    } else if buf.starts_with(&[0xFF, 0xFE]) || buf.starts_with(&[0xFE, 0xFF]) {
        return Err(XmlError::Encoding);
    } else if !buf.is_empty() && UTF8_BOM.starts_with(buf) {
        return Ok(None); // partial BOM
    }

    let mut entities: HashMap<Box<str>, Box<str>> = HashMap::new();

    // Optional XML declaration, immediately after any BOM.
    if buf.len() < i + 2 {
        return Ok(None);
    }
    if buf[i..].starts_with(b"<?xml") {
        match buf.get(i + 5).copied() {
            None => return Ok(None),
            Some(c) if is_xml_ws(c) || c == b'?' => match memmem::find(&buf[i + 2..], b"?>") {
                Some(off) => {
                    let decl = &buf[i + 5..i + 2 + off];
                    if let Some(enc) = pseudo_attr(decl, b"encoding")
                        && !enc.eq_ignore_ascii_case(b"utf-8")
                        && !enc.eq_ignore_ascii_case(b"us-ascii")
                    {
                        return Err(XmlError::Encoding);
                    }
                    i += 2 + off + 2;
                }
                None => return Ok(None),
            },
            Some(_) => {} // e.g. "<?xml-stylesheet" — a PI, handled below
        }
    } else if b"<?xml".starts_with(&buf[i..]) {
        return Ok(None); // could still become the declaration
    }

    loop {
        skip_ws_at(buf, &mut i);
        if i >= buf.len() {
            return Ok(None);
        }
        match classify_prolog(&buf[i..])? {
            None => return Ok(None),
            Some(Construct::Comment) => match memmem::find(&buf[i + 4..], b"-->") {
                Some(off) => i += 4 + off + 3,
                None => return Ok(None),
            },
            Some(Construct::Pi) => match memmem::find(&buf[i + 2..], b"?>") {
                Some(off) => i += 2 + off + 2,
                None => return Ok(None),
            },
            Some(Construct::Doctype) => match find_doctype_end(buf, i)? {
                Some(end) => {
                    parse_doctype_entities(&buf[i..end], &mut entities);
                    i = end;
                }
                None => return Ok(None),
            },
            Some(Construct::Root) => return try_parse_root_tag(buf, i, entities),
        }
    }
}

enum Construct {
    Comment,
    Pi,
    Doctype,
    Root,
}

/// Classify the markup at a prolog `<`. `Ok(None)` => need more bytes to decide.
fn classify_prolog(rest: &[u8]) -> Result<Option<Construct>, XmlError> {
    debug_assert_eq!(rest[0], b'<');
    if rest.len() < 2 {
        return Ok(None);
    }
    match rest[1] {
        b'?' => Ok(Some(Construct::Pi)),
        b'!' => {
            if rest.len() < 4 {
                return Ok(None);
            }
            if rest.starts_with(b"<!--") {
                Ok(Some(Construct::Comment))
            } else if rest.len() < 9 {
                if b"<!DOCTYPE".starts_with(rest) {
                    Ok(None)
                } else {
                    Err(XmlError::Malformed(0))
                }
            } else if rest.starts_with(b"<!DOCTYPE") {
                Ok(Some(Construct::Doctype))
            } else {
                Err(XmlError::Malformed(0))
            }
        }
        c if is_name_start(c) => Ok(Some(Construct::Root)),
        _ => Err(XmlError::Malformed(0)),
    }
}

/// NeedMore-aware search for a DOCTYPE's closing `>` (tracking quotes, the
/// internal subset, and comments). Returns the offset just past `>`.
fn find_doctype_end(buf: &[u8], start: usize) -> Result<Option<usize>, XmlError> {
    let n = buf.len();
    let mut i = start + b"<!DOCTYPE".len();
    let mut in_subset = false;
    while i < n {
        let b = buf[i];
        if b == b'"' || b == b'\'' {
            i += 1;
            match memchr(b, &buf[i..]) {
                Some(off) => i += off + 1,
                None => return Ok(None),
            }
        } else if buf[i..].starts_with(b"<!--") {
            let s = i + 4;
            match memmem::find(&buf[s..], b"-->") {
                Some(off) => i = s + off + 3,
                None => return Ok(None),
            }
        } else if b == b'<' && buf.len() < i + 4 {
            return Ok(None); // might be a comment we can't classify yet
        } else if b == b'[' {
            in_subset = true;
            i += 1;
        } else if b == b']' {
            in_subset = false;
            i += 1;
        } else if b == b'>' && !in_subset {
            return Ok(Some(i + 1));
        } else {
            i += 1;
        }
    }
    Ok(None)
}

/// NeedMore-aware root start-tag parse: name + xmlns declarations + the `>`.
fn try_parse_root_tag(
    buf: &[u8],
    lt: usize,
    entities: HashMap<Box<str>, Box<str>>,
) -> Result<Option<PreludeParse>, XmlError> {
    let n = buf.len();
    let mut j = lt + 1;
    while j < n && is_name_char(buf[j]) {
        j += 1;
    }
    if j >= n {
        return Ok(None); // name may continue
    }
    let name = &buf[lt + 1..j];
    if name.is_empty() {
        return Err(XmlError::Malformed(lt));
    }
    let root_name: Box<str> = utf8(name)?.into();

    let mut ns = NamespaceContext::new();
    let mut i = j;
    loop {
        skip_ws_at(buf, &mut i);
        if i >= n {
            return Ok(None);
        }
        match buf[i] {
            b'>' => {
                return Ok(Some(make_prelude(root_name, ns, entities, i + 1, false)));
            }
            b'/' => {
                return match buf.get(i + 1) {
                    None => Ok(None),
                    Some(b'>') => Ok(Some(make_prelude(root_name, ns, entities, i + 2, true))),
                    Some(_) => Err(XmlError::Malformed(i)),
                };
            }
            _ => {
                let astart = i;
                while i < n && is_name_char(buf[i]) {
                    i += 1;
                }
                if i >= n {
                    return Ok(None);
                }
                let aname = &buf[astart..i];
                if aname.is_empty() {
                    return Err(XmlError::Malformed(i));
                }
                skip_ws_at(buf, &mut i);
                if i >= n {
                    return Ok(None);
                }
                if buf[i] != b'=' {
                    return Err(XmlError::Malformed(i));
                }
                i += 1;
                skip_ws_at(buf, &mut i);
                if i >= n {
                    return Ok(None);
                }
                let q = buf[i];
                if q != b'"' && q != b'\'' {
                    return Err(XmlError::Malformed(i));
                }
                i += 1;
                let vstart = i;
                let off = match memchr(q, &buf[i..]) {
                    Some(off) => off,
                    None => return Ok(None),
                };
                let value = &buf[vstart..vstart + off];
                i = vstart + off + 1;
                if aname == b"xmlns" {
                    ns.insert(utf8(b"")?, utf8(value)?);
                } else if let Some(prefix) = aname.strip_prefix(b"xmlns:") {
                    ns.insert(utf8(prefix)?, utf8(value)?);
                }
            }
        }
    }
}

fn make_prelude(
    root_name: Box<str>,
    namespaces: NamespaceContext,
    entities: HashMap<Box<str>, Box<str>>,
    content_start: usize,
    self_closing: bool,
) -> PreludeParse {
    PreludeParse {
        prelude: Prelude {
            encoding: Encoding::Utf8,
            root_name,
            namespaces,
            entities,
        },
        content_start,
        self_closing,
    }
}

/// Lexical state of the resumable content framer (default byte-by-byte variant).
#[cfg(not(feature = "memchr-framer"))]
#[derive(Clone, Copy)]
enum Cs {
    Text,
    Lt,
    Bang,
    BangDash,
    Comment,
    CommentDash,
    CommentDashDash,
    CdataMatch(u8),
    Cdata,
    CdataBracket,
    CdataBracket2,
    Pi,
    PiQ,
    /// Inside a start/end tag. `quote` is the open quote byte (0 = none);
    /// `prev_slash` tracks a `/` immediately before a possible `>`.
    Tag { is_end: bool, quote: u8, prev_slash: bool },
}

/// Lexical state of the resumable content framer (`memchr-framer` variant).
/// Multi-byte spans (comment, CDATA, PI bodies and tag interiors) are skipped
/// with `memchr`/`memmem`; terminators that straddle a chunk boundary are
/// handled by retaining the last `needle.len() - 1` bytes (see
/// [`StreamFramer::skip_to`]).
#[cfg(feature = "memchr-framer")]
#[derive(Clone, Copy)]
enum Cs {
    Text,
    Lt,
    Bang,
    BangDash,
    Comment,
    CdataMatch(u8),
    Cdata,
    Pi,
    /// Inside a start/end tag. `quote` is the open quote byte (0 = none); a `>`
    /// outside quotes ends the tag.
    Tag { is_end: bool, quote: u8 },
}

/// Resumable depth-1 record framer. Feed bytes with [`StreamFramer::push`],
/// pull records with [`StreamFramer::next_record`], and call
/// [`StreamFramer::compact`] between reads to bound memory.
pub(crate) struct StreamFramer {
    carry: Vec<u8>,
    base: usize,
    cursor: usize,
    state: Cs,
    depth: usize,
    record_start: Option<usize>,
    tag_start: usize,
    next_index: usize,
    finished: bool,
    root_name: Box<str>,
}

impl StreamFramer {
    pub(crate) fn new() -> Self {
        Self {
            carry: Vec::new(),
            base: 0,
            cursor: 0,
            state: Cs::Text,
            depth: 0,
            record_start: None,
            tag_start: 0,
            next_index: 0,
            finished: false,
            root_name: Box::default(),
        }
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.carry.extend_from_slice(chunk);
    }

    /// Attempt to parse the prolog from the buffered bytes. `Ok(None)` => feed
    /// more. On success the framer switches to content mode.
    pub(crate) fn try_prelude(&mut self) -> Result<Option<Arc<Prelude>>, XmlError> {
        match try_parse_prelude(&self.carry)? {
            Some(p) => {
                self.cursor = p.content_start;
                self.depth = 1;
                self.state = Cs::Text;
                if p.self_closing {
                    self.finished = true;
                    self.depth = 0;
                }
                self.root_name = p.prelude.root_name.clone();
                Ok(Some(Arc::new(p.prelude)))
            }
            None => Ok(None),
        }
    }

    /// Drop already-consumed bytes so resident memory stays bounded.
    pub(crate) fn compact(&mut self) {
        let keep_from = match self.record_start {
            // Inside an open record: keep from its start; we will emit those bytes.
            Some(rs) => rs,
            // Between records, in plain text or while skipping an ignored span
            // (comment/CDATA/PI), nothing before `cursor` is needed — for an
            // ignored span that is just the terminator overlap `skip_to`/the
            // state machine left ahead of `cursor`. This keeps a huge root-level
            // comment/CDATA/PI from growing the buffer to its full length.
            None if matches!(self.state, Cs::Text) || self.in_skip_span() => self.cursor,
            // Mid-classification of `<…` (it may still open a record): keep the
            // `<` so the record's bytes survive.
            None => self.tag_start,
        };
        let drop = keep_from - self.base;
        if drop > 0 {
            self.carry.drain(0..drop);
            self.base = keep_from;
        }
    }

    /// Whether the framer is inside a *confirmed* ignored span (comment / CDATA /
    /// PI), whose already-scanned bytes can be dropped on compaction (the
    /// terminator is found via retained overlap or via the state machine, not by
    /// re-reading the whole span).
    #[cfg(not(feature = "memchr-framer"))]
    fn in_skip_span(&self) -> bool {
        matches!(
            self.state,
            Cs::Comment
                | Cs::CommentDash
                | Cs::CommentDashDash
                | Cs::Cdata
                | Cs::CdataBracket
                | Cs::CdataBracket2
                | Cs::Pi
                | Cs::PiQ
        )
    }

    #[cfg(feature = "memchr-framer")]
    fn in_skip_span(&self) -> bool {
        matches!(self.state, Cs::Comment | Cs::Cdata | Cs::Pi)
    }

    /// Whether the end tag currently at `tag_start` names the root element.
    fn root_close_ok(&self) -> bool {
        end_tag_name(&self.carry, self.tag_start - self.base + 2) == self.root_name.as_bytes()
    }

    /// Validate end-of-stream: the root must have closed.
    pub(crate) fn finish(&self) -> Result<(), XmlError> {
        if self.finished {
            Ok(())
        } else {
            Err(XmlError::Malformed(self.cursor))
        }
    }

    /// Advance the framer over the buffered bytes. On a complete record, its
    /// bytes are appended to `arena` and `(index, span)` is returned (the span
    /// indexes into `arena`); `Ok(None)` means more input is needed. Appending
    /// into a caller-owned arena lets the producer pack many records into one
    /// allocation (see the streaming batcher).
    #[cfg(not(feature = "memchr-framer"))]
    pub(crate) fn next_record_into(
        &mut self,
        arena: &mut Vec<u8>,
    ) -> Result<Option<(usize, Range<usize>)>, XmlError> {
        let mut i = self.cursor - self.base;
        let n = self.carry.len();
        while i < n && !self.finished {
            match self.state {
                Cs::Text => match memchr(b'<', &self.carry[i..]) {
                    Some(off) => {
                        if self.depth == 1
                            && !self.carry[i..i + off].iter().all(|&b| is_xml_ws(b))
                        {
                            return Err(XmlError::Malformed(self.base + i));
                        }
                        self.tag_start = self.base + i + off;
                        i += off + 1;
                        self.state = Cs::Lt;
                    }
                    None => {
                        if self.depth == 1 && !self.carry[i..].iter().all(|&b| is_xml_ws(b)) {
                            return Err(XmlError::Malformed(self.base + i));
                        }
                        i = n;
                    }
                },
                Cs::Lt => {
                    match self.carry[i] {
                        b'?' => self.state = Cs::Pi,
                        b'!' => self.state = Cs::Bang,
                        b'/' => self.state = Cs::Tag { is_end: true, quote: 0, prev_slash: false },
                        c if is_name_start(c) => {
                            self.state = Cs::Tag { is_end: false, quote: 0, prev_slash: false }
                        }
                        _ => return Err(XmlError::Malformed(self.base + i)),
                    }
                    i += 1;
                }
                Cs::Bang => {
                    match self.carry[i] {
                        b'-' => self.state = Cs::BangDash,
                        b'[' => self.state = Cs::CdataMatch(0),
                        _ => return Err(XmlError::Malformed(self.base + i)),
                    }
                    i += 1;
                }
                Cs::BangDash => {
                    match self.carry[i] {
                        b'-' => self.state = Cs::Comment,
                        _ => return Err(XmlError::Malformed(self.base + i)),
                    }
                    i += 1;
                }
                Cs::Comment => {
                    if self.carry[i] == b'-' {
                        self.state = Cs::CommentDash;
                    }
                    i += 1;
                }
                Cs::CommentDash => {
                    self.state = if self.carry[i] == b'-' {
                        Cs::CommentDashDash
                    } else {
                        Cs::Comment
                    };
                    i += 1;
                }
                Cs::CommentDashDash => {
                    match self.carry[i] {
                        b'>' => self.state = Cs::Text,
                        b'-' => {}
                        _ => self.state = Cs::Comment,
                    }
                    i += 1;
                }
                Cs::CdataMatch(k) => {
                    const LIT: &[u8] = b"CDATA[";
                    if self.carry[i] == LIT[k as usize] {
                        self.state = if k as usize + 1 == LIT.len() {
                            Cs::Cdata
                        } else {
                            Cs::CdataMatch(k + 1)
                        };
                        i += 1;
                    } else {
                        return Err(XmlError::Malformed(self.base + i));
                    }
                }
                Cs::Cdata => {
                    if self.carry[i] == b']' {
                        self.state = Cs::CdataBracket;
                    }
                    i += 1;
                }
                Cs::CdataBracket => {
                    self.state = if self.carry[i] == b']' {
                        Cs::CdataBracket2
                    } else {
                        Cs::Cdata
                    };
                    i += 1;
                }
                Cs::CdataBracket2 => {
                    match self.carry[i] {
                        b'>' => self.state = Cs::Text,
                        b']' => {}
                        _ => self.state = Cs::Cdata,
                    }
                    i += 1;
                }
                Cs::Pi => {
                    if self.carry[i] == b'?' {
                        self.state = Cs::PiQ;
                    }
                    i += 1;
                }
                Cs::PiQ => {
                    match self.carry[i] {
                        b'>' => self.state = Cs::Text,
                        b'?' => {}
                        _ => self.state = Cs::Pi,
                    }
                    i += 1;
                }
                Cs::Tag { is_end, quote, prev_slash } => {
                    let b = self.carry[i];
                    if quote != 0 {
                        if b == quote {
                            self.state = Cs::Tag { is_end, quote: 0, prev_slash };
                        }
                        i += 1;
                    } else if b == b'"' || b == b'\'' {
                        self.state = Cs::Tag { is_end, quote: b, prev_slash: false };
                        i += 1;
                    } else if b == b'>' {
                        let end = self.base + i + 1;
                        i += 1;
                        self.state = Cs::Text;
                        if is_end {
                            self.depth =
                                self.depth.checked_sub(1).ok_or(XmlError::Malformed(end))?;
                            if self.depth == 0 {
                                if !self.root_close_ok() {
                                    return Err(XmlError::Malformed(end));
                                }
                                self.finished = true;
                            } else if self.depth == 1 {
                                let start =
                                    self.record_start.take().ok_or(XmlError::Malformed(end))?;
                                self.cursor = end;
                                return Ok(Some(self.emit(arena, start, end)));
                            }
                        } else if prev_slash {
                            if self.depth == 1 {
                                self.cursor = end;
                                return Ok(Some(self.emit(arena, self.tag_start, end)));
                            }
                        } else if self.depth == 1 {
                            self.record_start = Some(self.tag_start);
                            self.depth = 2;
                        } else {
                            self.depth += 1;
                        }
                    } else if b == b'/' {
                        self.state = Cs::Tag { is_end, quote: 0, prev_slash: true };
                        i += 1;
                    } else {
                        self.state = Cs::Tag { is_end, quote: 0, prev_slash: false };
                        i += 1;
                    }
                }
            }
        }
        self.cursor = self.base + i;
        Ok(None)
    }

    /// Advance the framer over the buffered bytes. On a complete record, its
    /// bytes are appended to `arena` and `(index, span)` is returned (the span
    /// indexes into `arena`); `Ok(None)` means more input is needed. Appending
    /// into a caller-owned arena lets the producer pack many records into one
    /// allocation (see the streaming batcher).
    #[cfg(feature = "memchr-framer")]
    pub(crate) fn next_record_into(
        &mut self,
        arena: &mut Vec<u8>,
    ) -> Result<Option<(usize, Range<usize>)>, XmlError> {
        let mut i = self.cursor - self.base;
        let n = self.carry.len();
        while i < n && !self.finished {
            match self.state {
                Cs::Text => match memchr(b'<', &self.carry[i..]) {
                    Some(off) => {
                        if self.depth == 1
                            && !self.carry[i..i + off].iter().all(|&b| is_xml_ws(b))
                        {
                            return Err(XmlError::Malformed(self.base + i));
                        }
                        self.tag_start = self.base + i + off;
                        i += off + 1;
                        self.state = Cs::Lt;
                    }
                    None => {
                        if self.depth == 1 && !self.carry[i..].iter().all(|&b| is_xml_ws(b)) {
                            return Err(XmlError::Malformed(self.base + i));
                        }
                        i = n;
                    }
                },
                Cs::Lt => {
                    match self.carry[i] {
                        b'?' => self.state = Cs::Pi,
                        b'!' => self.state = Cs::Bang,
                        b'/' => self.state = Cs::Tag { is_end: true, quote: 0 },
                        c if is_name_start(c) => {
                            self.state = Cs::Tag { is_end: false, quote: 0 }
                        }
                        _ => return Err(XmlError::Malformed(self.base + i)),
                    }
                    i += 1;
                }
                Cs::Bang => {
                    match self.carry[i] {
                        b'-' => self.state = Cs::BangDash,
                        b'[' => self.state = Cs::CdataMatch(0),
                        _ => return Err(XmlError::Malformed(self.base + i)),
                    }
                    i += 1;
                }
                Cs::BangDash => {
                    match self.carry[i] {
                        b'-' => self.state = Cs::Comment,
                        _ => return Err(XmlError::Malformed(self.base + i)),
                    }
                    i += 1;
                }
                Cs::Comment => match self.skip_to(i, b"-->") {
                    Some(next) => {
                        i = next;
                        self.state = Cs::Text;
                    }
                    None => return Ok(None),
                },
                Cs::CdataMatch(k) => {
                    const LIT: &[u8] = b"CDATA[";
                    if self.carry[i] == LIT[k as usize] {
                        self.state = if k as usize + 1 == LIT.len() {
                            Cs::Cdata
                        } else {
                            Cs::CdataMatch(k + 1)
                        };
                        i += 1;
                    } else {
                        return Err(XmlError::Malformed(self.base + i));
                    }
                }
                Cs::Cdata => match self.skip_to(i, b"]]>") {
                    Some(next) => {
                        i = next;
                        self.state = Cs::Text;
                    }
                    None => return Ok(None),
                },
                Cs::Pi => match self.skip_to(i, b"?>") {
                    Some(next) => {
                        i = next;
                        self.state = Cs::Text;
                    }
                    None => return Ok(None),
                },
                Cs::Tag { is_end, quote } if quote != 0 => {
                    // Skip a quoted attribute value to its closing quote.
                    match memchr(quote, &self.carry[i..]) {
                        Some(off) => {
                            i += off + 1;
                            self.state = Cs::Tag { is_end, quote: 0 };
                        }
                        None => {
                            self.cursor = self.base + n;
                            return Ok(None);
                        }
                    }
                }
                Cs::Tag { is_end, quote: _ } => {
                    // Jump to the next `>` or opening quote.
                    let off = match memchr3(b'>', b'"', b'\'', &self.carry[i..]) {
                        Some(off) => off,
                        None => {
                            self.cursor = self.base + n;
                            return Ok(None);
                        }
                    };
                    let pos = i + off;
                    if self.carry[pos] != b'>' {
                        i = pos + 1;
                        self.state = Cs::Tag { is_end, quote: self.carry[pos] };
                        continue;
                    }
                    // Tag end. A self-closing start tag has `/` just before `>`.
                    let end = self.base + pos + 1;
                    let self_closing = !is_end && pos > 0 && self.carry[pos - 1] == b'/';
                    i = pos + 1;
                    self.state = Cs::Text;
                    if is_end {
                        self.depth = self.depth.checked_sub(1).ok_or(XmlError::Malformed(end))?;
                        if self.depth == 0 {
                            if !self.root_close_ok() {
                                return Err(XmlError::Malformed(end));
                            }
                            self.finished = true;
                        } else if self.depth == 1 {
                            let start = self.record_start.take().ok_or(XmlError::Malformed(end))?;
                            self.cursor = end;
                            return Ok(Some(self.emit(arena, start, end)));
                        }
                    } else if self_closing {
                        if self.depth == 1 {
                            self.cursor = end;
                            return Ok(Some(self.emit(arena, self.tag_start, end)));
                        }
                    } else if self.depth == 1 {
                        self.record_start = Some(self.tag_start);
                        self.depth = 2;
                    } else {
                        self.depth += 1;
                    }
                }
            }
        }
        self.cursor = self.base + i;
        Ok(None)
    }

    fn emit(&mut self, arena: &mut Vec<u8>, start: usize, end: usize) -> (usize, Range<usize>) {
        let from = arena.len();
        arena.extend_from_slice(&self.carry[start - self.base..end - self.base]);
        let index = self.next_index;
        self.next_index += 1;
        (index, from..arena.len())
    }

    /// Find `needle` in `carry[i..]`. On success returns the index just past it.
    /// On failure (need more input), retains the last `needle.len() - 1` bytes —
    /// the terminator may straddle the next chunk — and records the resume
    /// cursor, then returns `None`.
    #[cfg(feature = "memchr-framer")]
    fn skip_to(&mut self, i: usize, needle: &[u8]) -> Option<usize> {
        match memmem::find(&self.carry[i..], needle) {
            Some(off) => Some(i + off + needle.len()),
            None => {
                let keep = needle.len() - 1;
                self.cursor = self.base + self.carry.len().saturating_sub(keep).max(i);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scan `input` and return each framed record as an owned `String`.
    fn frames(input: &str) -> Vec<String> {
        let idx = scan(input.as_bytes()).expect("scan should succeed");
        idx.records()
            .iter()
            .map(|r| String::from_utf8(input.as_bytes()[r.clone()].to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn basic_two_records_exact_ranges() {
        let input = "<trades><trade>a</trade><trade>b</trade></trades>";
        let idx = scan(input.as_bytes()).unwrap();
        let recs: Vec<&str> = idx.records().iter().map(|r| &input[r.clone()]).collect();
        assert_eq!(recs, vec!["<trade>a</trade>", "<trade>b</trade>"]);
        assert_eq!(idx.prelude().root_name.as_ref(), "trades");
    }

    #[test]
    fn whitespace_between_records_is_skipped() {
        assert_eq!(frames("<r>\n  <a/>\n  <b/>\n</r>"), vec!["<a/>", "<b/>"]);
    }

    #[test]
    fn self_closing_and_normal_records_mixed() {
        assert_eq!(
            frames("<r><a/><b>x</b><c/></r>"),
            vec!["<a/>", "<b>x</b>", "<c/>"]
        );
    }

    #[test]
    fn nested_content_is_one_record() {
        assert_eq!(
            frames("<r><a><b/><c>x</c></a></r>"),
            vec!["<a><b/><c>x</c></a>"]
        );
    }

    #[test]
    fn greater_than_inside_attribute_value() {
        assert_eq!(frames(r#"<r><a x="1 > 0"/></r>"#), vec![r#"<a x="1 > 0"/>"#]);
    }

    #[test]
    fn comment_and_cdata_record_lookalikes_are_ignored() {
        assert_eq!(
            frames("<r><!-- <x/> --><a>1</a><![CDATA[</a><b>]]></r>"),
            vec!["<a>1</a>"]
        );
    }

    #[test]
    fn pis_in_prolog_and_content() {
        assert_eq!(
            frames("<?xml version=\"1.0\"?><?pi data?><r><?pi?><a/></r>"),
            vec!["<a/>"]
        );
    }

    #[test]
    fn root_attributes_do_not_become_records() {
        assert_eq!(frames(r#"<r id="root"><a/></r>"#), vec!["<a/>"]);
    }

    #[test]
    fn self_closing_root_has_no_records() {
        let idx = scan(b"<r/>").unwrap();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn root_namespaces_are_captured() {
        let idx = scan(br#"<r xmlns="urn:d" xmlns:p="urn:p"><a/></r>"#).unwrap();
        let p = idx.prelude();
        assert_eq!(p.root_name.as_ref(), "r");
        assert_eq!(p.namespaces.resolve(""), Some("urn:d"));
        assert_eq!(p.namespaces.resolve("p"), Some("urn:p"));
        assert_eq!(p.namespaces.resolve("missing"), None);
    }

    #[test]
    fn xml_declaration_utf8_is_accepted() {
        let idx = scan(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><r><a/></r>").unwrap();
        assert_eq!(idx.prelude().encoding, Encoding::Utf8);
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn non_utf8_declared_encoding_is_rejected() {
        assert!(matches!(
            scan(b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><r/>"),
            Err(XmlError::Encoding)
        ));
    }

    #[test]
    fn internal_entities_captured() {
        let idx = scan(b"<!DOCTYPE r [ <!ENTITY a 'x'> <!ENTITY b \"z\"> ]><r/>").unwrap();
        let e = &idx.prelude().entities;
        assert_eq!(e.get("a").map(|s| &**s), Some("x"));
        assert_eq!(e.get("b").map(|s| &**s), Some("z"));
    }

    #[test]
    fn parameter_entity_is_rejected() {
        assert!(matches!(
            scan(b"<!DOCTYPE r [ <!ENTITY % p 'y'> ]><r/>"),
            Err(XmlError::UnsupportedDtd)
        ));
    }

    #[test]
    fn external_entity_is_rejected() {
        assert!(matches!(
            scan(br#"<!DOCTYPE r [ <!ENTITY ext SYSTEM "ext.ent"> ]><r/>"#),
            Err(XmlError::UnsupportedDtd)
        ));
    }

    #[test]
    fn external_dtd_is_rejected() {
        assert!(matches!(
            scan(br#"<!DOCTYPE r SYSTEM "r.dtd"><r><a/></r>"#),
            Err(XmlError::UnsupportedDtd)
        ));
        assert!(matches!(
            scan(br#"<!DOCTYPE r PUBLIC "-//x//DTD//EN" "r.dtd"><r/>"#),
            Err(XmlError::UnsupportedDtd)
        ));
    }

    #[test]
    fn utf8_bom_is_skipped() {
        let mut input = vec![0xEF, 0xBB, 0xBF];
        input.extend_from_slice(b"<r><a/></r>");
        let idx = scan(&input).unwrap();
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn utf16_bom_is_rejected() {
        assert!(matches!(scan(&[0xFF, 0xFE, b'<']), Err(XmlError::Encoding)));
    }

    #[test]
    fn non_ascii_element_names() {
        assert_eq!(frames("<späm><ítem/></späm>"), vec!["<ítem/>"]);
    }

    #[test]
    fn malformed_inputs_error() {
        assert!(scan(b"").is_err(), "empty");
        assert!(scan(b"   ").is_err(), "no root element");
        assert!(scan(b"<r><a>").is_err(), "unclosed record");
        assert!(scan(b"<r><a></r>").is_err(), "mismatched / root consumed by child");
        assert!(scan(b"<r></r>trailing").is_err(), "junk after root");
        assert!(scan(b"<r/>x").is_err(), "junk after self-closing root");
    }

    #[test]
    fn mismatched_root_close_is_rejected() {
        assert!(scan(b"<r><a/></x>").is_err(), "root close name mismatch");
        assert!(
            scan(b"<trades><trade/></trade>").is_err(),
            "root close matches record name, not root"
        );
        assert!(scan(b"<r><a/></r>").is_ok(), "matching root close is fine");
    }

    #[test]
    fn non_whitespace_text_under_root_is_rejected() {
        assert!(scan(b"<r>junk<a/></r>").is_err(), "text before first record");
        assert!(scan(b"<r><a/>junk<b/></r>").is_err(), "text between records");
        assert!(scan(b"<r><a/>junk</r>").is_err(), "text after last record");
        assert!(scan(b"<r> \n\t <a/> \r\n </r>").is_ok(), "whitespace is allowed");
    }

    /// Record byte-ranges per the materialized scanner, as strings.
    fn materialized(input: &[u8]) -> Vec<String> {
        let idx = scan(input).unwrap();
        idx.records()
            .iter()
            .map(|r| String::from_utf8(input[r.clone()].to_vec()).unwrap())
            .collect()
    }

    /// Drive the streaming framer feeding `chunk` bytes at a time.
    fn stream_frame(input: &[u8], chunk: usize) -> Vec<String> {
        let mut framer = StreamFramer::new();
        let mut fed = 0;
        loop {
            if framer.try_prelude().unwrap().is_some() {
                break;
            }
            assert!(fed < input.len(), "exhausted input before prolog completed");
            let end = (fed + chunk).min(input.len());
            framer.push(&input[fed..end]);
            fed = end;
        }
        let mut out = Vec::new();
        loop {
            let mut arena = Vec::new();
            while let Some((_index, span)) = framer.next_record_into(&mut arena).unwrap() {
                out.push(String::from_utf8(arena[span].to_vec()).unwrap());
            }
            framer.compact();
            if fed >= input.len() {
                framer.finish().unwrap();
                break;
            }
            let end = (fed + chunk).min(input.len());
            framer.push(&input[fed..end]);
            fed = end;
        }
        out
    }

    #[test]
    fn streaming_framer_matches_materialized() {
        let inputs: &[&[u8]] = &[
            b"<trades><trade>a</trade><trade>b</trade></trades>",
            b"<r>\n  <a/>\n  <b>x</b>\n</r>",
            b"<r><a x=\"1 > 0\"/></r>",
            b"<r><!-- <a/> --><a>1</a><![CDATA[</a><b>]]></r>",
            b"<?xml version=\"1.0\"?><?pi data?><r><?pi?><a/></r>",
            b"<!DOCTYPE r [ <!ENTITY foo \"bar\"> ]><r><a>&foo;</a></r>",
            b"<r id=\"root\"><a><b/><c>x</c></a></r>",
            b"<r/>",
            b"<r></r>",
        ];
        for input in inputs {
            let expected = materialized(input);
            for &chunk in &[1usize, 2, 3, 5, 7, 13, 1000] {
                let got = stream_frame(input, chunk);
                assert_eq!(
                    got,
                    expected,
                    "input={:?} chunk={chunk}",
                    std::str::from_utf8(input).unwrap()
                );
            }
        }
    }

    #[test]
    fn streaming_framer_indices_are_sequential() {
        let mut framer = StreamFramer::new();
        framer.push(b"<r><a/><b/><c/></r>");
        assert!(framer.try_prelude().unwrap().is_some());
        let mut arena = Vec::new();
        let mut indices = Vec::new();
        while let Some((index, _span)) = framer.next_record_into(&mut arena).unwrap() {
            indices.push(index);
        }
        assert_eq!(indices, vec![0, 1, 2]);
    }

    /// Drive the streaming framer over a whole input; return the first error
    /// (framing or end-of-stream).
    fn stream_result(input: &[u8]) -> Result<(), XmlError> {
        let mut f = StreamFramer::new();
        f.push(input);
        if f.try_prelude()?.is_none() {
            return Err(XmlError::Malformed(0));
        }
        let mut arena = Vec::new();
        loop {
            match f.next_record_into(&mut arena)? {
                Some(_) => {}
                None => return f.finish(),
            }
        }
    }

    #[test]
    fn streaming_framer_enforces_well_formedness() {
        assert!(stream_result(b"<r><a/></x>").is_err(), "mismatched root close");
        assert!(stream_result(b"<r>junk<a/></r>").is_err(), "text before record");
        assert!(stream_result(b"<r><a/>junk</r>").is_err(), "text after record");
        assert!(stream_result(b"<r> <a/> </r>").is_ok(), "whitespace is allowed");
    }

    #[test]
    fn large_ignored_comment_keeps_carry_bounded() {
        // A big depth-1 comment between the root open and a record. Its bytes are
        // ignored, so the framer must not retain the whole span (see compaction).
        let mut input = String::from("<r><!--");
        input.push_str(&"x".repeat(100_000));
        input.push_str("--><a/></r>");
        let bytes = input.as_bytes();

        let mut framer = StreamFramer::new();
        let mut fed = 0;
        let feed = |framer: &mut StreamFramer, fed: &mut usize| {
            let end = (*fed + 64).min(bytes.len());
            framer.push(&bytes[*fed..end]);
            *fed = end;
        };
        while framer.try_prelude().unwrap().is_none() {
            feed(&mut framer, &mut fed);
        }

        let mut arena = Vec::new();
        let mut max_carry = 0;
        let mut records = 0;
        loop {
            while framer.next_record_into(&mut arena).unwrap().is_some() {
                records += 1;
            }
            framer.compact();
            max_carry = max_carry.max(framer.carry.len());
            if fed >= bytes.len() {
                framer.finish().unwrap();
                break;
            }
            feed(&mut framer, &mut fed);
        }

        assert_eq!(records, 1, "the single <a/> record");
        assert!(
            max_carry < 1024,
            "carry grew to {max_carry} bytes; the 100 KB comment was retained"
        );
    }
}
