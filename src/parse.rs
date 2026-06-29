//! Phase B — per-record pull parser over a single record's byte slice.
//!
//! [`RecordReader`] wraps a borrowed `quick_xml::Reader` over the record's
//! `&[u8]`, mapping `quick_xml`'s events to our [`Event`]. Text and attribute
//! entity references are resolved against the shared [`Prelude`] (predefined
//! entities plus the document's internal-subset `<!ENTITY>` definitions), so a
//! record parses correctly even though it is read in isolation.
//!
//! `quick_xml` borrows element names from the event rather than the input, so
//! events are tied to the reader: process (or copy out of) each event before
//! requesting the next.
//!
//! Namespace prefixes are surfaced lexically (as written). The root/prolog
//! [`NamespaceContext`](crate::NamespaceContext) is available on the prelude for
//! callers that need to resolve them.

use std::borrow::Cow;
use std::sync::Arc;

use quick_xml::Reader;
use quick_xml::escape::EscapeError;
use quick_xml::events::{BytesText, Event as QxEvent};

use crate::XmlError;
use crate::event::{Attrs, Event};
use crate::prelude::Prelude;

/// A StAX-style pull cursor over one record's events.
pub struct RecordReader<'doc> {
    reader: Reader<&'doc [u8]>,
    prelude: Arc<Prelude>,
    current: Option<QxEvent<'doc>>,
    /// One-slot lookahead: an event already read from the reader but not yet
    /// surfaced (the structural event that terminated a coalesced text run).
    pending: Option<QxEvent<'doc>>,
    index: usize,
}

impl<'doc> RecordReader<'doc> {
    /// Build a reader over a single record's slice with shared prolog context.
    /// `index` is the record's position in document order, used to tag errors.
    pub(crate) fn new(bytes: &'doc [u8], prelude: Arc<Prelude>, index: usize) -> Self {
        let mut reader = Reader::from_reader(bytes);
        // Surface `<a/>` as a Start/End pair so consumers see matched tags.
        reader.config_mut().expand_empty_elements = true;
        Self {
            reader,
            prelude,
            current: None,
            pending: None,
            index,
        }
    }

    /// This record's position in document order.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Advance to the next event, or `Ok(None)` at the end of the record.
    /// Comments, PIs, and the XML/DOCTYPE declarations are skipped. Text content
    /// is coalesced: quick-xml surfaces entity references in text as separate
    /// `GeneralRef` events, so a maximal run of `Text`/`GeneralRef` events is
    /// merged back into one resolved [`Event::Text`].
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
            // Fast path: a lone literal text node decodes straight from the
            // document buffer (zero-copy for UTF-8), no allocation.
            let lone_literal =
                matches!(ev, QxEvent::Text(_)) && !next.as_ref().is_some_and(is_text_run);
            if lone_literal {
                let QxEvent::Text(e) = &ev else {
                    unreachable!("checked Text above")
                };
                let text = decode_text(e, self.index)?;
                self.pending = next;
                return Ok(Some(Event::Text(text)));
            }
            // Otherwise coalesce the run into one owned, fully-resolved string.
            let mut out = String::new();
            append_run_event(&mut out, &ev, self.prelude.as_ref(), self.index)?;
            let mut cur = next;
            while let Some(ev) = cur {
                if !is_text_run(&ev) {
                    self.pending = Some(ev);
                    break;
                }
                append_run_event(&mut out, &ev, self.prelude.as_ref(), self.index)?;
                cur = self.read_raw()?;
            }
            return Ok(Some(Event::Text(Cow::Owned(out))));
        }

        self.current = Some(ev);
        let event = map_event(
            self.current.as_ref().expect("event stored above"),
            self.prelude.as_ref(),
            self.index,
        )?;
        Ok(Some(event))
    }

    /// Read the next *surfaced* event, draining the lookahead buffer first and
    /// skipping comments, PIs, and the XML/DOCTYPE declarations. Used only to
    /// start an event; the text-run lookahead uses [`read_raw`](Self::read_raw)
    /// so skipped markup still bounds a text node. `Ok(None)` at end of input.
    fn next_surfaced(&mut self) -> Result<Option<QxEvent<'doc>>, XmlError> {
        loop {
            match self.read_raw()? {
                None => return Ok(None),
                Some(
                    QxEvent::Comment(_) | QxEvent::PI(_) | QxEvent::Decl(_) | QxEvent::DocType(_),
                ) => continue,
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
        match self
            .reader
            .read_event()
            .map_err(|e| record_error(self.index, e))?
        {
            QxEvent::Eof => Ok(None),
            ev => Ok(Some(ev)),
        }
    }
}

/// Map a structural `quick_xml` event to a [`crate::Event`], resolving attribute
/// entities against `prelude`. Shared by [`RecordReader`] and the crate's
/// sequential reader. `index` tags any decode error.
///
/// Only `Start`/`Empty`/`End`/`CData` are valid here. Text content — `Text` and
/// the `GeneralRef` entity references now interleaved with it by quick-xml — is
/// coalesced by the callers' cursors, and comments/PIs/declarations are skipped,
/// before this is reached.
pub(crate) fn map_event<'e>(
    ev: &'e QxEvent<'e>,
    prelude: &'e Prelude,
    index: usize,
) -> Result<Event<'e>, XmlError> {
    Ok(match ev {
        QxEvent::Start(e) | QxEvent::Empty(e) => Event::Start {
            name: e.name(),
            attrs: Attrs::new(e.attributes_raw(), prelude, index),
        },
        QxEvent::End(e) => Event::End { name: e.name() },
        QxEvent::CData(e) => {
            let bytes: &[u8] = e;
            Event::Cdata(bytes)
        }
        _ => unreachable!("non-surfaced event passed to map_event"),
    })
}

/// True for events that make up a text run — literal text or an entity
/// reference. quick-xml 0.40 surfaces `&…;` references in text content as
/// standalone [`GeneralRef`](QxEvent::GeneralRef) events, so a run of these must
/// be coalesced back into a single [`Event::Text`].
pub(crate) fn is_text_run(ev: &QxEvent) -> bool {
    matches!(ev, QxEvent::Text(_) | QxEvent::GeneralRef(_))
}

/// Decode a literal `Text` event to UTF-8. Because entity references are now
/// separate `GeneralRef` events, a `Text` event carries no `&…;` to expand —
/// decoding suffices, and stays zero-copy (borrowed) for UTF-8 input.
pub(crate) fn decode_text<'e>(e: &BytesText<'e>, index: usize) -> Result<Cow<'e, str>, XmlError> {
    e.decode()
        .map_err(|err| record_error(index, quick_xml::Error::from(err)))
}

/// Append one text-run event's resolved content to `out`: literal text is
/// decoded; a `GeneralRef` is resolved as a character reference, or else a named
/// entity from `prelude` (an unknown name is rejected, never silently dropped).
pub(crate) fn append_run_event(
    out: &mut String,
    ev: &QxEvent,
    prelude: &Prelude,
    index: usize,
) -> Result<(), XmlError> {
    match ev {
        QxEvent::Text(e) => out.push_str(&decode_text(e, index)?),
        QxEvent::GeneralRef(e) => {
            if let Some(ch) = e
                .resolve_char_ref()
                .map_err(|err| record_error(index, err))?
            {
                out.push(ch);
            } else {
                let name = e
                    .decode()
                    .map_err(|err| record_error(index, quick_xml::Error::from(err)))?;
                match prelude.resolve_entity(&name) {
                    Some(value) => out.push_str(value),
                    None => {
                        return Err(record_error(
                            index,
                            quick_xml::Error::from(EscapeError::UnrecognizedEntity(
                                0..name.len(),
                                name.into_owned(),
                            )),
                        ));
                    }
                }
            }
        }
        _ => unreachable!("append_run_event called with a non-text-run event"),
    }
    Ok(())
}

fn record_error(
    index: usize,
    source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> XmlError {
    XmlError::RecordError {
        index,
        source: source.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use crate::prelude::{Encoding, NamespaceContext};
    use proptest::prelude::*;
    use std::collections::HashMap;

    fn prelude(entities: &[(&str, &str)]) -> Arc<Prelude> {
        let mut map = HashMap::new();
        for (k, v) in entities {
            map.insert((*k).into(), (*v).into());
        }
        Arc::new(Prelude {
            encoding: Encoding::Utf8,
            root_name: "root".into(),
            namespaces: NamespaceContext::new(),
            entities: map,
        })
    }

    /// Render a record's events as compact tags for easy assertions.
    fn events(bytes: &[u8], prelude: Arc<Prelude>) -> Vec<String> {
        let mut r = RecordReader::new(bytes, prelude, 0);
        let mut out = Vec::new();
        while let Some(ev) = r.next_event().expect("event ok") {
            out.push(match ev {
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
        out
    }

    #[test]
    fn nested_elements_and_text() {
        assert_eq!(
            events(b"<trade><id>7</id></trade>", prelude(&[])),
            ["S:trade", "S:id", "T:7", "E:id", "E:trade"]
        );
    }

    #[test]
    fn self_closing_expands_to_start_end() {
        assert_eq!(events(b"<trade/>", prelude(&[])), ["S:trade", "E:trade"]);
    }

    #[test]
    fn predefined_entities_in_text() {
        assert_eq!(
            events(b"<t>a &lt; b &amp; c</t>", prelude(&[])),
            ["S:t", "T:a < b & c", "E:t"]
        );
    }

    #[test]
    fn prelude_entity_in_text() {
        assert_eq!(
            events(b"<t>&foo;</t>", prelude(&[("foo", "BAR")])),
            ["S:t", "T:BAR", "E:t"]
        );
    }

    #[test]
    fn char_references_in_text() {
        // Decimal and hex character references resolve and coalesce with the
        // surrounding literal text into one event.
        assert_eq!(
            events(b"<t>&#65;&#x42;C</t>", prelude(&[])),
            ["S:t", "T:ABC", "E:t"]
        );
    }

    #[test]
    fn adjacent_entities_coalesce() {
        assert_eq!(
            events(b"<t>&lt;&gt;</t>", prelude(&[])),
            ["S:t", "T:<>", "E:t"]
        );
    }

    #[test]
    fn entities_at_text_boundaries() {
        assert_eq!(
            events(b"<t>&amp;mid&amp;</t>", prelude(&[])),
            ["S:t", "T:&mid&", "E:t"]
        );
    }

    #[test]
    fn mixed_char_and_named_refs() {
        assert_eq!(
            events(b"<t>x&#38;&foo;y</t>", prelude(&[("foo", "BAR")])),
            ["S:t", "T:x&BARy", "E:t"]
        );
    }

    #[test]
    fn empty_entity_expansion_yields_empty_text() {
        assert_eq!(
            events(b"<t>&empty;</t>", prelude(&[("empty", "")])),
            ["S:t", "T:", "E:t"]
        );
    }

    #[test]
    fn cdata_breaks_a_text_run() {
        // A text run never spans a CDATA section: the entity-bearing text before
        // it and the literal text after it remain distinct events.
        assert_eq!(
            events(b"<t>a&amp;<![CDATA[b]]>c</t>", prelude(&[])),
            ["S:t", "T:a&", "C:b", "T:c", "E:t"]
        );
    }

    #[test]
    fn comment_breaks_a_text_run() {
        // A comment between text nodes is a boundary: the run must not coalesce
        // across it, even though the comment itself is skipped.
        assert_eq!(
            events(b"<t>a<!--c-->&amp;</t>", prelude(&[])),
            ["S:t", "T:a", "T:&", "E:t"]
        );
    }

    #[test]
    fn pi_breaks_a_text_run() {
        assert_eq!(
            events(b"<t>&amp;<?pi?>b</t>", prelude(&[])),
            ["S:t", "T:&", "T:b", "E:t"]
        );
    }

    #[test]
    fn entity_run_terminated_by_element() {
        assert_eq!(
            events(b"<t>&amp;<b/>c</t>", prelude(&[])),
            ["S:t", "T:&", "S:b", "E:b", "T:c", "E:t"]
        );
    }

    #[test]
    fn unknown_entity_in_text_is_record_error() {
        let mut r = RecordReader::new(b"<t>&nope;</t>", prelude(&[]), 3);
        assert!(r.next_event().is_ok(), "start reads fine");
        assert!(matches!(
            r.next_event(),
            Err(XmlError::RecordError { index: 3, .. })
        ));
    }

    #[test]
    fn cdata_is_raw() {
        assert_eq!(
            events(b"<t><![CDATA[<not>&x;]]></t>", prelude(&[])),
            ["S:t", "C:<not>&x;", "E:t"]
        );
    }

    #[test]
    fn comments_and_pis_are_skipped() {
        assert_eq!(
            events(b"<t><!-- c --><?pi?>hi</t>", prelude(&[])),
            ["S:t", "T:hi", "E:t"]
        );
    }

    #[test]
    fn attributes_decode_values() {
        let mut r = RecordReader::new(
            br#"<a k1="v1" k2="a&amp;b&e;"/>"#,
            prelude(&[("e", "X")]),
            0,
        );
        let ev = r.next_event().unwrap().unwrap();
        let Event::Start { name, attrs } = ev else {
            panic!("expected start");
        };
        assert_eq!(name.as_ref(), b"a");
        let got: Vec<(String, String)> = attrs
            .iter()
            .map(|a| {
                let a = a.unwrap();
                (
                    String::from_utf8(a.key.to_vec()).unwrap(),
                    a.value.into_owned(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("k1".to_string(), "v1".to_string()),
                ("k2".to_string(), "a&bX".to_string()),
            ]
        );
    }

    #[test]
    fn lexical_qnames_keep_prefixes() {
        assert_eq!(
            events(b"<p:trade><p:id>1</p:id></p:trade>", prelude(&[])),
            ["S:p:trade", "S:p:id", "T:1", "E:p:id", "E:p:trade"]
        );
    }

    #[test]
    fn mismatched_end_tag_is_record_error() {
        let mut r = RecordReader::new(b"<a></b>", prelude(&[]), 7);
        assert!(r.next_event().is_ok(), "start reads fine");
        assert!(matches!(
            r.next_event(),
            Err(XmlError::RecordError { index: 7, .. })
        ));
    }

    /// One element-content segment paired with the text it should decode to: a
    /// literal run, a predefined or custom entity, or a character reference.
    fn arb_text_seg() -> impl Strategy<Value = (String, String)> {
        prop_oneof![
            "[a-zA-Z0-9 .,_:-]{1,8}".prop_map(|s| (s.clone(), s)),
            Just(("&lt;".to_string(), "<".to_string())),
            Just(("&gt;".to_string(), ">".to_string())),
            Just(("&amp;".to_string(), "&".to_string())),
            Just(("&apos;".to_string(), "'".to_string())),
            Just(("&quot;".to_string(), "\"".to_string())),
            Just(("&foo;".to_string(), "BAR".to_string())),
            (0x20u32..0x7f).prop_map(|c| {
                let ch = char::from_u32(c).unwrap();
                (format!("&#{c};"), ch.to_string())
            }),
            (0x20u32..0x7f).prop_map(|c| {
                let ch = char::from_u32(c).unwrap();
                (format!("&#x{c:x};"), ch.to_string())
            }),
        ]
    }

    proptest! {
        /// Any interleaving of literal text, entity references, and character
        /// references in element content coalesces back into exactly one
        /// resolved `Text` event whose value is the concatenation of the
        /// segments' expansions.
        #[test]
        fn coalesced_text_roundtrips(segs in prop::collection::vec(arb_text_seg(), 1..12)) {
            let mut raw = String::from("<t>");
            let mut expected = String::new();
            for (src, decoded) in &segs {
                raw.push_str(src);
                expected.push_str(decoded);
            }
            raw.push_str("</t>");

            let got = events(raw.as_bytes(), prelude(&[("foo", "BAR")]));
            prop_assert_eq!(got, vec!["S:t".to_string(), format!("T:{expected}"), "E:t".to_string()]);
        }
    }
}
