#![no_main]
//! `xml::decode` and the `Reader` to the end on arbitrary bytes, in both
//! modes (plain XML and with the HTML names), asserting the step bound the
//! linearity tests hold (whatever the input, the work stays linear), that
//! the events nest and never go deeper than the limit, and that an error
//! is final.
//! Bytes that do not decode are also read as lossy UTF-8, so the reader
//! sees every input, not only the valid UTF-8 or UTF-16 ones.
//! The same text also goes through the XHTML chapter reader
//! (`doc::xhtml_read`), asserting its step bound, that it agrees with the
//! XML reader on well-formedness, and that what it builds writes as text.

use libfuzzer_sys::fuzz_target;

fn read_all(text: &str) {
    for html_entities in [false, true] {
        let mut r = xml::Reader::new(
            text,
            xml::Options {
                max_depth: xml::MAX_DEPTH,
                html_entities,
            },
        );
        let mut depth: u32 = 0;
        let end = loop {
            match r.next() {
                Ok(Some(xml::Event::Start(_))) => {
                    depth += 1;
                    assert!(depth <= xml::MAX_DEPTH, "nested deeper than the limit");
                }
                Ok(Some(xml::Event::End(_))) => depth -= 1,
                Ok(Some(xml::Event::Text(t))) => assert!(depth > 0 && !t.is_empty()),
                other => break other,
            }
        };
        if end == Ok(None) {
            assert_eq!(depth, 0, "a well-formed end with elements open");
        }
        // An error is final: the next call returns it again.
        if let Err(e) = end {
            assert!(e.line >= 1);
            assert_eq!(r.next(), Err(e));
        }
        assert!(
            r.steps() <= xml::STEPS_PER_BYTE * text.len() as u64,
            "{} steps for {} bytes",
            r.steps(),
            text.len()
        );
    }
}

/// A node budget the fuzzer reaches at speed (a 64 KiB input holds
/// ~16,000 empty elements); the shipped 2^19 is pinned by the doc tests.
const NODES: usize = 4096;

fn xhtml(text: &str) {
    use doc::xhtml_read::{STEPS_PER_BYTE, STEPS_PER_NODE, XhtmlError, has_text, read_steps};
    let limits = doc::Limits {
        nodes: NODES,
        ..doc::Limits::DEFAULT
    };
    let (r, steps) = read_steps(text, limits);
    assert!(
        steps <= STEPS_PER_BYTE * text.len() as u64 + STEPS_PER_NODE * NODES as u64,
        "{steps} steps for {} bytes",
        text.len()
    );
    let mut x = xml::Reader::new(
        text,
        xml::Options {
            max_depth: xml::MAX_DEPTH,
            html_entities: true,
        },
    );
    let xml_ok = loop {
        match x.next() {
            Ok(Some(_)) => {}
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    match r {
        Ok((d, losses)) => {
            assert_eq!(xml_ok, Ok(()), "read a document the XML reader refuses");
            assert!(losses.contains(&doc::Loss::Styles));
            let _ = has_text(&d);
            let _ = doc::txt::write_limited(&d, doc::Limits::DEFAULT);
        }
        // The XML error is the XML reader's own; a budget may stop the
        // chapter before an error further on.
        Err(XhtmlError::Xml(e)) => assert_eq!(xml_ok, Err(e)),
        Err(XhtmlError::Limit(_)) => {}
    }
}

fuzz_target!(|data: &[u8]| {
    match xml::decode(data) {
        Ok(text) => {
            read_all(&text);
            xhtml(&text);
        }
        Err(_) => {
            let text = String::from_utf8_lossy(data);
            read_all(&text);
            xhtml(&text);
        }
    }
});
