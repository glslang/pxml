//! Phase B — per-record pull parser over a single record's byte slice.
//!
//! [`RecordReader`] wraps `quick_xml`'s reader over `&[u8]`, seeded with the
//! shared [`Prelude`] so namespace resolution and `&entity;` expansion are
//! correct even though the record is parsed in isolation. It is a thin adapter:
//! `quick_xml` does the heavy lifting; we map its events to [`Event`].

use std::sync::Arc;

use crate::XmlError;
use crate::event::Event;
use crate::prelude::Prelude;

/// A StAX-style pull cursor over one record's events.
pub struct RecordReader<'doc> {
    bytes: &'doc [u8],
    prelude: Arc<Prelude>,
    // Future: owns a `quick_xml::NsReader<&'doc [u8]>` seeded from `prelude`.
}

impl<'doc> RecordReader<'doc> {
    /// Build a reader over a single record's slice with shared prolog context.
    pub(crate) fn new(bytes: &'doc [u8], prelude: Arc<Prelude>) -> Self {
        Self { bytes, prelude }
    }

    /// Advance to the next event, or `Ok(None)` at the end of the record.
    pub fn next_event(&mut self) -> Result<Option<Event<'doc>>, XmlError> {
        let _ = (self.bytes, &self.prelude);
        todo!("Phase B: map quick_xml events to crate::Event")
    }
}
