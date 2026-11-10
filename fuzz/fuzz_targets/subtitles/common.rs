//! The checks the three subtitle reader targets (`srt`, `vtt`, `ass`) share.
//! Each target hands its reader the same bytes and calls [`check`]. On every
//! verdict the reader must keep its promises:
//!
//! - the same bytes give the same verdict;
//! - `Ok` has cues, each with `start <= end <= MAX_MS` and some text that is
//!   not white space, the skipped count and its first line agree, and every loss is
//!   counted at least once;
//! - `NotUtf8` names the first bad byte of the input, `TooLarge` comes only
//!   past `INPUT_BYTES`; `TooComplex` and `NoCues` are valid verdicts.
//!
//! The cues, sorted as the caller sorts them, are then written as SRT and as
//! WebVTT and read back by the matching reader. This is own consistency, not
//! independent evidence: the same times and top flag come back, and per line
//! the same characters with the same italic, bold and underline; WebVTT also
//! gives the same voice. The output must have no block the reader skips.
//! SRT writes a speaker as a `Name: ` prefix, so its text includes the prefix;
//! WebVTT writes it as a tag, which the reader sets as the voice.
//!
//! The SRT writer makes two rewrites, both judged by the reader's own rules,
//! and the oracle expects exactly those. A `<` the reader would take as a tag,
//! or a `{` it would take as an override block, becomes a look-alike (`tag.rs`,
//! included from the crate, so there is one source). The `>` of each `-->` in
//! a line the reader would take for a timing line becomes U+FF1E; the timing
//! test (`is_timing`) is copied here, and a copy that disagrees fails the
//! round trip. Any other arrow is text and must come back as it was.

// The SRT reader's tag rule, the same file the crate builds with.
#[path = "../../../crates/subtitles/src/tag.rs"]
mod tag;

use subtitles::{
    Cue, INPUT_BYTES, Line, MAX_MS, Parsed, ReadError, read_srt, read_vtt, sort_cues, write_srt,
    write_vtt,
};

/// One character of a line with its styles: the character, italic, bold and
/// underline. Styles are compared per character, because the writers may split
/// or merge runs.
type Styled = (char, bool, bool, bool);

/// The part of a cue a round trip compares: start, end, top flag, and each line
/// that has text, with its WebVTT voice (`None` for SRT, whose speaker is in the
/// text) and its characters, trimmed. The readers trim every line (a line of
/// spaces is a blank line), so the spaces at a line's ends do not come back.
type Shape = Vec<(u64, u64, bool, Vec<(Option<String>, Vec<Styled>)>)>;

/// Whether a line has text, as the writers judge it: white space is no text.
fn has_text(line: &Line) -> bool {
    line.runs
        .iter()
        .any(|r| r.text.chars().any(|c| !c.is_whitespace()))
}

/// The characters of `line` with their styles. A NUL is U+FFFD, which is what
/// reads back (both writers replace it). In SRT the speaker is a `Name: `
/// prefix in the text, unstyled.
fn styled(line: &Line, srt: bool) -> Vec<Styled> {
    let fix = |c: char| if c == '\0' { '\u{fffd}' } else { c };
    let mut chars = Vec::new();
    if let (true, Some(name)) = (srt, line.voice.as_deref()) {
        chars.extend(
            name.chars()
                .chain(": ".chars())
                .map(|c| (fix(c), false, false, false)),
        );
    }
    for run in &line.runs {
        chars.extend(
            run.text
                .chars()
                .map(|c| (fix(c), run.italic, run.bold, run.underline)),
        );
    }
    chars
}

/// The SRT writer's look-alikes (`srt_safe`): the ones `tag::look_alikes` picks
/// from the line's plain text, the speaker's prefix included, where the reader
/// would take a `<` as a tag or a `{` as an override block. The reader's round
/// trip is what checks them; the rule itself is the crate's, not a copy.
fn look_alikes(chars: &mut [Styled]) {
    let text: String = chars.iter().map(|c| c.0).collect();
    let mut looks = tag::look_alikes(&text).into_iter().peekable();
    for (c, (at, _)) in chars.iter_mut().zip(text.char_indices()) {
        if let Some(&(look_at, look)) = looks.peek() {
            if look_at == at {
                c.0 = look;
                looks.next();
            }
        }
    }
}

/// The SRT reader's timing test (`srt::is_timing_line`), copied. The reader
/// reads the start first: a start that is not a time fails the line, and a
/// start past the limit already counts as a timing line (its cue is skipped).
/// Otherwise the end token must read as a time too.
fn is_timing(line: &str) -> bool {
    line.split_once("-->")
        .is_some_and(|(start, end)| match time_ms(start.trim()) {
            None => false,
            Some(start) if start > u128::from(MAX_MS) => true,
            Some(_) => time_ms(end.split_whitespace().next().unwrap_or_default()).is_some(),
        })
}

/// `H+:MM:SS,mmm` (`.` also accepted before the milliseconds) in milliseconds,
/// `None` for any other shape: hours of any length, minutes and seconds of one
/// or two digits, milliseconds of any length. A value past `u128` saturates,
/// which is past the limit either way.
fn time_ms(s: &str) -> Option<u128> {
    let mut fields = s.split(':');
    let (Some(h), Some(m), Some(rest), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return None;
    };
    let (sec, ms) = rest.split_once([',', '.'])?;
    let (hours, minutes, seconds, millis) = (
        digits(h, usize::MAX)?,
        digits(m, 2)?,
        digits(sec, 2)?,
        digits(ms, usize::MAX)?,
    );
    // 3,600,000 ms an hour, 60,000 a minute, 1,000 a second.
    Some(
        hours
            .saturating_mul(3_600_000)
            .saturating_add(minutes * 60_000)
            .saturating_add(seconds * 1_000)
            .saturating_add(millis),
    )
}

/// The value of a non-empty run of at most `max_len` ASCII digits.
fn digits(s: &str, max_len: usize) -> Option<u128> {
    (!s.is_empty() && s.len() <= max_len && s.bytes().all(|b| b.is_ascii_digit())).then(|| {
        s.bytes().fold(0u128, |n, b| {
            n.saturating_mul(10).saturating_add(u128::from(b - b'0'))
        })
    })
}

/// The characters an SRT line must read back as, from the writer's side. The
/// writer renders the line first (`write_srt_line`): the `{\an8}` marker of a
/// top cue's first line, the speaker, then each run with its tags. It rewrites
/// the `>` of each `-->` of the rendered text when the reader would take that
/// text for a timing line. An arrow with a tag between two of its characters is
/// not an arrow in the rendered text, so it is not rewritten.
fn srt_expected(line: &Line, marker: bool) -> Vec<Styled> {
    let mut plain = styled(line, true);
    look_alikes(&mut plain);
    // The rendered line: each character with its index in `plain`, or None
    // for markup.
    let mut rendered: Vec<(char, Option<usize>)> = Vec::new();
    if marker {
        rendered.extend("{\\an8}".chars().map(|c| (c, None)));
    }
    // (opening tags, characters of text, closing tags) in the writer's order.
    let mut segments: Vec<(String, usize, String)> = Vec::new();
    if let Some(name) = line.voice.as_deref() {
        segments.push((
            String::new(),
            name.chars().count() + ": ".len(),
            String::new(),
        ));
    }
    for run in line.runs.iter().filter(|r| !r.text.is_empty()) {
        let (mut open, mut close) = (String::new(), String::new());
        if run.italic {
            open.push_str("<i>");
            close.insert_str(0, "</i>");
        }
        if run.bold {
            open.push_str("<b>");
            close.insert_str(0, "</b>");
        }
        if run.underline {
            open.push_str("<u>");
            close.insert_str(0, "</u>");
        }
        segments.push((open, run.text.chars().count(), close));
    }
    let mut k = 0;
    for (open, n, close) in &segments {
        rendered.extend(open.chars().map(|c| (c, None)));
        for _ in 0..*n {
            rendered.push((plain[k].0, Some(k)));
            k += 1;
        }
        rendered.extend(close.chars().map(|c| (c, None)));
    }
    let text: String = rendered.iter().map(|c| c.0).collect();
    if is_timing(text.trim()) {
        let mut i = 0;
        while i + 3 <= rendered.len() {
            if rendered[i..i + 3].iter().map(|c| c.0).eq("-->".chars()) {
                if let Some(k) = rendered[i + 2].1 {
                    plain[k].0 = '\u{ff1e}';
                }
                i += 3;
            } else {
                i += 1;
            }
        }
    }
    plain
}

/// `chars` without white space at either end.
fn trimmed(chars: &[Styled]) -> &[Styled] {
    let start = chars
        .iter()
        .position(|c| !c.0.is_whitespace())
        .unwrap_or(chars.len());
    let end = chars
        .iter()
        .rposition(|c| !c.0.is_whitespace())
        .map_or(start, |i| i + 1);
    &chars[start..end]
}

/// The shape of `cues`: as the writer gives them (`original`, the SRT rules of
/// [`srt_expected`]), or as the reader reads them back.
fn shape(cues: &[Cue], srt: bool, original: bool) -> Shape {
    cues.iter()
        .filter_map(|cue| {
            let lines: Vec<(Option<String>, Vec<Styled>)> = cue
                .lines
                .iter()
                .filter(|line| has_text(line))
                .enumerate()
                .filter_map(|(i, line)| {
                    let (voice, chars) = match (srt, original) {
                        (true, true) => (None, srt_expected(line, cue.top && i == 0)),
                        (true, false) => (None, styled(line, true)),
                        (false, _) => (
                            line.voice.as_deref().map(|n| n.replace('\0', "\u{fffd}")),
                            styled(line, false),
                        ),
                    };
                    let chars = trimmed(&chars);
                    (!chars.is_empty()).then(|| (voice, chars.to_vec()))
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
        // A malformed document names a line that exists.
        Err(ReadError::Malformed { line, .. }) => assert!(line >= 1, "Malformed names line 0"),
        // A valid verdict. TooComplex needs more than ITEM_BUDGET items, which
        // a fuzz input (at most 1 MiB, about one item per byte) cannot reach;
        // the budget boundary is pinned by the unit test in crates/subtitles.
        Err(ReadError::TooComplex | ReadError::NoCues | ReadError::NotTtml) => {}
        Err(ReadError::Unsupported(_)) => {}
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
                .any(|run| run.text.chars().any(|c| !c.is_whitespace())),
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
/// filter out cues with no text (white space is no text), so the expected
/// shape is taken the same way; when every cue is such a cue, the output has
/// no cue and reads back as `NoCues`.
fn round_trip(cues: &[Cue]) {
    let mut srt = Vec::new();
    write_srt(cues, &mut srt).expect("writing to memory");
    let want = shape(cues, true, true);
    if want.is_empty() {
        assert_eq!(read_srt(&srt), Err(ReadError::NoCues), "empty SRT output");
    } else {
        let back = read_srt(&srt).unwrap_or_else(|e| panic!("SRT output does not read back: {e}"));
        assert_eq!(
            back.skipped.count, 0,
            "SRT output has a block the reader skips"
        );
        let mut back = back.cues;
        sort_cues(&mut back);
        assert_eq!(shape(&back, true, false), want, "SRT round trip");
    }

    let mut vtt = Vec::new();
    write_vtt(cues, &mut vtt).expect("writing to memory");
    let want = shape(cues, false, true);
    if want.is_empty() {
        assert_eq!(
            read_vtt(&vtt),
            Err(ReadError::NoCues),
            "empty WebVTT output"
        );
    } else {
        let back =
            read_vtt(&vtt).unwrap_or_else(|e| panic!("WebVTT output does not read back: {e}"));
        assert_eq!(
            back.skipped.count, 0,
            "WebVTT output has a block the reader skips"
        );
        let mut back = back.cues;
        sort_cues(&mut back);
        assert_eq!(shape(&back, false, false), want, "WebVTT round trip");
    }
}
