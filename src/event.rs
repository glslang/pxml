//! StAX-style events surfaced to record consumers.

use std::borrow::Cow;
use std::fmt;

use quick_xml::name::QName;

use crate::XmlError;
use crate::prelude::Prelude;

/// A single pull-parser event, borrowed from the underlying record slice where
/// possible (zero-copy). Comments and PIs are skipped by Phase B.
///
/// The lifetime is tied to the [`RecordReader`](crate::RecordReader) that
/// produced the event: process (or copy out of) each event before requesting
/// the next one.
#[derive(Debug)]
pub enum Event<'a> {
    /// An element start tag and its attributes. Self-closing elements are
    /// surfaced as a `Start` immediately followed by an `End`.
    Start { name: QName<'a>, attrs: Attrs<'a> },
    /// An element end tag.
    End { name: QName<'a> },
    /// Character data, entity-decoded (predefined + prelude entities).
    Text(Cow<'a, str>),
    /// A `<![CDATA[ … ]]>` section, surfaced raw (never entity-decoded).
    Cdata(&'a [u8]),
}

/// The attributes of a start tag. Iterate with [`Attrs::iter`] (or `for`); each
/// item is an [`Attribute`] with the raw key and an entity-decoded value.
#[derive(Clone, Copy)]
pub struct Attrs<'a> {
    raw: &'a [u8],
    prelude: &'a Prelude,
    index: usize,
}

impl<'a> Attrs<'a> {
    pub(crate) fn new(raw: &'a [u8], prelude: &'a Prelude, index: usize) -> Self {
        Self {
            raw,
            prelude,
            index,
        }
    }

    /// The raw, undecoded attribute span of the start tag (everything after the
    /// element name, including leading whitespace and any trailing `/`).
    pub fn as_bytes(&self) -> &'a [u8] {
        self.raw
    }

    /// Iterate the attributes as `key="value"` pairs, decoding entity references
    /// in each value.
    pub fn iter(&self) -> AttrIter<'a> {
        AttrIter {
            buf: self.raw,
            pos: 0,
            prelude: self.prelude,
            index: self.index,
        }
    }
}

impl fmt::Debug for Attrs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attrs")
            .field("raw", &String::from_utf8_lossy(self.raw))
            .finish()
    }
}

impl<'a> IntoIterator for Attrs<'a> {
    type Item = Result<Attribute<'a>, XmlError>;
    type IntoIter = AttrIter<'a>;
    fn into_iter(self) -> AttrIter<'a> {
        self.iter()
    }
}

/// One decoded attribute of a start tag.
#[derive(Debug, Clone)]
pub struct Attribute<'a> {
    /// The raw, undecoded attribute name (may include a `prefix:`).
    pub key: &'a [u8],
    /// The attribute value with entity references decoded.
    pub value: Cow<'a, str>,
}

/// Iterator over a start tag's [`Attribute`]s. See [`Attrs::iter`].
pub struct AttrIter<'a> {
    buf: &'a [u8],
    pos: usize,
    prelude: &'a Prelude,
    index: usize,
}

impl<'a> Iterator for AttrIter<'a> {
    type Item = Result<Attribute<'a>, XmlError>;

    fn next(&mut self) -> Option<Self::Item> {
        let buf = self.buf;
        let n = buf.len();

        skip_ws(buf, &mut self.pos);
        // End of attributes: nothing left, or the self-closing `/`.
        if self.pos >= n || buf[self.pos] == b'/' {
            return None;
        }

        let key_start = self.pos;
        while self.pos < n && !is_ws(buf[self.pos]) && buf[self.pos] != b'=' {
            self.pos += 1;
        }
        let key = &buf[key_start..self.pos];
        if key.is_empty() {
            return Some(Err(XmlError::Malformed(key_start)));
        }

        skip_ws(buf, &mut self.pos);
        if self.pos >= n || buf[self.pos] != b'=' {
            return Some(Err(XmlError::Malformed(self.pos)));
        }
        self.pos += 1;
        skip_ws(buf, &mut self.pos);

        if self.pos >= n || (buf[self.pos] != b'"' && buf[self.pos] != b'\'') {
            return Some(Err(XmlError::Malformed(self.pos)));
        }
        let quote = buf[self.pos];
        self.pos += 1;
        let val_start = self.pos;
        while self.pos < n && buf[self.pos] != quote {
            self.pos += 1;
        }
        if self.pos >= n {
            return Some(Err(XmlError::Malformed(val_start)));
        }
        let raw_value = &buf[val_start..self.pos];
        self.pos += 1; // consume the closing quote

        let value_str = match std::str::from_utf8(raw_value) {
            Ok(s) => s,
            Err(_) => return Some(Err(XmlError::Encoding)),
        };
        let prelude = self.prelude;
        let value = match quick_xml::escape::unescape_with(value_str, |e| prelude.resolve_entity(e))
        {
            Ok(v) => v,
            Err(e) => {
                return Some(Err(XmlError::RecordError {
                    index: self.index,
                    source: Box::new(quick_xml::Error::from(e)),
                }));
            }
        };

        Some(Ok(Attribute { key, value }))
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

fn skip_ws(buf: &[u8], pos: &mut usize) {
    while *pos < buf.len() && is_ws(buf[*pos]) {
        *pos += 1;
    }
}
