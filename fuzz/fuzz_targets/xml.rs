#![no_main]
//! `xml::decode` and the `Reader` to the end on arbitrary bytes, in both
//! modes (plain XML and with the HTML names), asserting the step bound the
//! linearity tests hold (whatever the input, the work stays linear), that
//! the events nest and never go deeper than the limit, and that an error
//! is final.
//! Bytes that do not decode are also read as lossy UTF-8, so the reader
//! sees every input, not only the valid UTF-8 or UTF-16 ones.

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

fuzz_target!(|data: &[u8]| {
    match xml::decode(data) {
        Ok(text) => read_all(&text),
        Err(_) => read_all(&String::from_utf8_lossy(data)),
    }
});
