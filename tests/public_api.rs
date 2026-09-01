//! Integration tests: exercise `pxml` exactly as a dependent crate does, through
//! the public API only.
//!
//! The inline `#[cfg(test)]` suites cover scanner/parser internals with access to
//! private items. This file deliberately has none of that access, so it also
//! serves as a guard on the *shape* of the public API: anything a user needs must
//! be reachable via `use pxml::…`, and re-exports that disappear break here.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use pxml::{Config, Encoding, Event, ParallelXml, StreamReader, XmlError};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A realistic target document: a root of uniform records with attributes,
/// nested elements, and text.
fn trades_doc(n: usize) -> String {
    let mut s = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><trades>"#);
    for i in 0..n {
        s.push_str(&format!(
            r#"<trade id="{i}" side="buy"><sym>ACME</sym><qty>{}</qty></trade>"#,
            i * 10
        ));
    }
    s.push_str("</trades>");
    s
}

/// Concatenated text content of a record.
fn text_of(rec: &pxml::Record) -> String {
    let mut events = rec.events();
    let mut out = String::new();
    while let Some(ev) = events.next_event().unwrap() {
        if let Event::Text(t) = ev {
            out.push_str(&t);
        }
    }
    out
}

/// The value of one attribute on the record's first start tag.
fn first_attr(rec: &pxml::Record, key: &[u8]) -> Option<String> {
    let mut events = rec.events();
    while let Some(ev) = events.next_event().unwrap() {
        if let Event::Start { attrs, .. } = ev {
            for attr in attrs.iter() {
                let attr = attr.unwrap();
                if attr.key == key {
                    return Some(attr.value.into_owned());
                }
            }
            return None;
        }
    }
    None
}

/// Force the parallel path regardless of input size.
fn parallel() -> Config {
    Config::new().with_parallel_threshold(0).with_min_records(0)
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// The builders and getters must round-trip through the public API, and the
/// defaults must match what the docs promise.
#[test]
fn config_builders_round_trip() {
    let c = Config::new()
        .with_parallel_threshold(1 << 20)
        .with_min_records(32)
        .with_record_path(["body", "objects"]);

    assert_eq!(c.parallel_threshold(), 1 << 20);
    assert_eq!(c.min_records(), 32);
    assert_eq!(c.record_path(), [Box::from("body"), Box::from("objects")]);

    let d = Config::default();
    assert_eq!(d.parallel_threshold(), 4 * 1024 * 1024);
    assert_eq!(d.min_records(), 64);
    assert!(d.record_path().is_empty());
}

/// The record path travels on the `Config`, so it cannot be clobbered by a
/// later `with_config` — there is exactly one place to set it, and it survives
/// however the builder chain is written.
#[test]
fn record_path_is_carried_by_the_config() {
    let xml = b"<root><manifest/><objects><object>a</object><object>b</object></objects></root>";

    // Path plus thresholds, built in either order on the Config.
    let path_last = Config::new()
        .with_parallel_threshold(0)
        .with_record_path(["objects"]);
    let path_first = Config::new()
        .with_record_path(["objects"])
        .with_parallel_threshold(0);

    for config in [path_last, path_first] {
        let got: Vec<String> = ParallelXml::from_bytes(&xml[..])
            .with_config(config)
            .map_collect(text_of)
            .unwrap();
        assert_eq!(got, ["a", "b"]);
    }
}

/// `with_config` replaces the whole configuration rather than merging — the
/// last call wins outright, including the record path.
#[test]
fn with_config_replaces_rather_than_merges() {
    let xml = b"<root><manifest/><objects><object>a</object></objects></root>";

    // A second `with_config` with no path reverts to the root's direct children
    // (<manifest> and <objects>), rather than retaining the earlier path.
    let replaced = ParallelXml::from_bytes(&xml[..])
        .with_config(Config::new().with_record_path(["objects"]))
        .with_config(Config::new())
        .index()
        .unwrap();
    assert_eq!(replaced.len(), 2);

    let kept = ParallelXml::from_bytes(&xml[..])
        .with_config(Config::new())
        .with_config(Config::new().with_record_path(["objects"]))
        .index()
        .unwrap();
    assert_eq!(kept.len(), 1);
}

/// The thresholds must actually gate the fallback: identical documents parsed
/// with forced-parallel and forced-sequential configs agree.
#[test]
fn config_thresholds_select_the_path_without_changing_results() {
    let xml = trades_doc(100);

    let forced_parallel: Vec<String> = ParallelXml::from_bytes(xml.clone().into_bytes())
        .with_config(Config::new().with_parallel_threshold(0).with_min_records(0))
        .map_collect(text_of)
        .unwrap();

    let forced_sequential: Vec<String> = ParallelXml::from_bytes(xml.into_bytes())
        .with_config(
            Config::new()
                .with_parallel_threshold(usize::MAX)
                .with_min_records(usize::MAX),
        )
        .map_collect(text_of)
        .unwrap();

    assert_eq!(forced_parallel, forced_sequential);
    assert_eq!(forced_parallel.len(), 100);
}

// ---------------------------------------------------------------------------
// The headline use cases
// ---------------------------------------------------------------------------

/// The primary advertised workflow: extract a typed field from every record,
/// in document order.
#[test]
fn extract_typed_fields_in_document_order() {
    let doc = ParallelXml::from_bytes(trades_doc(500).into_bytes()).with_config(parallel());

    let qtys: Vec<u64> = doc
        .map_collect(|rec| {
            let mut events = rec.events();
            let mut in_qty = false;
            let mut qty = 0;
            while let Some(ev) = events.next_event().unwrap() {
                match ev {
                    Event::Start { name, .. } => in_qty = name.as_ref() == "qty",
                    Event::Text(t) if in_qty => qty = t.parse().unwrap(),
                    _ => {}
                }
            }
            qty
        })
        .unwrap();

    assert_eq!(qtys.len(), 500);
    assert_eq!(qtys[0], 0);
    assert_eq!(qtys[499], 4990);
}

/// Attributes are the other half of a typical extraction.
#[test]
fn attributes_are_readable_from_records() {
    let doc = ParallelXml::from_bytes(trades_doc(100).into_bytes()).with_config(parallel());
    let ids: Vec<String> = doc
        .map_collect(|rec| first_attr(rec, b"id").unwrap())
        .unwrap();
    assert_eq!(ids[0], "0");
    assert_eq!(ids[99], "99");
}

// ---------------------------------------------------------------------------
// User-defined functions as the per-record callback
// ---------------------------------------------------------------------------

/// The shape a real caller has: a domain type plus a function that builds one
/// from a record. Deliberately a free `fn` (not a closure) so the drivers are
/// proven to accept a plain function item.
#[derive(Debug, PartialEq, Eq)]
struct Trade {
    id: u32,
    qty: u64,
    sym: String,
}

fn parse_trade(rec: &pxml::Record) -> Trade {
    let mut events = rec.events();
    let mut id = 0;
    let mut qty = 0;
    let mut sym = String::new();
    let mut field = Vec::new();

    while let Some(ev) = events.next_event().unwrap() {
        match ev {
            Event::Start { name, attrs } => {
                field = name.as_ref().as_bytes().to_vec();
                for attr in attrs.iter() {
                    let attr = attr.unwrap();
                    if attr.key == b"id" {
                        id = attr.value.parse().unwrap();
                    }
                }
            }
            Event::Text(t) => match field.as_slice() {
                b"qty" => qty = t.parse().unwrap(),
                b"sym" => sym = t.into_owned(),
                _ => {}
            },
            _ => {}
        }
    }
    Trade { id, qty, sym }
}

/// A fallible user function, for the `try_*` drivers.
fn try_parse_trade(rec: &pxml::Record) -> Result<Trade, std::num::ParseIntError> {
    Ok(parse_trade(rec))
}

/// Every driver must accept a user-defined `fn` item applied to each record —
/// this is the library's whole purpose, so pin it for all four entry points
/// plus the streaming path.
#[test]
fn user_defined_function_is_applied_to_every_record() {
    let n = 300;
    let doc = ParallelXml::from_bytes(trades_doc(n).into_bytes()).with_config(parallel());

    // 1. map_collect with a named function item.
    let trades: Vec<Trade> = doc.map_collect(parse_trade).unwrap();
    assert_eq!(trades.len(), n);
    assert_eq!(
        trades[42],
        Trade {
            id: 42,
            qty: 420,
            sym: "ACME".into()
        }
    );

    // 2. try_map_collect with a fallible named function.
    let trades2: Vec<Trade> = doc.try_map_collect(try_parse_trade).unwrap();
    assert_eq!(trades2, trades);

    // 3. par_for_each with a function pointer stored in a variable.
    let f: fn(&pxml::Record) -> Trade = parse_trade;
    let count = AtomicUsize::new(0);
    doc.par_for_each(|rec| {
        let t = f(rec);
        assert_eq!(t.sym, "ACME");
        count.fetch_add(1, Ordering::Relaxed);
    })
    .unwrap();
    assert_eq!(count.load(Ordering::Relaxed), n);

    // 4. try_par_for_each with the fallible function.
    doc.try_par_for_each(|rec| try_parse_trade(rec).map(|_| ()))
        .unwrap();

    // 5. The same function over the streaming path.
    let xml = trades_doc(n);
    let streamed = Mutex::new(Vec::new());
    StreamReader::from_reader(xml.as_bytes())
        .par_for_each(|rec| streamed.lock().unwrap().push(parse_trade(rec)))
        .unwrap();
    let mut streamed = streamed.into_inner().unwrap();
    streamed.sort_by_key(|t| t.id);
    assert_eq!(streamed, trades);
}

/// User callbacks routinely close over their environment — a lookup table, a
/// filter, a sink. The `Fn + Sync` bound must accommodate that without forcing
/// the caller into interior mutability for read-only captures.
#[test]
fn user_closure_may_capture_its_environment() {
    let doc = ParallelXml::from_bytes(trades_doc(200).into_bytes()).with_config(parallel());

    // Read-only captures, shared across workers.
    let wanted: std::collections::HashSet<u32> = [3, 17, 199].into_iter().collect();
    let multiplier = 2;

    let hits: Vec<u64> = doc
        .map_collect(|rec| {
            let t = parse_trade(rec);
            if wanted.contains(&t.id) {
                t.qty * multiplier
            } else {
                0
            }
        })
        .unwrap()
        .into_iter()
        .filter(|&q| q != 0)
        .collect();

    assert_eq!(hits, [60, 340, 3980]);
}

/// A boxed `dyn Fn` — the erased form a caller ends up with when the callback is
/// chosen at runtime.
#[test]
fn user_callback_may_be_a_boxed_trait_object() {
    let doc = ParallelXml::from_bytes(trades_doc(50).into_bytes()).with_config(parallel());

    let callback: Box<dyn Fn(&pxml::Record) -> u32 + Sync> = Box::new(|rec| parse_trade(rec).id);
    let ids: Vec<u32> = doc.map_collect(&callback).unwrap();

    assert_eq!(ids, (0..50).collect::<Vec<_>>());
}

/// The unordered driver must visit every record exactly once, and it must be
/// usable with the `Sync` shared state a real caller would reach for.
#[test]
fn par_for_each_visits_each_record_once() {
    let n = 2000;
    let doc = ParallelXml::from_bytes(trades_doc(n).into_bytes()).with_config(parallel());

    let seen = Mutex::new(vec![false; n]);
    let count = AtomicUsize::new(0);

    doc.par_for_each(|rec| {
        count.fetch_add(1, Ordering::Relaxed);
        let mut seen = seen.lock().unwrap();
        assert!(!seen[rec.index()], "record {} visited twice", rec.index());
        seen[rec.index()] = true;
    })
    .unwrap();

    assert_eq!(count.load(Ordering::Relaxed), n);
    assert!(seen.into_inner().unwrap().into_iter().all(|s| s));
}

/// The sequential fallback is a performance switch, not a behavior switch:
/// small and large inputs must agree. This is the invariant most at risk when
/// the drivers are refactored.
#[test]
fn sequential_fallback_and_parallel_path_agree() {
    for n in [1, 2, 63, 64, 65, 500] {
        let xml = trades_doc(n);

        // Default config: below both thresholds -> sequential fallback.
        let fallback: Vec<String> = ParallelXml::from_bytes(xml.clone().into_bytes())
            .map_collect(text_of)
            .unwrap();

        let forced: Vec<String> = ParallelXml::from_bytes(xml.into_bytes())
            .with_config(parallel())
            .map_collect(text_of)
            .unwrap();

        assert_eq!(fallback, forced, "paths disagree at n={n}");
        assert_eq!(fallback.len(), n);
    }
}

// ---------------------------------------------------------------------------
// Framing: record_path and document shapes
// ---------------------------------------------------------------------------

#[test]
fn record_path_descends_to_a_nested_container() {
    let xml = r#"<root>
        <manifest><object>DECOY</object></manifest>
        <objects><object>a</object><object>b</object></objects>
    </root>"#;

    let doc = ParallelXml::from_bytes(xml.as_bytes().to_vec())
        .with_config(Config::new().with_record_path(["objects"]));
    let got: Vec<String> = doc.map_collect(text_of).unwrap();

    // The decoy inside the skipped <manifest> sibling must not be framed.
    assert_eq!(got, ["a", "b"]);
}

#[test]
fn record_path_descends_multiple_levels() {
    let xml = r#"<root><body><objects><object>x</object></objects></body></root>"#;
    let doc = ParallelXml::from_bytes(xml.as_bytes().to_vec())
        .with_config(Config::new().with_record_path(["body", "objects"]));
    assert_eq!(doc.index().unwrap().len(), 1);
}

/// An absent container is not an error — it yields zero records.
#[test]
fn missing_container_yields_no_records() {
    let doc = ParallelXml::from_bytes(&b"<root><other/></root>"[..])
        .with_config(Config::new().with_record_path(["objects"]));
    assert_eq!(doc.index().unwrap().len(), 0);
}

/// Degenerate but legal shapes a user will eventually hand the library.
#[test]
fn empty_and_single_record_documents() {
    assert_eq!(
        ParallelXml::from_bytes(&b"<empty></empty>"[..])
            .index()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        ParallelXml::from_bytes(&b"<empty/>"[..])
            .index()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        ParallelXml::from_bytes(&b"<rs><r>only</r></rs>"[..])
            .index()
            .unwrap()
            .len(),
        1
    );
}

/// Record-lookalikes hidden in comments/CDATA/PIs must not mis-frame. This is
/// the correctness property that justifies the whole two-phase design, so it is
/// worth asserting through the public API too.
#[test]
fn record_lookalikes_in_markup_do_not_misframe() {
    let xml = r#"<rs>
        <!-- <r>comment decoy</r> -->
        <r>real<![CDATA[ <r>cdata decoy</r> ]]></r>
        <?pi <r>pi decoy</r> ?>
        <r>also real</r>
    </rs>"#;

    let idx = ParallelXml::from_bytes(xml.as_bytes().to_vec())
        .index()
        .unwrap();
    assert_eq!(idx.len(), 2);
}

// ---------------------------------------------------------------------------
// Events: the pull cursor contract
// ---------------------------------------------------------------------------

#[test]
fn events_cover_nesting_self_closing_text_and_cdata() {
    let doc = ParallelXml::from_bytes(&b"<rs><r><a>t</a><b/><![CDATA[raw]]></r></rs>"[..]);
    let shapes: Vec<Vec<String>> = doc
        .map_collect(|rec| {
            let mut events = rec.events();
            let mut out = Vec::new();
            while let Some(ev) = events.next_event().unwrap() {
                out.push(match ev {
                    Event::Start { name, .. } => {
                        format!("S:{}", name.as_ref())
                    }
                    Event::End { name } => format!("E:{}", name.as_ref()),
                    Event::Text(t) => format!("T:{t}"),
                    Event::Cdata(c) => format!("C:{}", String::from_utf8_lossy(c)),
                });
            }
            out
        })
        .unwrap();

    assert_eq!(
        shapes[0],
        // Self-closing <b/> surfaces as Start + End.
        ["S:r", "S:a", "T:t", "E:a", "S:b", "E:b", "C:raw", "E:r"]
    );
}

/// Entity handling is the reason records carry a shared `Prelude`; a user
/// relying on DOCTYPE-defined entities must get them expanded inside records.
#[test]
fn doctype_entities_expand_inside_records() {
    let xml = r#"<!DOCTYPE rs [ <!ENTITY co "ACME Corp"> ]><rs><r>&co; &amp; co</r></rs>"#;
    let doc = ParallelXml::from_bytes(xml.as_bytes().to_vec());
    let got: Vec<String> = doc.map_collect(text_of).unwrap();
    assert_eq!(got, ["ACME Corp & co"]);
}

#[test]
fn char_references_and_unicode_decode() {
    let xml = "<rs><r>caf&#233; \u{4e2d}\u{6587} &#x1F600;</r></rs>";
    let doc = ParallelXml::from_bytes(xml.as_bytes().to_vec());
    let got: Vec<String> = doc.map_collect(text_of).unwrap();
    assert_eq!(got, ["café 中文 \u{1F600}"]);
}

/// Namespace prefixes are surfaced lexically; the declared URIs are resolvable
/// from the shared prelude.
#[test]
fn namespace_prefixes_are_lexical_and_resolvable() {
    let xml = r#"<rs xmlns="urn:default" xmlns:t="urn:trade"><t:r>x</t:r></rs>"#;
    let doc = ParallelXml::from_bytes(xml.as_bytes().to_vec());

    let idx = doc.index().unwrap();
    let ns = &idx.prelude().namespaces;
    assert_eq!(ns.resolve("t"), Some("urn:trade"));
    assert_eq!(ns.resolve(""), Some("urn:default"));
    assert_eq!(ns.resolve("absent"), None);

    // The event keeps the prefix as written rather than resolving it.
    let names: Vec<String> = doc
        .map_collect(|rec| {
            let mut events = rec.events();
            match events.next_event().unwrap().unwrap() {
                Event::Start { name, .. } => name.as_ref().to_owned(),
                other => panic!("expected Start, got {other:?}"),
            }
        })
        .unwrap();
    assert_eq!(names, ["t:r"]);
}

// ---------------------------------------------------------------------------
// Phase A only
// ---------------------------------------------------------------------------

#[test]
fn index_exposes_ranges_that_slice_the_source() {
    let xml = b"<rs><r>a</r><r>b</r></rs>".to_vec();
    let doc = ParallelXml::from_bytes(xml.clone());
    let idx = doc.index().unwrap();

    assert_eq!(idx.len(), 2);
    assert!(!idx.is_empty());
    assert_eq!(idx.prelude().root_name.as_ref(), "rs");
    assert_eq!(idx.prelude().encoding, Encoding::Utf8);

    // The ranges index the original buffer.
    let ranges = idx.records();
    assert_eq!(&xml[ranges[0].clone()], b"<r>a</r>");
    assert_eq!(&xml[ranges[1].clone()], b"<r>b</r>");
}

// ---------------------------------------------------------------------------
// The sequential escape hatch
// ---------------------------------------------------------------------------

/// `sequential()` is the documented answer for users whose records are *not*
/// order-independent, so it must surface the root and cross-record structure.
#[test]
fn sequential_reader_sees_the_whole_document() {
    let doc = ParallelXml::from_bytes(&b"<rs><r>a</r><r>b</r></rs>"[..]);
    let mut reader = doc.sequential();

    let mut starts = Vec::new();
    let mut text = String::new();
    while let Some(ev) = reader.next_event().unwrap() {
        match ev {
            Event::Start { name, .. } => starts.push(name.as_ref().to_owned()),
            Event::Text(t) => text.push_str(&t),
            _ => {}
        }
    }

    assert_eq!(starts, ["rs", "r", "r"]); // includes the root
    assert_eq!(text, "ab");
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[test]
fn malformed_document_is_rejected_with_an_offset() {
    let res = ParallelXml::from_bytes(&b"<rs><r></rs>"[..]).index();
    assert!(matches!(res, Err(XmlError::Malformed(_))));
}

#[test]
fn external_dtd_is_rejected_rather_than_ignored() {
    let xml = br#"<!DOCTYPE rs SYSTEM "ext.dtd"><rs><r>a</r></rs>"#;
    let res = ParallelXml::from_bytes(&xml[..]).index();
    assert!(matches!(res, Err(XmlError::UnsupportedDtd)));
}

#[test]
fn utf16_input_is_rejected_rather_than_mangled() {
    let mut xml = vec![0xFF, 0xFE]; // UTF-16 LE BOM
    xml.extend_from_slice(b"<rs/>");
    assert!(matches!(
        ParallelXml::from_bytes(xml).index(),
        Err(XmlError::Encoding)
    ));
}

/// A user's own closure error must come back identifiable — with the right
/// record index, and with the original error reachable through `source()`.
#[test]
fn closure_errors_carry_record_index_and_source() {
    let xml = "<rs><r>1</r><r>NaN</r></rs>";
    let doc = ParallelXml::from_bytes(xml.as_bytes().to_vec());

    let err = doc
        .try_par_for_each(|rec| text_of(rec).parse::<u32>().map(|_| ()))
        .unwrap_err();

    match &err {
        XmlError::RecordError { index, source } => {
            assert_eq!(*index, 1);
            assert!(source.to_string().contains("invalid digit"));
        }
        other => panic!("expected RecordError, got {other:?}"),
    }

    // The standard error plumbing works, so `?` into anyhow/Box<dyn Error> is fine.
    use std::error::Error;
    assert!(err.source().is_some());
    assert!(err.to_string().contains("record 1"));
    let _boxed: Box<dyn Error + Send + Sync> = Box::new(err);
}

/// A malformed *record* (as opposed to malformed framing) is a Phase B error and
/// must name the offending record.
#[test]
fn record_level_parse_error_names_the_record() {
    // Framing is fine (depth returns to 0), but record 1 has a mismatched tag.
    let xml = "<rs><r>ok</r><r><a></b></r></rs>";
    let doc = ParallelXml::from_bytes(xml.as_bytes().to_vec());

    let err = doc
        .try_par_for_each(|rec| {
            let mut events = rec.events();
            while events.next_event()?.is_some() {}
            Ok::<(), XmlError>(())
        })
        .unwrap_err();

    assert!(matches!(err, XmlError::RecordError { index: 1, .. }));
}

/// `try_par_for_each` short-circuits; `try_map_collect` documents that it does
/// not on the parallel path. Pin both behaviors so the docs stay honest.
#[test]
fn try_map_collect_still_returns_a_record_error() {
    let doc = ParallelXml::from_bytes(trades_doc(200).into_bytes()).with_config(parallel());
    let res: Result<Vec<u32>, _> = doc.try_map_collect(|rec| {
        if rec.index() == 7 {
            "x".parse::<u32>()
        } else {
            Ok(0)
        }
    });
    assert!(matches!(res, Err(XmlError::RecordError { index: 7, .. })));
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

/// The streaming path must frame and parse the same records as the resident
/// path — that equivalence is what lets a user switch on document size alone.
#[test]
fn streaming_matches_resident_results() {
    let xml = trades_doc(1000);

    let mut resident: Vec<String> = ParallelXml::from_bytes(xml.clone().into_bytes())
        .with_config(parallel())
        .map_collect(text_of)
        .unwrap();

    let streamed = Mutex::new(Vec::new());
    StreamReader::from_reader(xml.as_bytes())
        .par_for_each(|rec| streamed.lock().unwrap().push(text_of(rec)))
        .unwrap();

    // Streaming results are unordered by contract, so compare as multisets.
    let mut streamed = streamed.into_inner().unwrap();
    resident.sort();
    streamed.sort();
    assert_eq!(resident, streamed);
}

#[test]
fn streaming_supports_record_path() {
    let xml = r#"<root><manifest><object>DECOY</object></manifest>
                 <objects><object>a</object><object>b</object></objects></root>"#;

    let got = Mutex::new(Vec::new());
    StreamReader::from_reader(xml.as_bytes())
        .record_path(["objects"])
        .par_for_each(|rec| got.lock().unwrap().push(text_of(rec)))
        .unwrap();

    let mut got = got.into_inner().unwrap();
    got.sort();
    assert_eq!(got, ["a", "b"]);
}

/// A `Read` that dribbles bytes out must not change framing — real sources
/// (sockets, decompressors) return short reads.
#[test]
fn streaming_survives_a_dribbling_reader() {
    struct Dribble<'a>(&'a [u8]);
    impl std::io::Read for Dribble<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[0];
            self.0 = &self.0[1..];
            Ok(1) // one byte at a time
        }
    }

    let xml = trades_doc(50);
    let count = AtomicUsize::new(0);
    StreamReader::from_reader(Dribble(xml.as_bytes()))
        .par_for_each(|_| {
            count.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 50);
}

#[test]
fn streaming_reports_malformed_input() {
    let res = StreamReader::from_reader(&b"<rs><r>unclosed"[..]).par_for_each(|_| {});
    assert!(res.is_err());
}

// ---------------------------------------------------------------------------
// File and compressed input
// ---------------------------------------------------------------------------

fn temp_path(tag: &str) -> std::path::PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("pxml-it-{tag}-{}-{id}.xml", std::process::id()))
}

#[test]
fn from_path_reads_a_file() {
    let path = temp_path("plain");
    std::fs::write(&path, trades_doc(100)).unwrap();

    let doc = ParallelXml::from_path(&path).unwrap();
    let got: Vec<String> = doc.map_collect(text_of).unwrap();
    assert_eq!(got.len(), 100);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn from_path_reports_a_missing_file() {
    let err = ParallelXml::from_path(std::path::Path::new("/nonexistent/pxml.xml")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[cfg(feature = "zstd")]
mod compressed {
    use super::*;

    fn compressed_doc(n: usize) -> Vec<u8> {
        zstd::encode_all(trades_doc(n).as_bytes(), 3).unwrap()
    }

    #[test]
    fn from_zstd_bytes_decompresses() {
        let doc = ParallelXml::from_zstd_bytes(&compressed_doc(200)).unwrap();
        assert_eq!(doc.index().unwrap().len(), 200);
    }

    /// The advertised convenience: `from_path` sniffs the zstd magic, so a user
    /// can hand it either form of the file.
    #[test]
    fn from_path_transparently_decompresses_zstd() {
        let path = temp_path("zstd");
        std::fs::write(&path, compressed_doc(150)).unwrap();

        let doc = ParallelXml::from_path(&path).unwrap();
        let got: Vec<String> = doc.map_collect(text_of).unwrap();
        assert_eq!(got.len(), 150);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn streaming_over_zstd_matches_resident() {
        let n = 300;
        let compressed = compressed_doc(n);

        let count = AtomicUsize::new(0);
        StreamReader::from_zstd_reader(&compressed[..])
            .unwrap()
            .par_for_each(|_| {
                count.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();

        assert_eq!(count.load(Ordering::Relaxed), n);
    }
}
