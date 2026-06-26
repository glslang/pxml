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

use std::sync::Arc;

use quick_xml::Reader;
use quick_xml::events::Event as QxEvent;

use crate::XmlError;
use crate::event::{Attrs, Event};
use crate::prelude::Prelude;

/// A StAX-style pull cursor over one record's events.
pub struct RecordReader<'doc> {
    reader: Reader<&'doc [u8]>,
    prelude: Arc<Prelude>,
    current: Option<QxEvent<'doc>>,
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
            index,
        }
    }

    /// This record's position in document order.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Advance to the next event, or `Ok(None)` at the end of the record.
    /// Comments, PIs, and the XML/DOCTYPE declarations are skipped.
    pub fn next_event(&mut self) -> Result<Option<Event<'_>>, XmlError> {
        loop {
            let ev = self
                .reader
                .read_event()
                .map_err(|e| record_error(self.index, e))?;
            match ev {
                QxEvent::Eof => return Ok(None),
                QxEvent::Comment(_) | QxEvent::PI(_) | QxEvent::Decl(_) | QxEvent::DocType(_) => {
                    continue;
                }
                keep => {
                    self.current = Some(keep);
                    break;
                }
            }
        }

        let index = self.index;
        let prelude: &Prelude = self.prelude.as_ref();
        let event = match self.current.as_ref().expect("event stored above") {
            QxEvent::Start(e) | QxEvent::Empty(e) => Event::Start {
                name: e.name(),
                attrs: Attrs::new(e.attributes_raw(), prelude, index),
            },
            QxEvent::End(e) => Event::End { name: e.name() },
            QxEvent::Text(e) => {
                let text = e
                    .unescape_with(|name| prelude.resolve_entity(name))
                    .map_err(|err| record_error(index, err))?;
                Event::Text(text)
            }
            QxEvent::CData(e) => {
                let bytes: &[u8] = e;
                Event::Cdata(bytes)
            }
            // Comment/PI/Decl/DocType are skipped above; Eof returns early.
            _ => unreachable!("non-surfaced event was stored"),
        };
        Ok(Some(event))
    }
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
}
