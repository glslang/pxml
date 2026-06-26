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
        let prelude = Arc::new(Prelude {
            encoding,
            root_name,
            namespaces,
            entities,
        });

        let mut records = Vec::new();
        if !self_closing {
            self.scan_content(&mut records)?;
        }
        self.skip_trailing_misc()?;

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

    /// Parse a DOCTYPE, capturing internal-subset `<!ENTITY>` definitions.
    /// External DTDs / parameter entities are skipped (out of scope for v1).
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
            } else if in_subset && self.buf[i..].starts_with(b"<!ENTITY") {
                i = self.parse_entity_decl(i, entities)?;
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

    /// Parse one `<!ENTITY …>` declaration starting at `i`; capture general
    /// internal entities (`<!ENTITY name "value">`) and skip parameter/external
    /// ones. Returns the offset just past the declaration's `>`.
    fn parse_entity_decl(
        &self,
        i: usize,
        entities: &mut HashMap<Box<str>, Box<str>>,
    ) -> Result<usize, XmlError> {
        let n = self.buf.len();
        let mut j = i + b"<!ENTITY".len();
        skip_ws_at(self.buf, &mut j);

        // Parameter entity (`<!ENTITY % …>`) — out of scope.
        if j < n && self.buf[j] == b'%' {
            return skip_decl_to_gt(self.buf, i);
        }

        let name_start = j;
        while j < n && is_name_char(self.buf[j]) {
            j += 1;
        }
        let name = &self.buf[name_start..j];
        if name.is_empty() {
            return Err(XmlError::Malformed(i));
        }
        skip_ws_at(self.buf, &mut j);

        // Internal entity: a quoted replacement value. Anything else (SYSTEM /
        // PUBLIC) is external — skip without capturing.
        if j < n && (self.buf[j] == b'"' || self.buf[j] == b'\'') {
            let q = self.buf[j];
            j += 1;
            let off = memchr(q, &self.buf[j..]).ok_or(XmlError::Malformed(j))?;
            let value = &self.buf[j..j + off];
            j += off + 1;
            let name = utf8(name)?;
            let value = utf8(value)?;
            entities.insert(name.into(), value.into());
            skip_decl_to_gt(self.buf, j)
        } else {
            skip_decl_to_gt(self.buf, i)
        }
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
    fn scan_content(&mut self, records: &mut Vec<Range<usize>>) -> Result<(), XmlError> {
        let mut depth: usize = 1;
        let mut record_start: Option<usize> = None;

        loop {
            let lt = match memchr(b'<', &self.buf[self.pos..]) {
                Some(off) => self.pos + off,
                None => return Err(XmlError::Malformed(self.pos)), // EOF before root close
            };
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
                    // Root end tag. A record left open here is malformed.
                    if record_start.is_some() {
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

fn is_xml_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

fn skip_ws_at(buf: &[u8], i: &mut usize) {
    while *i < buf.len() && is_xml_ws(buf[*i]) {
        *i += 1;
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
    fn internal_entities_captured_params_and_external_skipped() {
        let idx = scan(
            b"<!DOCTYPE r [ <!ENTITY a 'x'> <!ENTITY % p 'y'> <!ENTITY b \"z\"> ]><r/>",
        )
        .unwrap();
        let e = &idx.prelude().entities;
        assert_eq!(e.get("a").map(|s| &**s), Some("x"));
        assert_eq!(e.get("b").map(|s| &**s), Some("z"));
        assert!(e.get("p").is_none(), "parameter entity must be skipped");
    }

    #[test]
    fn doctype_without_subset_is_skipped() {
        let idx = scan(br#"<!DOCTYPE r SYSTEM "r.dtd"><r><a/></r>"#).unwrap();
        assert_eq!(idx.len(), 1);
        assert!(idx.prelude().entities.is_empty());
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
}
