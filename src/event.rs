//! StAX-style events surfaced to record consumers.

use std::borrow::Cow;

use quick_xml::name::QName;

/// A single pull-parser event, borrowed from the underlying record slice where
/// possible (zero-copy). Comments and PIs are optional and filterable via
/// [`Config`](crate::Config).
#[derive(Debug)]
pub enum Event<'a> {
    /// An element start tag and its (lazily-iterated) attributes.
    Start { name: QName<'a>, attrs: Attrs<'a> },
    /// An element end tag.
    End { name: QName<'a> },
    /// Character data — entity-decoded; owned only when decoding was required.
    Text(Cow<'a, str>),
    /// A `<![CDATA[ … ]]>` section, surfaced raw.
    Cdata(&'a [u8]),
}

/// Lazily-iterated attributes of a start tag.
///
/// Scaffold placeholder: in Phase B this wraps `quick_xml`'s attribute iterator
/// over the start tag's bytes.
#[derive(Debug, Clone)]
pub struct Attrs<'a> {
    raw: &'a [u8],
}

impl<'a> Attrs<'a> {
    /// The raw, undecoded attribute span of the start tag.
    pub fn as_bytes(&self) -> &'a [u8] {
        self.raw
    }
}
