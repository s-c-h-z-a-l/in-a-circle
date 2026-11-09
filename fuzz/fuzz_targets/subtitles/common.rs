//! The checks the three subtitle reader targets (`srt`, `vtt`, `ass`) share.
//! Each target hands its reader the same bytes and calls [`check`]. On every
//! verdict the reader must keep its promises:
//!
//! - the same bytes give the same verdict;
//! - `Ok` has cues, each with `start <= end <= MAX_MS` and some non-empty
//!   text, the skipped count and its first line agree, and every loss is
//!   counted at least once;
//! - `NotUtf8` names the first bad byte of the input, `TooLarge` comes only
//!   past `INPUT_BYTES`; `TooComplex` and `NoCues` are valid verdicts.
//!
//! The cues, sorted as the caller sorts them, are then written as SRT and as
//! WebVTT and read back by the matching reader. This is own consistency, not
//! independent evidence: the same times, top flag and text per line must come
//! back, and the output must have no block the reader skips. SRT writes a
//! speaker as a `Name: ` prefix, so its text includes the prefix; WebVTT writes
//! it as a tag, which the reader sets as the voice, so the text does not.

use subtitles::{
    Cue, INPUT_BYTES, MAX_MS, Parsed, ReadError, read_srt, read_vtt, sort_cues, write_srt,
    write_vtt,
};

/// The part of a cue a round trip compares: start, end, top flag, and the
/// text of each line that has any. The text is trimmed because the readers
/// trim every line (a line of spaces is a blank line), so the spaces at a
/// line's ends do not come back; a line of spaces is no text on either side.
type Shape = Vec<(u64, u64, bool, Vec<String>)>;

fn shape(cues: &[Cue], voice_prefix: bool) -> Shape {
    cues.iter()
        .filter_map(|cue| {
            let lines: Vec<String> = cue
                .lines
                .iter()
                .filter_map(|line| {
                    let mut text = String::new();
                    if let (true, Some(name)) = (voice_prefix, &line.voice) {
                        text.push_str(name);
                        text.push_str(": ");
                    }
                    for run in &line.runs {
                        text.push_str(&run.text);
                    }
                    let trimmed = text.trim();
                    (!trimmed.is_empty()).then(|| trimmed.to_owned())
                })
                .collect();
            (!lines.is_empty()).then_some((cue.start_ms, cue.end_ms, cue.top, lines))
        })
        .collect()
}

/// Checks the reader `read` on `data`, then the round trip of its cues.
pub fn check(data: &[u8], read: fn(&[u8]) -> Result<Parsed, ReadError>) {
    let verdict = read(data);
    assert_eq!(verdict, read(data), "the same bytes gave another verdict");
    match verdict {
        Ok(mut parsed) => {
            assert_usable(&parsed);
            sort_cues(&mut parsed.cues);
            assert!(
                parsed
                    .cues
                    .windows(2)
                    .all(|w| w[0].start_ms <= w[1].start_ms),
                "sorted cues out of order"
            );
            round_trip(&parsed.cues);
        }
        Err(ReadError::NotUtf8 { at }) => assert_eq!(
            std::str::from_utf8(data).err().map(|e| e.valid_up_to()),
            Some(at),
            "NotUtf8 names a byte that is not the first bad one"
        ),
        Err(ReadError::TooLarge) => assert!(data.len() > INPUT_BYTES, "TooLarge under the limit"),
        // A valid verdict. TooComplex needs more than ITEM_BUDGET items, which
        // a fuzz input (at most 1 MiB, about one item per byte) cannot reach;
        // the budget boundary is pinned by the unit test in crates/subtitles.
        Err(ReadError::TooComplex | ReadError::NoCues) => {}
    }
}

fn assert_usable(parsed: &Parsed) {
    assert!(!parsed.cues.is_empty(), "an Ok verdict with no cues");
    for cue in &parsed.cues {
        assert!(
            cue.start_ms <= cue.end_ms && cue.end_ms <= MAX_MS,
            "bad span: {cue:?}"
        );
        assert!(
            cue.lines
                .iter()
                .flat_map(|line| &line.runs)
                .any(|run| !run.text.is_empty()),
            "a cue with no text: {cue:?}"
        );
    }
    assert_eq!(
        parsed.skipped.count == 0,
        parsed.skipped.first.is_none(),
        "skipped count and first skip disagree"
    );
    if let Some((line, reason)) = parsed.skipped.first {
        assert!(
            line >= 1 && !reason.is_empty(),
            "bad first skip: {line} {reason:?}"
        );
    }
    assert!(
        parsed.losses.values().all(|&n| n >= 1),
        "a loss counted zero times"
    );
}

/// Writes the sorted cues as SRT and WebVTT and reads each back. The writers
/// filter out cues with no text, so the expected shape is taken the same way.
fn round_trip(cues: &[Cue]) {
    let mut srt = Vec::new();
    write_srt(cues, &mut srt).expect("writing to memory");
    let back = read_srt(&srt).unwrap_or_else(|e| panic!("SRT output does not read back: {e}"));
    assert_eq!(
        back.skipped.count, 0,
        "SRT output has a block the reader skips"
    );
    let mut back = back.cues;
    sort_cues(&mut back);
    assert_eq!(shape(&back, true), shape(cues, true), "SRT round trip");

    let mut vtt = Vec::new();
    write_vtt(cues, &mut vtt).expect("writing to memory");
    let back = read_vtt(&vtt).unwrap_or_else(|e| panic!("WebVTT output does not read back: {e}"));
    assert_eq!(
        back.skipped.count, 0,
        "WebVTT output has a block the reader skips"
    );
    let mut back = back.cues;
    sort_cues(&mut back);
    assert_eq!(shape(&back, false), shape(cues, false), "WebVTT round trip");
}
