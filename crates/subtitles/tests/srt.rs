//! SRT reader: the forgiving rules, the limits, the linear-time guard, and a
//! differential check against ffmpeg on the files in `tests/data/srt`.
//!
//! Deliberate differences from ffmpeg. The differential test skips the files
//! in `DIVERGENT_FILES`; each difference below is pinned by the named test.
//! - A cue that ends before it starts: ffmpeg moves its end to the next cue's
//!   start and keeps it. We skip it (`end_before_start_is_skipped`).
//! - A cue at 100 hours or more: ffmpeg writes a three-digit hour and keeps
//!   it. We skip it (`hour_100_is_skipped`).
//! - A `<` that is not a tag (`a < b`): ffmpeg reads it as a tag and rewrites
//!   it. We keep the text (`literal_less_than_is_text`).
//! - A `<` whose name does not start with a letter (`<3>`, `<>`, `</>`): ffmpeg
//!   drops the tag. We keep the text, since text that may be the file's own is
//!   not dropped on a guess (`angle_text_without_a_leading_letter_is_kept`).
//! - A broken timing line after a blank line: ffmpeg merges its block into the
//!   previous cue. We skip the block (`broken_timing_skips_its_text_only`).
//! - A block of text that is neither a number nor a timing (stray text, a
//!   wrong index, a title): ffmpeg appends it to the previous cue. We count it
//!   as one skipped item and show nothing, since text that belongs to no cue
//!   would show at the wrong time (`stray_text_between_cues_is_skipped_and_counted`).
//! - `<font>` is kept by ffmpeg and dropped by us. The comparison strips the
//!   style tags ffmpeg writes (`<i>`, `<b>`, `<u>`, `<font …>`, `{\an…}`) from
//!   its output, so this does not show in it. Every other `<…>` and `{…}` is
//!   compared as text on both sides.

use std::collections::BTreeMap;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use subtitles::{Cue, INPUT_BYTES, Loss, MAX_MS, Parsed, ReadError, Skipped, read_srt, sort_cues};

/// Data files the ffmpeg comparison leaves out, with the reason.
const DIVERGENT_FILES: &[(&str, &str)] = &[(
    "wild.srt",
    "hand-written edge cases; the differences above are pinned by their named tests",
)];

/// Set to `1` on a runner that must have ffmpeg: a missing tool then fails
/// the test instead of skipping the comparison.
const REQUIRE_TOOLS: &str = "INACIRCLE_REQUIRE_SUBTITLE_TOOLS";

fn read(text: &str) -> Parsed {
    read_srt(text.as_bytes()).expect("the input has a usable cue")
}

/// A cue's text: its runs joined, its lines separated by a newline.
fn text_of(cue: &Cue) -> String {
    cue.lines
        .iter()
        .map(|line| {
            line.runs
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn texts(parsed: &Parsed) -> Vec<String> {
    parsed.cues.iter().map(text_of).collect()
}

fn spans(parsed: &Parsed) -> Vec<(u64, u64)> {
    parsed.cues.iter().map(|c| (c.start_ms, c.end_ms)).collect()
}

fn losses(pairs: &[(Loss, usize)]) -> BTreeMap<Loss, usize> {
    pairs.iter().copied().collect()
}

#[test]
fn bad_cue_in_the_middle_is_skipped_rest_converts() {
    // 100 cues of four lines each: number, timing, text, blank. Cue 40 uses
    // `;` for the comma, so its timing line is line 40 * 4 - 2 = 158.
    let mut file = String::new();
    for i in 1..=100 {
        let sep = if i == 40 { ';' } else { ',' };
        file += &format!(
            "{i}\n00:{:02}:{:02}{sep}000 --> 00:{:02}:{:02},500\ncue {i}\n\n",
            i / 60,
            i % 60,
            i / 60,
            i % 60,
        );
    }
    let parsed = read(&file);
    assert_eq!(parsed.cues.len(), 99);
    let expected: Vec<String> = (1..=100)
        .filter(|&i| i != 40)
        .map(|i| format!("cue {i}"))
        .collect();
    assert_eq!(texts(&parsed), expected);
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 1,
            first: Some((158, "bad timestamp")),
        }
    );
}

#[test]
fn missing_blank_line_between_cues() {
    let parsed = read(
        "1\n00:00:01,000 --> 00:00:02,000\nfirst\n2\n00:00:03,000 --> 00:00:04,000\nsecond\n3\n00:00:05,000 --> 00:00:06,000\nthird\n",
    );
    assert_eq!(texts(&parsed), vec!["first", "second", "third"]);
    assert_eq!(parsed.skipped, Skipped::default());
}

#[test]
fn bare_timing_line_without_blank_line_starts_cue() {
    // Players start a cue at a valid timing line even with no number before
    // it and no blank line; ffmpeg does the same.
    let parsed =
        read("00:00:01,000 --> 00:00:02,000\nfirst\n00:00:03,000 --> 00:00:04,000\nsecond\n");
    assert_eq!(texts(&parsed), vec!["first", "second"]);
}

#[test]
fn arrow_and_broken_timing_in_text_are_text() {
    // `a --> b` and a timing with no milliseconds are not timing lines, so
    // inside a cue they stay text.
    let parsed =
        read("1\n00:00:01,000 --> 00:00:02,000\na --> b\n00:00:03 --> 00:00:04\nstill text\n");
    assert_eq!(
        texts(&parsed),
        vec!["a --> b\n00:00:03 --> 00:00:04\nstill text"]
    );
    assert_eq!(parsed.skipped, Skipped::default());
}

#[test]
fn wrong_or_missing_index_is_fine() {
    let body = "00:00:01,000 --> 00:00:02,000\nfirst\n\n00:00:03,000 --> 00:00:04,000\nsecond\n";
    let plain = read(body);
    for index in ["9\n", "1\n"] {
        let other = read(&format!("{index}{body}"));
        assert_eq!(other.cues, plain.cues, "index {index:?}");
        assert_eq!(other.skipped, Skipped::default(), "index {index:?}");
    }
    // A text line where the number goes is a stray block: the cues are the
    // same, and the block is counted once.
    let text = read(&format!("x\n{body}"));
    assert_eq!(text.cues, plain.cues);
    assert_eq!(
        text.skipped,
        Skipped {
            count: 1,
            first: Some((1, "text outside any cue")),
        }
    );
    let numbers_out_of_order = read(
        "9\n00:00:01,000 --> 00:00:02,000\nfirst\n\n7\n00:00:03,000 --> 00:00:04,000\nsecond\n",
    );
    assert_eq!(numbers_out_of_order.cues, plain.cues);
}

#[test]
fn dot_instead_of_comma_and_short_fields() {
    // 1:02:03.004 is 3 723 004 ms; 1:02:05.007 is 3 725 007 ms.
    let parsed = read("1:2:3.4 --> 1:2:5.7\nfirst\n\n00:00:01.000 --> 00:00:02.5\nsecond\n");
    assert_eq!(spans(&parsed), vec![(3_723_004, 3_725_007), (1_000, 2_005)]);
}

#[test]
fn short_milliseconds_are_integers() {
    // `,5` is 5 ms and `,50` is 50 ms, as ffmpeg reads them; `,1234` is 1234
    // ms, not cut to three digits.
    let parsed = read(
        "00:00:01,5 --> 00:00:01,50\na\n\n00:00:02,500 --> 00:00:03,1234\nb\n\n00:00:00,05 --> 00:00:00,50\nc\n",
    );
    assert_eq!(
        spans(&parsed),
        vec![(1_005, 1_050), (2_500, 4_234), (5, 50)]
    );
}

#[test]
fn stray_text_between_cues_is_skipped_and_counted() {
    // Neither a number nor a timing line: it belongs to no cue. It is not
    // added to the first one, and it is counted, not dropped silently.
    let parsed = read(
        "1\n00:00:01,000 --> 00:00:02,000\nfirst\n\nx\n00:00:03,000 --> 00:00:04,000\nsecond\n",
    );
    assert_eq!(texts(&parsed), vec!["first", "second"]);
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 1,
            first: Some((5, "text outside any cue")),
        }
    );
}

#[test]
fn two_stray_blocks_count_twice() {
    // Block one is two lines after a blank line; block two runs to the end of
    // the file. A multi-line block is one item, not one per line.
    let parsed = read(
        "1\n00:00:01,000 --> 00:00:02,000\nfirst\n\nstray one\nstray one more\n\n2\n00:00:03,000 --> 00:00:04,000\nsecond\n\nstray two\n",
    );
    assert_eq!(texts(&parsed), vec!["first", "second"]);
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 2,
            first: Some((5, "text outside any cue")),
        }
    );
}

#[test]
fn leading_junk_before_first_cue_is_counted() {
    let parsed = read("Title line\n\n1\n00:00:01,000 --> 00:00:02,000\nfirst\n");
    assert_eq!(texts(&parsed), vec!["first"]);
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 1,
            first: Some((1, "text outside any cue")),
        }
    );
}

#[test]
fn lone_index_number_is_silent() {
    // A number with no timing after it is a cue index, not text: nothing is
    // counted, whether it is followed by a blank line or the end of the file.
    let parsed = read(
        "1\n00:00:01,000 --> 00:00:02,000\nfirst\n\n2\n00:00:03,000 --> 00:00:04,000\nsecond\n\n3\n",
    );
    assert_eq!(texts(&parsed), vec!["first", "second"]);
    assert_eq!(parsed.skipped, Skipped::default());
}

#[test]
fn only_stray_text_is_no_cues() {
    // Every block is stray and no cue remains: the file has no subtitles.
    assert_eq!(read_srt(b"Title only\n\nstray\n"), Err(ReadError::NoCues));
}

#[test]
fn bom_and_crlf_and_cr() {
    let base = "1\n00:00:01,000 --> 00:00:02,000\nfirst line\nsecond line\n\n2\n00:00:03,000 --> 00:00:04,000\nsecond\n";
    let lf = read(base);
    let variants = [
        base.replace('\n', "\r\n"),
        base.replace('\n', "\r"),
        base.replacen('\n', "\r\n", 1),
        format!("\u{feff}{base}"),
        format!("\u{feff}{}", base.replace('\n', "\r\n")),
    ];
    for variant in &variants {
        assert_eq!(read(variant), lf, "variant {variant:?}");
    }
    // The byte order mark matters only when it sits on a timing line: on a
    // number line it is ignored either way, so the file above cannot show
    // whether it was skipped.
    let timing_first = read("\u{feff}00:00:01,000 --> 00:00:02,000\nfirst\n");
    assert_eq!(spans(&timing_first), vec![(1_000, 2_000)]);
    assert_eq!(timing_first.skipped, Skipped::default());
}

#[test]
fn font_tag_keeps_text_and_reports_styling() {
    let parsed = read(
        "1\n00:00:01,000 --> 00:00:02,000\n<font color=\"red\">Red</font> plain\n\n2\n00:00:03,000 --> 00:00:04,000\n<font size=5>big</font>\n",
    );
    assert_eq!(texts(&parsed), vec!["Red plain", "big"]);
    assert_eq!(parsed.losses, losses(&[(Loss::Styling, 2)]));
}

#[test]
fn an8_sets_top_others_dropped() {
    let parsed = read(
        "1\n00:00:01,000 --> 00:00:02,000\n{\\an8}top\n\n2\n00:00:03,000 --> 00:00:04,000\n{\\an2}bottom\n\n3\n00:00:05,000 --> 00:00:06,000\n{\\an5}middle\n\n4\n00:00:07,000 --> 00:00:08,000\n{\\an7}left top\n\n5\n00:00:09,000 --> 00:00:10,000\n{\\an9}right top\n",
    );
    let tops: Vec<bool> = parsed.cues.iter().map(|c| c.top).collect();
    assert_eq!(tops, vec![true, false, false, true, true]);
    assert_eq!(texts(&parsed).len(), 5);
    assert!(parsed.losses.is_empty());
}

#[test]
fn end_before_start_is_skipped() {
    let parsed = read(
        "1\n00:00:05,000 --> 00:00:04,000\nbackwards\n\n2\n00:00:06,000 --> 00:00:06,000\nzero length\n",
    );
    // Equal start and end is a zero-length cue, which is usable.
    assert_eq!(spans(&parsed), vec![(6_000, 6_000)]);
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 1,
            first: Some((2, "ends before it starts")),
        }
    );
}

#[test]
fn hour_100_is_skipped() {
    let parsed = read(
        "1\n99:59:59,999 --> 99:59:59,999\nlast\n\n2\n100:00:00,000 --> 100:00:01,000\nlate\n\n3\n00:00:01,000 --> 100:00:00,000\nlate end\n\n4\n1000000000000000000000000000:00:00,000 --> 00:00:01,000\nsaturated\n\n5\n00:00:02,000 --> 00:00:03,000\nfine\n",
    );
    // The last millisecond before the limit is usable; the rest are not, and
    // an absurdly long hour field is refused rather than overflowing.
    assert_eq!(spans(&parsed), vec![(MAX_MS, MAX_MS), (2_000, 3_000)]);
    assert_eq!(texts(&parsed), vec!["last", "fine"]);
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 3,
            first: Some((6, "time is 100 hours or more")),
        }
    );
}

#[test]
fn empty_file_is_no_cues() {
    assert_eq!(read_srt(b""), Err(ReadError::NoCues));
    assert_eq!(
        read_srt("\u{feff}\r\n \r\n".as_bytes()),
        Err(ReadError::NoCues)
    );
    // Nothing usable is the same answer as nothing at all.
    assert_eq!(
        read_srt(b"1\n00:00:05,000 --> 00:00:04,000\nbackwards\n"),
        Err(ReadError::NoCues)
    );
}

#[test]
fn only_tags_cue_is_dropped_not_skipped() {
    let parsed = read(
        "1\n00:00:01,000 --> 00:00:02,000\n<i></i>\n\n2\n00:00:03,000 --> 00:00:04,000\nreal\n",
    );
    assert_eq!(texts(&parsed), vec!["real"]);
    assert_eq!(parsed.skipped, Skipped::default());
}

#[test]
fn broken_timing_skips_its_text_only() {
    let parsed = read(
        "1\n00:00:01 --> 00:00:02\nlost\nstill lost\n\n2\n00:00:03,000 --> 00:00:04,000\nkept\n",
    );
    assert_eq!(texts(&parsed), vec!["kept"]);
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 1,
            first: Some((2, "bad timestamp")),
        }
    );
}

#[test]
fn not_utf8_names_the_byte() {
    let body = b"1\n00:00:01,000 --> 00:00:02,000\nab\xFFcd\n";
    let at = body.iter().position(|&b| b == 0xFF).expect("the bad byte");
    assert_eq!(read_srt(body), Err(ReadError::NotUtf8 { at }));

    // A byte order mark counts toward the offset: it is part of the file.
    let with_bom = [b"\xEF\xBB\xBF".as_slice(), body].concat();
    assert_eq!(read_srt(&with_bom), Err(ReadError::NotUtf8 { at: at + 3 }));

    // A multi-byte character cut off at the end is reported at its first byte.
    let cut = b"1\n00:00:01,000 --> 00:00:02,000\nx\xC3";
    assert_eq!(read_srt(cut), Err(ReadError::NotUtf8 { at: cut.len() - 1 }));
}

#[test]
fn overlapping_cues_sorted_stably() {
    let mut parsed = read(
        "1\n00:00:05,000 --> 00:00:09,000\nA\n\n2\n00:00:02,000 --> 00:00:03,000\nB\n\n3\n00:00:05,000 --> 00:00:06,000\nC\n\n4\n00:00:02,000 --> 00:00:04,000\nD\n",
    );
    assert_eq!(texts(&parsed), vec!["A", "B", "C", "D"]);
    sort_cues(&mut parsed.cues);
    // Cues that start together keep their file order: B before D, A before C.
    assert_eq!(texts(&parsed), vec!["B", "D", "A", "C"]);
}

#[test]
fn input_limit_is_inclusive() {
    let head = b"1\n00:00:01,000 --> 00:00:02,000\nx\n\n";
    let mut exact = head.to_vec();
    exact.resize(INPUT_BYTES, b'\n');
    assert!(read_srt(&exact).is_ok());
    exact.push(b'\n');
    assert_eq!(read_srt(&exact), Err(ReadError::TooLarge));
}

#[test]
fn styling_persists_across_lines_of_a_cue() {
    let parsed = read("1\n00:00:01,000 --> 00:00:02,000\n<i>one\ntwo</i> three\n");
    let lines = &parsed.cues[0].lines;
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1].runs.len(), 2);
    assert!(lines[1].runs[0].italic && lines[1].runs[0].text == "two");
    assert!(!lines[1].runs[1].italic && lines[1].runs[1].text == " three");
}

#[test]
fn ass_block_switches_style_and_zero_closes_it() {
    let parsed = read("1\n00:00:01,000 --> 00:00:02,000\n{\\i1}a{\\i0}b\n");
    let runs = &parsed.cues[0].lines[0].runs;
    assert_eq!(runs.len(), 2);
    assert!(runs[0].italic && runs[0].text == "a");
    assert!(!runs[1].italic && runs[1].text == "b");
}

#[test]
fn number_line_without_timing_after_it_is_text() {
    let parsed = read("1\n00:00:01,000 --> 00:00:02,000\nfirst\n2\nsecond line\n");
    assert_eq!(texts(&parsed), vec!["first\n2\nsecond line"]);
}

#[test]
fn literal_less_than_is_text() {
    let parsed = read("1\n00:00:01,000 --> 00:00:02,000\na < b > c\n<unclosed text\n");
    assert_eq!(texts(&parsed), vec!["a < b > c\n<unclosed text"]);
}

#[test]
fn unknown_tags_drop_but_keep_their_text() {
    let parsed = read("1\n00:00:01,000 --> 00:00:02,000\n<c>tag</c> <v Anna>hi</v>\n");
    assert_eq!(texts(&parsed), vec!["tag hi"]);
    assert!(parsed.losses.is_empty());
}

#[test]
fn angle_text_that_is_not_a_tag_is_kept() {
    // A `<` opens a tag only when its name (up to a space or `>`) holds letters,
    // digits, `_` or `/`. A URL, an address or a dotted name is text, as in
    // ffmpeg: nothing is dropped and nothing is reported.
    let parsed = read(
        "1\n00:00:01,000 --> 00:00:02,000\nSync by <www.addic7ed.com>\n\n2\n00:00:03,000 --> 00:00:04,000\nmail <name@mail.com> <a-b> <c.yellow>x</c>\n",
    );
    assert_eq!(
        texts(&parsed),
        vec![
            "Sync by <www.addic7ed.com>",
            "mail <name@mail.com> <a-b> <c.yellow>x",
        ]
    );
    assert!(parsed.losses.is_empty());
}

#[test]
fn angle_text_without_a_leading_letter_is_kept() {
    // ffmpeg drops `<3>`, `<>` and `</>`. They are kept: a tag needs a letter
    // first, so these are text (the divergence list above says why).
    let parsed = read("1\n00:00:01,000 --> 00:00:02,000\nI <3 you <3> and <> and </>\n");
    assert_eq!(texts(&parsed), vec!["I <3 you <3> and <> and </>"]);
}

#[test]
fn a_less_than_before_a_closer_is_text() {
    // The first `<` meets another `<` before its `>`, so it is text; the `<i>`
    // after it is a tag. ffmpeg reads it the same way.
    let parsed = read("1\n00:00:01,000 --> 00:00:02,000\nx <a b<i>y</i>\n");
    let runs = &parsed.cues[0].lines[0].runs;
    assert_eq!(texts(&parsed), vec!["x <a by"]);
    assert!(!runs[0].italic && runs[1].italic && runs[1].text == "y");
}

#[test]
fn tag_name_is_read_whole() {
    // `<i_x>` is an unknown tag: it does not switch italic on, as `<i>` would.
    let parsed = read("1\n00:00:01,000 --> 00:00:02,000\n<i_x>a</i_x>b\n");
    assert_eq!(texts(&parsed), vec!["ab"]);
    assert!(parsed.cues[0].lines[0].runs.iter().all(|r| !r.italic));
}

#[test]
fn tag_names_with_underscore_and_slash_are_tags() {
    let parsed = read("1\n00:00:01,000 --> 00:00:02,000\n<a_b>x<c/d>y</c/d>z\n");
    assert_eq!(texts(&parsed), vec!["xyz"]);
    assert!(parsed.losses.is_empty());
}

#[test]
fn blocks_without_backslash_are_text_and_drawing_text_is_dropped() {
    let parsed = read("1\n00:00:01,000 --> 00:00:02,000\n{hello}{\\} x\n");
    assert_eq!(texts(&parsed), vec!["{hello} x"]);

    let parsed = read(
        "1\n00:00:01,000 --> 00:00:02,000\n{\\pos(1,2)}a{\\k10}b{\\p1}m 0 0 l 1{\\p0}c{\\c&HFF&}d\n",
    );
    assert_eq!(texts(&parsed), vec!["abcd"]);
    assert_eq!(
        parsed.losses,
        losses(&[
            (Loss::Styling, 1),
            (Loss::Position, 1),
            (Loss::Karaoke, 1),
            (Loss::Drawing, 1),
        ])
    );
}

#[test]
fn drawing_only_cue_reports_the_drawing_loss() {
    // A cue that is all drawing has no text left, so it is not kept. The
    // drawing was text the conversion removed, so each such cue is counted.
    let parsed = read(
        "1\n00:00:01,000 --> 00:00:02,000\n{\\p1}m 0 0 l 10 10{\\p0}\n\n\
         2\n00:00:03,000 --> 00:00:04,000\n{\\p1}m 0 0 l 1 1{\\p0}\n\n\
         3\n00:00:05,000 --> 00:00:06,000\nkept\n",
    );
    assert_eq!(texts(&parsed), vec!["kept"]);
    assert_eq!(parsed.losses, losses(&[(Loss::Drawing, 2)]));
    assert_eq!(parsed.skipped, Skipped::default());
    assert_eq!(
        read_srt(b"1\n00:00:01,000 --> 00:00:02,000\n{\\p1}m 0 0 l 1 1{\\p0}\n"),
        Err(ReadError::NoCues)
    );
}

#[test]
fn textless_cue_with_only_styling_reports_nothing() {
    // Decision: only a loss that removed text counts for a cue with no text.
    // Styling, position (the timing line's coordinates included) and karaoke
    // removed no text, so those cues are dropped silently. The third cue is
    // styled and drawn: its drawing is counted and its styling is not.
    let parsed = read(
        "1\n00:00:01,000 --> 00:00:02,000\n{\\c&HFF&}\n\n\
         2\n00:00:03,000 --> 00:00:04,000 X1:1 Y1:1\n{\\k10}\n\n\
         3\n00:00:05,000 --> 00:00:06,000\n{\\c&HFF&}{\\p1}m 0 0 l 1 1{\\p0}\n\n\
         4\n00:00:07,000 --> 00:00:08,000\nkept\n",
    );
    assert_eq!(texts(&parsed), vec!["kept"]);
    assert_eq!(parsed.losses, losses(&[(Loss::Drawing, 1)]));
}

#[test]
fn empty_cue_alone_is_silent() {
    // A cue with no text at all is not a loss: nothing is counted or skipped.
    let parsed =
        read("1\n00:00:01,000 --> 00:00:02,000\n\n2\n00:00:03,000 --> 00:00:04,000\nkept\n");
    assert_eq!(texts(&parsed), vec!["kept"]);
    assert!(parsed.losses.is_empty());
    assert_eq!(parsed.skipped, Skipped::default());
}

#[test]
fn reader_never_panics_on_every_prefix() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/srt");
    let wild = fs::read(dir.join("wild.srt")).expect("the wild file");
    assert!(read_srt(&wild).is_ok());
    for end in 0..=wild.len() {
        let _ = read_srt(&wild[..end]);
    }
    for start in 0..=wild.len() {
        let _ = read_srt(&wild[start..]);
    }
}

/// One cue whose text is `unit` repeated to at least `bytes`.
fn flood(unit: &str, bytes: usize) -> Vec<u8> {
    let count = bytes / unit.len() + 1;
    format!("0:0:0,0 --> 0:0:1,0\n{}\n", unit.repeat(count)).into_bytes()
}

/// The best of three timed runs, so one slow run does not decide the ratio.
fn best_of_three(input: &[u8]) -> Duration {
    (0..3)
        .map(|_| {
            let start = Instant::now();
            black_box(read_srt(black_box(input)).is_ok());
            start.elapsed()
        })
        .min()
        .unwrap_or_default()
}

#[test]
fn srt_linear_on_tag_floods() {
    // Each flood is read at 1 MiB and at 4 MiB, both under INPUT_BYTES. Linear
    // work makes the 4 MiB run about 4 times the 1 MiB run; a quadratic reader
    // makes it 16 times. The limit, 8 times, is halfway between on a log
    // scale, which leaves room for cache and allocator noise.
    const SMALL: usize = 1 << 20;
    const LARGE: usize = 4 << 20;
    let floods = [
        ("italic tags on one line", "x<i>"),
        ("ASS italic blocks on one line", "x{\\i1}"),
        ("tag openers never closed", "x<a"),
        // Each opener scans its name and then the text up to the next `<` or
        // `>`: those spans do not overlap, so the flood stays linear.
        (
            "long tag names, no closer",
            "<abcdefghijklmnopqrstuvwxyz0123456789_/ ",
        ),
        ("block openers never closed", "x{\\"),
        ("minimal cues", "0:0:0,0-->0:0:0,0\nx\n\n"),
    ];
    for (name, unit) in floods {
        let small = flood(unit, SMALL);
        let large = flood(unit, LARGE);
        assert!(read_srt(&large).is_ok(), "{name}: the flood is usable text");
        let t_small = best_of_three(&small);
        let t_large = best_of_three(&large);
        assert!(
            t_large < t_small * 8,
            "{name}: 4x the input took {t_large:?} against {t_small:?}; linear is about 4x"
        );
    }
}

/// True if `program` runs. A missing program is an error when `require` is
/// set, and a skipped comparison (with a note) otherwise.
fn tool_present(program: &str, require: bool) -> bool {
    let found = Command::new(program)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !found {
        assert!(!require, "{program} is missing but {REQUIRE_TOOLS}=1");
        eprintln!("skipping: {program} is not installed");
    }
    found
}

#[test]
fn missing_tool_is_skipped_when_not_required() {
    assert!(!tool_present("inacircle-no-such-tool", false));
}

#[test]
#[should_panic(expected = "is missing")]
fn missing_tool_is_an_error_when_required() {
    tool_present("inacircle-no-such-tool", true);
}

/// `H:MM:SS,mmm` in ffmpeg's output, as milliseconds.
fn clock(s: &str) -> Option<u64> {
    let (hms, ms) = s.trim().split_once(',')?;
    let parts: Vec<u64> = hms
        .split(':')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    match parts.as_slice() {
        &[h, m, sec] => Some(((h * 60 + m) * 60 + sec) * 1_000 + ms.parse::<u64>().ok()?),
        _ => None,
    }
}

/// Text with the words joined by one space: line breaks and runs of spaces do
/// not count.
fn words(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` without the spans that start with `open` and end at the next `close`.
/// A span with no closer is left as it is.
fn cut(text: &str, open: &str, close: char) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find(open) {
        let Some(j) = rest[i..].find(close) else {
            break;
        };
        out.push_str(&rest[..i]);
        rest = &rest[i + j + close.len_utf8()..];
    }
    out.push_str(rest);
    out
}

/// ffmpeg's output without the style markup it writes: `<i>`, `<b>`, `<u>` and
/// `<font …>` (with their closers) and `{\an…}`. Any other `<…>` or `{…}` is
/// the file's text and stays, so it is compared as text.
fn strip_style(text: &str) -> String {
    let mut out = text.to_owned();
    for tag in ["<i>", "</i>", "<b>", "</b>", "<u>", "</u>", "</font>"] {
        out = out.replace(tag, "");
    }
    words(&cut(&cut(&out, "<font ", '>'), "{\\an", '}'))
}

/// (start, end, top, text) for each cue with text.
type Shown = (u64, u64, bool, String);

/// Cues of ffmpeg's SRT output, read by a parser of its own: the output has
/// one fixed shape (blocks separated by blank lines, a timing line, text).
fn reference(srt: &str) -> Vec<Shown> {
    srt.split("\n\n")
        .filter_map(|block| {
            let mut lines = block.lines();
            let timing = lines.find(|l| l.contains(" --> "))?;
            let (from, to) = timing.split_once(" --> ")?;
            let raw = lines.collect::<Vec<_>>().join("\n");
            let top = ["{\\an7}", "{\\an8}", "{\\an9}"]
                .iter()
                .any(|tag| raw.contains(tag));
            Some((clock(from)?, clock(to)?, top, strip_style(&raw)))
        })
        .filter(|cue| !cue.3.is_empty())
        .collect()
}

fn ours(cues: &[Cue]) -> Vec<Shown> {
    cues.iter()
        .map(|c| (c.start_ms, c.end_ms, c.top, words(&text_of(c))))
        .filter(|cue| !cue.3.is_empty())
        .collect()
}

#[test]
fn ffmpeg_agrees_on_times_and_text() {
    let require = std::env::var_os(REQUIRE_TOOLS).is_some_and(|v| v == "1");
    if !tool_present("ffmpeg", require) {
        return;
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/srt");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("the data folder")
        .map(|entry| entry.expect("a readable entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "srt"))
        .collect();
    files.sort();
    let mut compared = 0;
    for path in files {
        let name = path
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        if DIVERGENT_FILES.iter().any(|(file, _)| *file == name) {
            continue;
        }
        let bytes = fs::read(&path).expect("a readable data file");
        let mut cues = read_srt(&bytes)
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .cues;
        sort_cues(&mut cues);
        let mut ours = ours(&cues);
        ours.sort();

        let out = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(&path)
            .args(["-f", "srt", "-"])
            .output()
            .expect("ffmpeg starts");
        assert!(
            out.status.success(),
            "{name}: ffmpeg failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut theirs = reference(&String::from_utf8_lossy(&out.stdout));
        theirs.sort();
        assert_eq!(
            ours, theirs,
            "{name}: times, top or text differ from ffmpeg"
        );
        compared += 1;
    }
    // ordinary, irregular and markup: a comparison that checked nothing would
    // pass, so the floor is the number of agreement files.
    assert!(compared >= 3, "compared {compared} files");
}

/// The densest line shape: one cue of `x` lines, two bytes each. Every line
/// is a line and a run, so the input passes the item budget after about a
/// million lines and is refused whole. Nothing is kept.
#[test]
fn dense_one_cue_of_tiny_lines_is_refused() {
    let header = "1\n0:00:00,0 --> 0:00:00,1\n";
    let text = format!("{header}{}", "x\n".repeat((INPUT_BYTES - header.len()) / 2));
    assert!(text.len() <= INPUT_BYTES);
    assert_eq!(read_srt(text.as_bytes()), Err(ReadError::TooComplex));
}

/// The densest real file: the smallest SRT cues, 27 bytes each (a timing line,
/// one line of one run, and a blank line), about 310,000 cues of three items.
/// That is under the budget, and every cue is kept.
#[test]
fn densest_real_file_still_fits() {
    let cue = "0:00:00,0 --> 0:00:00,1\nx\n\n";
    let count = INPUT_BYTES / cue.len();
    let parsed = read_srt(cue.repeat(count).as_bytes()).expect("the densest real file fits");
    assert_eq!(parsed.cues.len(), count);
    assert!(3 * count <= subtitles::ITEM_BUDGET);
}

/// Coordinates after the end time are dropped and reported as `Position`,
/// once per cue that has them. A cue without them reports nothing.
#[test]
fn srt_coordinates_report_position_loss() {
    let parsed = read(
        "1\n00:00:01,000 --> 00:00:02,000 X1:100 X2:200 Y1:10 Y2:20\nhi\nthere\n\n\
         2\n00:00:03,000 --> 00:00:04,000\nplain\n\n\
         3\n00:00:05,000 --> 00:00:06,000 X1:5\nlast\n",
    );
    assert_eq!(texts(&parsed), vec!["hi\nthere", "plain", "last"]);
    assert_eq!(parsed.losses, losses(&[(Loss::Position, 2)]));
}

/// A cue whose text is only white space has no text left: it is dropped
/// silently, as an empty cue is, and a file of only such cues has no cue.
#[test]
fn white_space_only_cue_is_dropped_silently() {
    let parsed =
        read("1\n00:00:01,000 --> 00:00:02,000\n<i> </i>\n\n2\n00:00:03,000 --> 00:00:04,000\nx\n");
    assert_eq!(texts(&parsed), vec!["x"]);
    assert_eq!(parsed.skipped.count, 0);
    assert!(parsed.losses.is_empty());
    assert_eq!(
        read_srt("1\n00:00:01,000 --> 00:00:02,000\n<i> </i>\n".as_bytes()),
        Err(ReadError::NoCues)
    );
}
