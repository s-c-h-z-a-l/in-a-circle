#![no_main]
//! `subtitles::read_ttml` on arbitrary bytes, with the checks every subtitle
//! reader shares (see `subtitles/common.rs`), and the TTML checks in them: a
//! `Malformed` names a line of the input, `NotTtml` and `Unsupported` are
//! verdicts on the whole document. The input is read as it is, so the XML
//! corpus (the seeds in `fuzz/corpus/ttml`) is mutated as XML. It is also read
//! inside a fixed document, as the text of one paragraph, so a mutated text
//! reaches the paragraph, span and break code, not only the root check. The
//! timing of one paragraph is checked against its own model (see
//! [`check_paragraph_timing`]), the one check of the reader's arithmetic that
//! the other checks cannot make.

use libfuzzer_sys::fuzz_target;
use subtitles::ReadError;

#[path = "subtitles/common.rs"]
mod common;

/// A document whose one paragraph is timed 0 to 1 hour (3,600,000 ms, inside
/// `MAX_MS`), so the input is that paragraph's content.
const HEAD: &[u8] = br#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="0s" end="1h">"#;
const TAIL: &[u8] = b"</p></div></body></tt>";

/// A paragraph with `begin`, `end` and `dur` in milliseconds, taken from the
/// first six bytes of the input as little-endian `u16` (at most 65 s each, so
/// the three values meet within a few seconds of each other). The model is the
/// TTML2 rule (12.2.2): the interval is `begin` to the earlier of `end` and
/// `begin + dur`, and a cue exists only when that end is later than `begin`
/// (an earlier end is `ends before it starts`, an equal one is `text with no
/// display time`). The reader's own helpers are not used here: the expected
/// values are written out again.
fn check_paragraph_timing(data: &[u8]) {
    let [b0, b1, e0, e1, d0, d1, ..] = data else {
        return;
    };
    let num = |lo: &u8, hi: &u8| u64::from(u16::from_le_bytes([*lo, *hi]));
    let (begin, end, dur) = (num(b0, b1), num(e0, e1), num(d0, d1));
    let doc = format!(
        r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="{begin}ms" end="{end}ms" dur="{dur}ms">x</p></div></body></tt>"#
    );
    let want_end = end.min(begin + dur);
    match subtitles::read_ttml(doc.as_bytes()) {
        Ok(parsed) if want_end > begin => {
            let spans: Vec<(u64, u64)> =
                parsed.cues.iter().map(|c| (c.start_ms, c.end_ms)).collect();
            assert_eq!(spans, [(begin, want_end)], "timing of {doc}");
            assert_eq!(parsed.skipped.count, 0, "a skip in {doc}");
        }
        Err(ReadError::NoCues) if want_end <= begin => {}
        other => panic!("{doc}: want [{begin}, {want_end}) or no cue, got {other:?}"),
    }
}

fuzz_target!(|data: &[u8]| {
    common::check(data, subtitles::read_ttml);
    common::check(&[HEAD, data, TAIL].concat(), subtitles::read_ttml);
    check_paragraph_timing(data);
});
