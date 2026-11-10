//! ASS and SSA reader: the libass rules as this reader keeps them, the limits,
//! the linear-time guards, and a differential check against ffmpeg on the files
//! in `tests/data/ass` (`wild.ass` is the exception, see `DIVERGENT_FILES`).
//!
//! Deliberate differences from ffmpeg. Each is pinned by the test named after
//! it; the differential test cannot show them, so its files avoid them.
//! - `\h` is a no-break space here; ffmpeg keeps the two characters `\h`
//!   (`hard_and_soft_breaks_and_spaces`).
//! - `\n` is a space unless `WrapStyle: 2`; ffmpeg breaks the line at every
//!   `\n` (`n_is_a_space_unless_wrapstyle_two`).
//! - Drawing text (`\p1` to `\p0`) is dropped; ffmpeg keeps it as text
//!   (`drawing_text_is_dropped_and_reported`).
//! - A `Format:` line in `[Events]` that orders the columns differently is read;
//!   ffmpeg drops every dialogue of such a file (`reordered_format_columns`).
//! - Three-digit centiseconds are skipped and counted; ffmpeg drops the cue
//!   without a word (`three_digit_centiseconds_are_skipped`).
//! - A zero-length cue is kept, as the SRT reader keeps it; ffmpeg drops it
//!   (`zero_length_cue_is_kept`).
//! - A line of text in `[Events]` that is not a dialogue is counted as skipped;
//!   ffmpeg ignores it silently (`text_outside_cues_is_counted_per_line`).
//! - A style name the file does not define takes the `Default` style; ffmpeg
//!   applies no style. Only the styling differs, and the comparison does not
//!   check styling (`unknown_style_falls_back_to_default`).
//! - Section names in other letter cases and with a trailing space are read;
//!   ffmpeg reads none of a file whose `[Script Info]` is in lowercase
//!   (`section_names_ignore_case_and_spaces`).
//!
//! Standard library only, as the reader.

use std::collections::BTreeMap;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use subtitles::{Cue, INPUT_BYTES, Loss, Parsed, ReadError, Skipped, read_ass, sort_cues};

/// Data files the ffmpeg comparison leaves out, with the reason.
const DIVERGENT_FILES: &[(&str, &str)] = &[(
    "wild.ass",
    "hand-written edge cases; the differences above are pinned by their named tests",
)];

/// Set to `1` on a runner that must have ffmpeg: a missing tool then fails
/// the test instead of skipping the comparison.
const REQUIRE_TOOLS: &str = "INACIRCLE_REQUIRE_SUBTITLE_TOOLS";

const V4_PLUS_FORMAT: &str = "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding";
const V4_FORMAT: &str = "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, TertiaryColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding";
const EVENT_FORMAT: &str =
    "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text";

/// A `[V4+ Styles]` line with the given values in the usual column order.
fn style(name: &str, bold: &str, italic: &str, underline: &str, alignment: &str) -> String {
    format!(
        "Style: {name},Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,{bold},{italic},{underline},0,100,100,0,0,1,2,0,{alignment},10,10,10,1\n"
    )
}

/// A `[V4 Styles]` line with the given name and alignment.
fn v4_style(name: &str, alignment: &str) -> String {
    format!("Style: {name},Arial,20,16777215,255,0,0,0,0,1,2,0,{alignment},10,10,10,0,0\n")
}

/// A Dialogue line at 1 s to 2 s, in the usual column order.
fn dialogue(style: &str, text: &str) -> String {
    format!("Dialogue: 0,0:00:01.00,0:00:02.00,{style},,0,0,0,,{text}\n")
}

/// A complete file: `[Script Info]` with the given WrapStyle, one `[V4+ Styles]`
/// section holding `styles`, and an `[Events]` section holding `events`.
fn v4plus(wrap: &str, styles: &str, events: &str) -> String {
    format!(
        "[Script Info]\nWrapStyle: {wrap}\n\n[V4+ Styles]\n{V4_PLUS_FORMAT}\n{styles}\n[Events]\n{EVENT_FORMAT}\n{events}"
    )
}

/// A file with the usual `Default` style (not bold, bottom) and `events`.
fn plain(events: &str) -> String {
    v4plus("0", &style("Default", "0", "0", "0", "2"), events)
}

fn read(text: &str) -> Parsed {
    read_ass(text.as_bytes()).expect("the input has a usable cue")
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

fn tops(parsed: &Parsed) -> Vec<bool> {
    parsed.cues.iter().map(|c| c.top).collect()
}

fn losses(pairs: &[(Loss, usize)]) -> BTreeMap<Loss, usize> {
    pairs.iter().copied().collect()
}

/// Whether every run of the cue has the given bold flag.
fn all_bold(cue: &Cue) -> bool {
    cue.lines.iter().flat_map(|l| &l.runs).all(|r| r.bold)
}

/// The runs of a cue's first line as (text, italic, bold, underline).
fn flags(cue: &Cue) -> Vec<(&str, bool, bool, bool)> {
    cue.lines[0]
        .runs
        .iter()
        .map(|r| (r.text.as_str(), r.italic, r.bold, r.underline))
        .collect()
}

#[test]
fn style_and_override_alignment_set_top() {
    // Alignment 8 is the top row in V4+; 2 is the bottom. An override names a
    // row, and a cue that is at the top at some point stays top (the SRT rule):
    // a bottom override does not move a top cue down.
    let parsed = read(&v4plus(
        "0",
        &format!(
            "{}{}",
            style("Top", "0", "0", "0", "8"),
            style("Bottom", "0", "0", "0", "2")
        ),
        &[
            dialogue("Top", "style top"),
            dialogue("Bottom", "bottom style"),
            dialogue("Bottom", "{\\an8}override top"),
            dialogue("Top", "{\\an2}top style stays top"),
            dialogue("Bottom", "{\\an4}middle"),
            dialogue("Bottom", "{\\an1}bottom left"),
            dialogue("Bottom", "{\\an7}top left"),
            dialogue("Bottom", "{\\an9}top right"),
            dialogue("Top", "{\\r}back to the top style"),
            dialogue("Bottom", "{\\rTop}named top"),
        ]
        .concat(),
    ));
    assert_eq!(
        tops(&parsed),
        vec![
            true, false, true, true, false, false, true, true, true, true
        ]
    );
    assert!(parsed.losses.is_empty());
}

#[test]
fn ssa_v4_alignment_scheme() {
    // In `[V4 Styles]` and in `\a`, 5, 6 and 7 are the top row. In `\an` and
    // V4+ the top row is 7, 8 and 9. Numbers outside the scheme are not top.
    let styles: String = (1..=11)
        .map(|a| v4_style(&format!("S{a}"), &a.to_string()))
        .collect();
    let events: String = (1..=11).map(|a| dialogue(&format!("S{a}"), "x")).collect();
    let file = format!(
        "[Script Info]\nWrapStyle: 0\n\n[V4 Styles]\n{V4_FORMAT}\n{styles}\n[Events]\n{EVENT_FORMAT}\n{events}"
    );
    let parsed = read(&file);
    let expected: Vec<bool> = (1..=11).map(|a| (5..=7).contains(&a)).collect();
    assert_eq!(tops(&parsed), expected);

    // The same numbers in a V4+ section: the top row is 7 to 9, so 5 and 6 are
    // not top and 7 is.
    let v4plus_file = v4plus(
        "0",
        &format!(
            "{}{}{}",
            style("Five", "0", "0", "0", "5"),
            style("Six", "0", "0", "0", "6"),
            style("Seven", "0", "0", "0", "7"),
        ),
        &[
            dialogue("Five", "x"),
            dialogue("Six", "x"),
            dialogue("Seven", "x"),
        ]
        .concat(),
    );
    assert_eq!(tops(&read(&v4plus_file)), vec![false, false, true]);

    // `\a` is the SSA scheme whatever the section, and `\an` the numpad.
    let overrides = [
        "{\\a6}a6",
        "{\\a9}a9",
        "{\\a1}a1",
        "{\\an8}an8",
        "{\\an5}an5",
    ];
    let events: String = overrides.iter().map(|t| dialogue("S5", t)).collect();
    let file = format!(
        "[Script Info]\nWrapStyle: 0\n\n[V4 Styles]\n{V4_FORMAT}\n{}\n[Events]\n{EVENT_FORMAT}\n{events}",
        v4_style("S5", "1")
    );
    assert_eq!(tops(&read(&file)), vec![true, false, false, true, false]);

    // `\a0` is not in the scheme: it is styling the reader cannot carry.
    let parsed = read(&file.replace("{\\an5}an5", "{\\a0}bad"));
    assert_eq!(parsed.losses, losses(&[(Loss::Styling, 1)]));
}

#[test]
fn reordered_format_columns() {
    // The styles' columns follow their own Format line, whatever the order.
    let parsed = read(&format!(
        "[Script Info]\nWrapStyle: 0\n\n[V4+ Styles]\nFormat: Alignment, Name, Italic, Bold, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, MarginL, MarginR, MarginV, Encoding\nStyle: 8,Top,0,-1,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,100,100,0,0,1,2,0,10,10,10,1\n\n[Events]\n{EVENT_FORMAT}\n{}",
        dialogue("Top", "styled by the reordered line")
    ));
    assert_eq!(tops(&parsed), vec![true]);
    assert!(all_bold(&parsed.cues[0]) && !parsed.cues[0].lines[0].runs[0].italic);

    // The dialogue's columns follow its own Format line: text is still the last
    // one and keeps its commas.
    let parsed = read(
        "[Script Info]\nWrapStyle: 0\n\n[Events]\nFormat: Start, Style, Layer, End, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0:00:01.00,Top,0,0:00:02.00,,0,0,0,,Reordered, with commas\n",
    );
    assert_eq!(texts(&parsed), vec!["Reordered, with commas"]);
    assert_eq!(spans(&parsed), vec![(1_000, 2_000)]);

    // A Format line without Text last, or without Start, makes its dialogue
    // unreadable: it is skipped and counted, and the next Format line counts.
    let parsed = read(
        "[Script Info]\nWrapStyle: 0\n\n[Events]\nFormat: Text, Start, End, Style\nDialogue: lost,0:00:01.00,0:00:02.00,Default\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,kept\n",
    );
    assert_eq!(texts(&parsed), vec!["kept"]);
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 1,
            first: Some((6, "unusable Events Format line")),
        }
    );
}

#[test]
fn missing_format_lines_use_the_usual_columns() {
    // Without a Format line, the usual column order is used for both sections.
    let parsed = read(
        "[Script Info]\nWrapStyle: 0\n\n[V4+ Styles]\nStyle: Top,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,-1,0,0,0,100,100,0,0,1,2,0,8,10,10,10,1\n\n[Events]\nDialogue: 0,0:00:01.00,0:00:02.00,Top,,0,0,0,,usual order\n",
    );
    assert_eq!(texts(&parsed), vec!["usual order"]);
    assert_eq!(tops(&parsed), vec![true]);
    assert!(all_bold(&parsed.cues[0]));
}

#[test]
fn text_commas_belong_to_the_text() {
    let parsed = read(&plain(
        "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,a, b,,c,\n",
    ));
    assert_eq!(texts(&parsed), vec!["a, b,,c,"]);
}

#[test]
fn transform_arguments_are_not_tags() {
    // `\t(...)` animates over time: the tags inside it are not applied to the
    // text now, and the whole tag is one styling loss.
    let parsed = read(&plain(
        "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\t(\\i1\\b1\\fs20)}plain\n",
    ));
    assert_eq!(texts(&parsed), vec!["plain"]);
    let runs = &parsed.cues[0].lines[0].runs;
    assert!(runs.len() == 1 && !runs[0].italic && !runs[0].bold);
    assert_eq!(parsed.losses, losses(&[(Loss::Styling, 1)]));
}

#[test]
fn bold_minus_one_and_weight_values() {
    // Style bold: -1 and 1 are on, 0 is off, and a weight from 600 up is bold.
    let styles: String = [
        ("Minus", "-1"),
        ("Zero", "0"),
        ("One", "1"),
        ("W700", "700"),
        ("W400", "400"),
        ("W600", "600"),
        ("W599", "599"),
    ]
    .iter()
    .map(|(name, bold)| style(name, bold, "0", "0", "2"))
    .collect();
    let names = ["Minus", "Zero", "One", "W700", "W400", "W600", "W599"];
    let events: String = names.iter().map(|n| dialogue(n, "x")).collect();
    let parsed = read(&v4plus("0", &styles, &events));
    let bold: Vec<bool> = parsed.cues.iter().map(all_bold).collect();
    assert_eq!(bold, vec![true, false, true, true, false, true, false]);

    // Overrides follow the same rule. An italic value that is neither on nor
    // off returns to the style, as libass does: `\i-1` and `\i2` on a plain
    // style leave the text plain.
    let parsed = read(&plain(
        "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\b1}a{\\b700}b{\\b400}c{\\b0}d\n\
         Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,{\\i-1}e{\\i2}f\n",
    ));
    let runs: Vec<(String, bool)> = parsed.cues[0].lines[0]
        .runs
        .iter()
        .map(|r| (r.text.clone(), r.bold))
        .collect();
    // Runs of one style are one run: "a" and "b" are both bold.
    assert_eq!(runs, vec![("ab".into(), true), ("cd".into(), false)]);
    let italic = &parsed.cues[1].lines[0].runs;
    assert_eq!(italic.len(), 1, "`\\i2` does not change the italic state");
    assert!(!italic[0].italic && italic[0].text == "ef");
    assert!(parsed.losses.is_empty());
}

#[test]
fn reset_to_named_style() {
    // `\r` returns to the cue's own style, `\r<Style>` to the named one. A cue
    // that is top at some point stays top.
    let styles = format!(
        "{}{}",
        style("Default", "-1", "0", "0", "2"),
        style("Ital", "0", "-1", "1", "8")
    );
    let parsed = read(&v4plus(
        "0",
        &styles,
        &[
            dialogue("Default", "plain {\\rItal}italic top"),
            dialogue("Default", "{\\rItal}start {\\r}back to bold"),
            dialogue("Ital", "{\\r}home is italic"),
            dialogue("Default", "{\\i1}{\\rItal}{\\rDefault}named again"),
        ]
        .concat(),
    ));
    // The fourth cue is top too: `\rItal` put it at the top for a moment.
    assert_eq!(tops(&parsed), vec![true, true, true, true]);
    let first = &parsed.cues[0].lines[0].runs;
    assert_eq!(first.len(), 2);
    assert!(first[0].bold && !first[0].italic && first[0].text == "plain ");
    assert!(!first[1].bold && first[1].italic && first[1].underline);
    let second = &parsed.cues[1].lines[0].runs;
    assert!(second[0].italic && second[0].text == "start ");
    assert!(second.last().is_some_and(|r| r.bold && !r.italic));
    assert!(parsed.cues[2].lines[0].runs[0].italic);
    // `\i1` was reset by `\rDefault`, and the cue ends in the bold style.
    assert!(all_bold(&parsed.cues[3]));
}

#[test]
fn bare_bold_italic_underline_return_to_the_style() {
    // A bare `\b`, `\i` or `\u` (no value) returns that flag to the cue's own
    // style, as libass does. Checked by rendering with libass (ffmpeg's
    // `subtitles` filter, a bold style, `{\b0}HHHH{\b}HHHH`): the frame's ink
    // matches `{\b1}` over that style (584,990) and not regular text (418,650);
    // `\i` and `\u` the same way. ffmpeg's own SRT text writer treats a bare
    // `\b` as off, so its output is not the reference here. A bare flag is
    // not styling the reader cannot carry, so it is not counted as lost.
    let plain = style("Default", "0", "0", "0", "2");
    let bold = style("Default", "-1", "0", "0", "2");
    let italic = style("Default", "0", "-1", "0", "2");
    let underline = style("Default", "0", "0", "-1", "2");
    let cue = |styles: &str, text: &str| {
        let parsed = read(&v4plus("0", styles, &dialogue("Default", text)));
        assert!(parsed.losses.is_empty(), "{text}: {:?}", parsed.losses);
        parsed.cues[0].lines[0]
            .runs
            .iter()
            .map(|r| (r.text.clone(), r.italic, r.bold, r.underline))
            .collect::<Vec<_>>()
    };
    let runs = |list: &[(&str, bool, bool, bool)]| -> Vec<(String, bool, bool, bool)> {
        list.iter()
            .map(|&(t, i, b, u)| (t.to_string(), i, b, u))
            .collect()
    };
    // Plain style: a bare flag is off, so `b` is not bold.
    assert_eq!(
        cue(&plain, "x{\\b1}a{\\b}b"),
        runs(&[
            ("x", false, false, false),
            ("a", false, true, false),
            ("b", false, false, false)
        ])
    );
    // Bold style: a bare `\b` after `\b0` brings the bold back; after `\b1` it
    // changes nothing.
    assert_eq!(
        cue(&bold, "x{\\b0}a{\\b}b"),
        runs(&[
            ("x", false, true, false),
            ("a", false, false, false),
            ("b", false, true, false)
        ])
    );
    assert_eq!(
        cue(&bold, "x{\\b1}a{\\b}b"),
        runs(&[("xab", false, true, false)])
    );
    assert_eq!(
        cue(&italic, "x{\\i0}a{\\i}b"),
        runs(&[
            ("x", true, false, false),
            ("a", false, false, false),
            ("b", true, false, false)
        ])
    );
    assert_eq!(
        cue(&plain, "x{\\i1}a{\\i}b"),
        runs(&[
            ("x", false, false, false),
            ("a", true, false, false),
            ("b", false, false, false)
        ])
    );
    assert_eq!(
        cue(&underline, "x{\\u0}a{\\u}b"),
        runs(&[
            ("x", false, false, true),
            ("a", false, false, false),
            ("b", false, false, true)
        ])
    );
    assert_eq!(
        cue(&plain, "x{\\u1}a{\\u}b"),
        runs(&[
            ("x", false, false, false),
            ("a", false, false, true),
            ("b", false, false, false)
        ])
    );
    // `\r` returns to the cue's own style, as before.
    assert_eq!(
        cue(&plain, "x{\\b1}a{\\r}b"),
        runs(&[
            ("x", false, false, false),
            ("a", false, true, false),
            ("b", false, false, false)
        ])
    );
    assert_eq!(
        cue(&bold, "x{\\b0}a{\\r}b"),
        runs(&[
            ("x", false, true, false),
            ("a", false, false, false),
            ("b", false, true, false)
        ])
    );
}

#[test]
fn bare_flag_returns_to_the_style_of_the_last_reset() {
    // `\r<Style>` sets the style a bare `\b`, `\i` or `\u` returns to, as libass
    // does, not the cue's own style. Checked by rendering with libass:
    // `{\rAlt}a{\b0\i0}b{\b\i}c` draws the same as `{\rAlt}a{\b0\i0}b{\b1\i1}c`.
    let styles = format!(
        "{}{}",
        style("Default", "0", "0", "0", "2"),
        style("Alt", "-1", "-1", "0", "2")
    );
    let parsed = read(&v4plus(
        "0",
        &styles,
        &dialogue("Default", r"{\rAlt}a{\b0\i0}b{\b\i}c"),
    ));
    assert_eq!(
        flags(&parsed.cues[0]),
        vec![
            ("a", true, true, false),
            ("b", false, false, false),
            ("c", true, true, false)
        ]
    );
    assert!(parsed.losses.is_empty());

    // A later plain `\r` makes the cue's own style the one a bare flag returns to.
    let parsed = read(&v4plus(
        "0",
        &styles,
        &dialogue("Default", r"{\rAlt}{\r}{\b1\i1}c{\b\i}d"),
    ));
    assert_eq!(
        flags(&parsed.cues[0]),
        vec![("c", true, true, false), ("d", false, false, false)]
    );
}

#[test]
fn override_values_ignore_spaces_around_them() {
    // libass skips the spaces around a value: `{\b }` is the bare form, and
    // `{\b 1}` and `{\i 1 }` are on. None of it is styling the reader lacks.
    let parsed = read(&plain(&dialogue("Default", r"{\b1}a{\b }b{\b 1}c{\i 1 }d")));
    assert_eq!(
        flags(&parsed.cues[0]),
        vec![
            ("a", false, true, false),
            ("b", false, false, false),
            ("c", false, true, false),
            ("d", true, true, false)
        ]
    );
    assert!(parsed.losses.is_empty());
}

#[test]
fn numeric_values_other_than_on_and_off_return_to_the_style() {
    // libass reads `\b2`, `\b-1`, `\i-1` and `\u3` as a bare flag: each returns
    // the flag to the style and switches nothing on or off. Checked by
    // rendering with libass: `{\b1}a{\b-1}b` draws as `{\b1}a{\b}b`, and on a
    // bold italic style `{\b0\i0}a{\b2}b{\i3}c` makes `b` bold and `c` bold
    // and italic. `\b100` is a weight, not on: it is plain.
    let styles = format!(
        "{}{}{}",
        style("Default", "0", "0", "0", "2"),
        style("Alt", "-1", "-1", "0", "2"),
        style("Ul", "0", "0", "-1", "2")
    );
    let parsed = read(&v4plus(
        "0",
        &styles,
        &[
            dialogue("Default", r"{\b1}a{\b-1}b{\i1}c{\i-1}d"),
            dialogue("Alt", r"{\b0\i0}a{\b2}b{\i3}c"),
            dialogue("Ul", r"{\u0}a{\u2}b{\u3}c"),
            dialogue("Default", r"{\b1}a{\b100}b"),
        ]
        .concat(),
    ));
    assert_eq!(
        flags(&parsed.cues[0]),
        vec![
            ("a", false, true, false),
            ("b", false, false, false),
            ("c", true, false, false),
            ("d", false, false, false)
        ]
    );
    assert_eq!(
        flags(&parsed.cues[1]),
        vec![
            ("a", false, false, false),
            ("b", false, true, false),
            ("c", true, true, false)
        ]
    );
    assert_eq!(
        flags(&parsed.cues[2]),
        vec![("a", false, false, false), ("bc", false, false, true)]
    );
    assert_eq!(
        flags(&parsed.cues[3]),
        vec![("a", false, true, false), ("b", false, false, false)]
    );
    assert!(parsed.losses.is_empty());
}

#[test]
fn unknown_style_falls_back_to_default() {
    // A name the file does not define takes `Default`, both as the cue's style
    // and after `\r`. Our choice: ffmpeg applies no style at all.
    let parsed = read(&v4plus(
        "0",
        &style("Default", "-1", "0", "0", "2"),
        &[
            dialogue("Missing", "unknown style"),
            dialogue("Default", "{\\rMissing}unknown reset"),
        ]
        .concat(),
    ));
    assert_eq!(texts(&parsed), vec!["unknown style", "unknown reset"]);
    assert!(parsed.cues.iter().all(all_bold));
}

#[test]
fn section_names_ignore_case_and_spaces() {
    // libass reads `[V4+ Styles ]` with a trailing space and any letter case,
    // and `[v4 styles]` in the SSA scheme, where 5 is the top row and 9 is not.
    let parsed = read(
        "[script info]\nWrapStyle: 0\n\n[v4+ styles ]\nFormat: Name, Bold, Alignment\nStyle: Top, -1, 8\n\n[v4 styles]\nFormat: Name, Bold, Alignment\nStyle: Five, 0, 5\nStyle: Nine, 0, 9\n\n[EVENTS]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.00,Top,,0,0,0,,case and space\nDialogue: 0,0:00:03.00,0:00:04.00,Five,,0,0,0,,ssa top\nDialogue: 0,0:00:05.00,0:00:06.00,Nine,,0,0,0,,ssa not top\n",
    );
    assert_eq!(
        texts(&parsed),
        vec!["case and space", "ssa top", "ssa not top"]
    );
    assert_eq!(tops(&parsed), vec![true, true, false]);
    assert!(all_bold(&parsed.cues[0]));
}

#[test]
fn hard_and_soft_breaks_and_spaces() {
    // `\N` is a line break. `\h` is a no-break space. `\n` is a space unless
    // WrapStyle is 2, which tests the next test.
    let parsed = read(&plain(
        "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,a\\Nb x\\hy\n",
    ));
    assert_eq!(texts(&parsed), vec!["a\nb x\u{a0}y"]);
}

#[test]
fn n_is_a_space_unless_wrapstyle_two() {
    let events = "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,one\\ntwo\n";
    let spaced = read(&v4plus("0", &style("Default", "0", "0", "0", "2"), events));
    assert_eq!(texts(&spaced), vec!["one two"]);
    assert_eq!(spaced.cues[0].lines.len(), 1);
    let wrapped = read(&v4plus("2", &style("Default", "0", "0", "0", "2"), events));
    assert_eq!(texts(&wrapped), vec!["one\ntwo"]);
    assert_eq!(wrapped.cues[0].lines.len(), 2);
}

#[test]
fn karaoke_keeps_text_and_reports() {
    // Each karaoke tag is a timing, not text. The loss is counted once per cue.
    let parsed = read(&plain(
        "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\k20}Ka{\\kf30}ra{\\K7}ke\n\
         Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,{\\ko5}plain{\\k9}\n\
         Dialogue: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,no karaoke here\n",
    ));
    assert_eq!(texts(&parsed), vec!["Karake", "plain", "no karaoke here"]);
    assert_eq!(parsed.losses, losses(&[(Loss::Karaoke, 2)]));
}

#[test]
fn drawing_text_is_dropped_and_reported() {
    // Between `\p1` and `\p0` the text is drawing commands: it is dropped and
    // the loss is reported. `\p0` by itself has nothing to report.
    let parsed = read(&plain(
        "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\p1}m 0 0 l 10 0{\\p0}after\n\
         Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,{\\p1}m 0 0\\Nm 1 1{\\p0}x\n\
         Dialogue: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,text{\\p1}m 0 0 l 1 1\n\
         Dialogue: 0,0:00:07.00,0:00:08.00,Default,,0,0,0,,{\\p0}plain\n",
    ));
    assert_eq!(texts(&parsed), vec!["after", "x", "text", "plain"]);
    assert_eq!(parsed.losses, losses(&[(Loss::Drawing, 3)]));
}

#[test]
fn drawing_only_cue_reports_the_drawing_loss() {
    // A cue that is all drawing has no text left, so it is not kept. The
    // drawing was text the conversion removed, so each such cue is counted.
    let parsed = read(&plain(
        "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\p1}m 0 0 l 10 10{\\p0}\n\
         Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,{\\p1}m 0 0 l 1 1{\\p0}\n\
         Dialogue: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,kept\n",
    ));
    assert_eq!(texts(&parsed), vec!["kept"]);
    assert_eq!(parsed.losses, losses(&[(Loss::Drawing, 2)]));
    assert_eq!(parsed.skipped, Skipped::default());
}

#[test]
fn textless_cue_with_only_styling_reports_nothing() {
    // Decision: only a loss that removed text counts for a cue with no text.
    // Styling, position and karaoke removed no text, so those cues are dropped
    // silently. The third cue is styled and drawn: its drawing is counted and
    // its styling is not, so the count is one per loss, not per tag.
    let text = plain(
        "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\c&HFF&}\n\
         Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,{\\pos(1,2)}{\\k10}\n\
         Dialogue: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,{\\c&HFF&}{\\p1}m 0 0 l 1 1{\\p0}\n\
         Dialogue: 0,0:00:07.00,0:00:08.00,Default,,0,0,0,,kept\n",
    );
    let parsed = read(&text);
    assert_eq!(texts(&parsed), vec!["kept"]);
    assert_eq!(parsed.losses, losses(&[(Loss::Drawing, 1)]));
    assert_eq!(parsed.skipped, Skipped::default());
}

#[test]
fn file_of_drawings_only_is_refused_with_no_cues() {
    // Nothing is left to show, so the file is refused. The loss counts are not
    // part of the refusal; the CLI says `no subtitles found`.
    let text = plain("Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\p1}m 0 0 l 1 1{\\p0}\n");
    assert_eq!(read_ass(text.as_bytes()), Err(ReadError::NoCues));
}

#[test]
fn empty_cue_alone_is_silent() {
    // A Dialogue with no text is not a loss: nothing is counted or skipped.
    let parsed = read(&plain(
        "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,\n\
         Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,kept\n",
    ));
    assert_eq!(texts(&parsed), vec!["kept"]);
    assert!(parsed.losses.is_empty());
    assert_eq!(parsed.skipped, Skipped::default());
}

#[test]
fn events_format_needs_start_end_and_text() {
    // Each of these Format lines lacks one of the three columns a dialogue
    // needs, so its dialogue is skipped with the Format's reason.
    for format in [
        "Format: Layer, End, Style, Text",
        "Format: Layer, Start, Style, Text",
        "Format: Start, End, Text, Style",
    ] {
        let parsed = read(&format!(
            "[Script Info]\nWrapStyle: 0\n\n[Events]\n{format}\nDialogue: 0,0:00:01.00,0:00:02.00,Default\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,kept\n"
        ));
        assert_eq!(texts(&parsed), vec!["kept"], "{format}");
        assert_eq!(
            parsed.skipped,
            Skipped {
                count: 1,
                first: Some((6, "unusable Events Format line")),
            },
            "{format}"
        );
    }
}

#[test]
fn text_less_cue_is_dropped_silently() {
    // A dialogue with only tags has nothing to show: no cue and no count, as in
    // the SRT reader.
    let parsed = read(&plain(
        "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\i1}{\\b1}\n\
         Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,shown\n",
    ));
    assert_eq!(texts(&parsed), vec!["shown"]);
    assert_eq!(parsed.skipped, Skipped::default());
}

#[test]
fn comments_are_skipped() {
    // Comment, Picture, Sound, Movie and Command lines are not text: nothing is
    // counted and nothing is reported.
    let parsed = read(&plain(
        "Comment: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,hidden\n\
         Picture: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,picture\n\
         Sound: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,sound\n\
         Movie: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,movie\n\
         Command: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,command\n\
         Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,shown\n",
    ));
    assert_eq!(texts(&parsed), vec!["shown"]);
    assert_eq!(parsed.skipped, Skipped::default());
    assert!(parsed.losses.is_empty());
}

#[test]
fn text_outside_cues_is_counted_per_line() {
    // A line in `[Events]` that is neither a dialogue nor a known event line is
    // counted once per line; the cues around it are unchanged.
    let parsed = read(&plain(
        "stray one\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,first\nstray two\nstray three\n",
    ));
    assert_eq!(texts(&parsed), vec!["first"]);
    // The header is nine lines, so the first stray line is line 10.
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 3,
            first: Some((10, "text outside any cue")),
        }
    );
}

#[test]
fn centiseconds_become_milliseconds() {
    // A centisecond is 10 ms. One digit is a tenth of a second, as ffmpeg reads
    // it: `.5` is 50 ms. A minute of 60 seconds carries.
    let parsed = read(&plain(
        "Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,a\n\
         Dialogue: 0,0:00:01.5,0:00:02.50,Default,,0,0,0,,b\n\
         Dialogue: 0,0:0:60.00,0:60:00.00,Default,,0,0,0,,c\n\
         Dialogue: 0,99:59:59.99,99:59:59.99,Default,,0,0,0,,d\n",
    ));
    assert_eq!(
        spans(&parsed),
        vec![
            (1_000, 2_000),
            (1_050, 2_500),
            (60_000, 3_600_000),
            // 99:59:59.99 is the last time the reader keeps, 1 ms under the limit.
            (359_999_990, 359_999_990),
        ]
    );
}

#[test]
fn three_digit_centiseconds_are_skipped() {
    // ffmpeg drops such a cue silently; we cannot tell what was meant, so the
    // cue is counted as bad.
    let parsed = read(&plain(
        "Dialogue: 0,0:00:01.123,0:00:02.00,Default,,0,0,0,,three\nDialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,fine\n",
    ));
    assert_eq!(texts(&parsed), vec!["fine"]);
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 1,
            first: Some((10, "bad timestamp")),
        }
    );
}

#[test]
fn zero_length_cue_is_kept() {
    // A cue that starts and ends together is usable, as in the SRT reader.
    let parsed = read(&plain(
        "Dialogue: 0,0:00:01.00,0:00:01.00,Default,,0,0,0,,zero\n",
    ));
    assert_eq!(spans(&parsed), vec![(1_000, 1_000)]);
}

#[test]
fn bad_dialogue_is_skipped_rest_converts() {
    // 100 dialogue lines after the header. Line 40 has a `;` for the dot (a bad
    // timestamp), line 60 ends before it starts, and line 80 starts at 100 hours.
    let header = plain("");
    let first_line = header.lines().count() + 1;
    let mut file = header;
    for i in 1..=100 {
        let timing = match i {
            40 => "0:00:01;00,0:00:02.00",
            60 => "0:00:05.00,0:00:04.00",
            80 => "100:00:00.00,100:00:01.00",
            _ => "0:00:01.00,0:00:02.00",
        };
        file += &format!("Dialogue: 0,{timing},Default,,0,0,0,,cue {i}\n");
    }
    let parsed = read(&file);
    let expected: Vec<String> = (1..=100)
        .filter(|i| ![40, 60, 80].contains(i))
        .map(|i| format!("cue {i}"))
        .collect();
    assert_eq!(texts(&parsed), expected);
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 3,
            first: Some((first_line + 39, "bad timestamp")),
        }
    );
}

#[test]
fn skipped_items_in_several_places_keep_the_earliest_line() {
    // Three skipped items: a bad timestamp on the first dialogue line after the
    // header, a stray line between dialogues, and another bad timestamp near the
    // end. `first` is the earliest of them, and the count is all three.
    let header = plain("");
    let first_line = header.lines().count() + 1;
    let file = format!(
        "{header}\
         Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,first\n\
         Dialogue: 0,0:00:01;00,0:00:02.00,Default,,0,0,0,,bad early\n\
         Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,second\n\
         garbage line\n\
         Dialogue: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,third\n\
         Dialogue: 0,0:00;05.00,0:00:06.00,Default,,0,0,0,,bad late\n"
    );
    let parsed = read(&file);
    assert_eq!(texts(&parsed), vec!["first", "second", "third"]);
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 3,
            first: Some((first_line + 1, "bad timestamp")),
        }
    );
}

/// A file with one dialogue whose text is `prefix` followed by `unit` repeated
/// to at least `bytes`.
fn flood(prefix: &str, unit: &str, bytes: usize) -> Vec<u8> {
    let count = bytes / unit.len() + 1;
    plain(&format!("{prefix}{}\n", unit.repeat(count))).into_bytes()
}

/// The best of three timed runs, so one slow run does not decide the ratio.
fn best_of_three(input: &[u8]) -> Duration {
    (0..3)
        .map(|_| {
            let start = Instant::now();
            black_box(read_ass(black_box(input)).is_ok());
            start.elapsed()
        })
        .min()
        .unwrap_or_default()
}

/// Each flood is read at 1 MiB and at 4 MiB, both under INPUT_BYTES. Linear
/// work makes the 4 MiB run about 4 times the 1 MiB run; a quadratic reader
/// makes it 16 times. The limit, 8 times, is halfway between on a log scale,
/// which leaves room for cache and allocator noise.
fn assert_linear(name: &str, small: &[u8], large: &[u8]) {
    assert!(large.len() <= INPUT_BYTES, "{name}: within the input limit");
    assert!(read_ass(large).is_ok(), "{name}: the flood is usable");
    let t_small = best_of_three(small);
    let t_large = best_of_three(large);
    assert!(
        t_large < t_small * 8,
        "{name}: 4x the input took {t_large:?} against {t_small:?}; linear is about 4x"
    );
}

#[test]
fn ass_linear_on_tag_floods() {
    const SMALL: usize = 1 << 20;
    const LARGE: usize = 4 << 20;
    const PREFIX: &str = "Dialogue: 0,0:00:00.00,0:00:01.00,Default,,0,0,0,,x";
    let floods = [
        ("italic blocks on one line", "{\\i1}"),
        ("brace openers never closed", "{{{"),
        ("open parentheses in tags", "{\\t("),
        ("style resets on one line", "{\\rDefault}"),
        ("escapes on one line", "\\n"),
    ];
    for (name, unit) in floods {
        assert_linear(
            name,
            &flood(PREFIX, unit, SMALL),
            &flood(PREFIX, unit, LARGE),
        );
    }
    // Whole cues: the reader does its work per line, so many short lines must
    // stay linear too.
    let minimal = "Dialogue: 0,0:00:00.00,0:00:01.00,Default,,0,0,0,,x\n";
    assert_linear(
        "minimal cues",
        &flood("", minimal, SMALL),
        &flood("", minimal, LARGE),
    );
}

#[test]
fn ass_linear_on_format_lines() {
    // A Format line with N columns and a dialogue with N commas: the columns
    // are found once per Format line, so the work stays linear in N.
    let file = |n: usize| {
        format!(
            "[Script Info]\nWrapStyle: 0\n\n[V4+ Styles]\n{V4_PLUS_FORMAT}\n{}\n[Events]\nFormat: Start, End, {}Text\nDialogue: 0:00:00.00,0:00:01.00,{}text\n",
            style("Default", "0", "0", "0", "2"),
            "x, ".repeat(n),
            "x,".repeat(n),
        )
        .into_bytes()
    };
    // The width is what changes; `x, ` is 3 bytes and `x,` is 2, so 1 MiB and
    // 4 MiB of columns are about 350 000 and 1.4 million columns.
    assert_linear("format columns", &file((1 << 20) / 3), &file((4 << 20) / 3));
}

#[test]
fn reader_never_panics_on_every_prefix() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/ass");
    let wild = fs::read(dir.join("wild.ass")).expect("the wild file");
    assert!(read_ass(&wild).is_ok());
    for end in 0..=wild.len() {
        let _ = read_ass(&wild[..end]);
    }
    for start in 0..=wild.len() {
        let _ = read_ass(&wild[start..]);
    }
}

#[test]
fn wild_file_has_the_documented_results() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/ass");
    let wild = fs::read(dir.join("wild.ass")).expect("the wild file");
    let parsed = read_ass(&wild).expect("the wild file has usable cues");
    assert_eq!(
        texts(&parsed),
        vec![
            "hard\nbreak, soft break and hard\u{a0}space",
            "after drawing",
            "transform font pos move",
            "unknown style name",
            " block and i and } stray brace { open",
            "bottom override of a bottom style",
            "green, i2 b400 italic",
            "unclosed { brace",
            "non-ASCII: árvíztűrő",
            "back to Default after an unknown name",
        ]
    );
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 5,
            first: Some((24, "bad timestamp")),
        }
    );
    // Styling is counted once per cue: the transform and the font are one cue,
    // and the colour, size and italic 2 are another.
    assert_eq!(
        parsed.losses,
        losses(&[(Loss::Styling, 2), (Loss::Position, 1), (Loss::Drawing, 1),])
    );
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

/// Text with every `<…>` and `{…}` removed and the words joined by one space.
fn plain_text(text: &str) -> String {
    let mut out = String::new();
    let mut close = None;
    for c in text.chars() {
        match close {
            Some(end) => {
                if c == end {
                    close = None;
                }
            }
            None => match c {
                '<' => close = Some('>'),
                '{' => close = Some('}'),
                _ => out.push(c),
            },
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// (start, end, top, plain text) for each cue with text.
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
            Some((clock(from)?, clock(to)?, top, plain_text(&raw)))
        })
        .filter(|cue| !cue.3.is_empty())
        .collect()
}

fn ours(cues: &[Cue]) -> Vec<Shown> {
    cues.iter()
        .map(|c| (c.start_ms, c.end_ms, c.top, plain_text(&text_of(c))))
        .filter(|cue| !cue.3.is_empty())
        .collect()
}

#[test]
fn ffmpeg_agrees_on_times_and_text() {
    let require = std::env::var_os(REQUIRE_TOOLS).is_some_and(|v| v == "1");
    if !tool_present("ffmpeg", require) {
        return;
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/ass");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("the data folder")
        .map(|entry| entry.expect("a readable entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "ass"))
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
        let mut cues = read_ass(&bytes)
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
    // ordinary, v4, reordered and reset: a comparison that checked nothing
    // would pass, so the floor is the number of agreement files.
    assert!(compared >= 4, "compared {compared} files");
}

#[test]
fn input_limit_is_inclusive() {
    let head = b"[Script Info]\nWrapStyle: 0\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,x\n";
    let mut exact = head.to_vec();
    exact.resize(INPUT_BYTES, b'\n');
    assert!(read_ass(&exact).is_ok());
    exact.push(b'\n');
    assert_eq!(read_ass(&exact), Err(ReadError::TooLarge));
}

#[test]
fn no_dialogue_is_no_cues() {
    assert_eq!(read_ass(b""), Err(ReadError::NoCues));
    assert_eq!(
        read_ass(b"[Script Info]\nTitle: nothing here\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n"),
        Err(ReadError::NoCues)
    );
}

/// A dialogue line with a bad time is skipped, and a file whose dialogue is
/// all skipped says why, with the first one's line (line 10 here).
#[test]
fn every_cue_unusable_names_the_first_reason() {
    assert_eq!(
        read_ass(plain("Dialogue: 0,0:00:05.00,0:00:04.00,Default,,0,0,0,,backwards\n").as_bytes()),
        Err(ReadError::AllSkipped {
            count: 1,
            line: 10,
            reason: "ends before it starts"
        })
    );
}

#[test]
fn not_utf8_names_the_byte() {
    let body = plain("Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,ab\u{0}");
    let mut bytes = body.into_bytes();
    let at = bytes.len() - 1;
    bytes[at] = 0xFF;
    assert_eq!(read_ass(&bytes), Err(ReadError::NotUtf8 { at }));
}

#[test]
fn overlapping_cues_sorted_stably() {
    let mut parsed = read(&plain(
        "Dialogue: 0,0:00:05.00,0:00:09.00,Default,,0,0,0,,A\n\
         Dialogue: 0,0:00:02.00,0:00:03.00,Default,,0,0,0,,B\n\
         Dialogue: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,C\n\
         Dialogue: 0,0:00:02.00,0:00:04.00,Default,,0,0,0,,D\n",
    ));
    assert_eq!(texts(&parsed), vec!["A", "B", "C", "D"]);
    sort_cues(&mut parsed.cues);
    // Cues that start together keep their file order: B before D, A before C.
    assert_eq!(texts(&parsed), vec!["B", "D", "A", "C"]);
}

/// The densest line shape: `x\N` is a run and a line per three bytes, so the
/// input is over the item budget long before its end and is refused whole.
#[test]
fn dense_one_cue_of_tiny_lines_is_refused() {
    let head = "[Events]\nFormat: Start, End, Text\nDialogue: 0:00:00.00,0:00:00.01,";
    let text = format!(
        "{head}{}\n",
        "x\\N".repeat((INPUT_BYTES - head.len() - 1) / 3)
    );
    assert!(text.len() <= INPUT_BYTES);
    assert_eq!(read_ass(text.as_bytes()), Err(ReadError::TooComplex));
}

/// The densest real file: the smallest ASS dialogues, 34 bytes each, about
/// 247,000 cues of three items. It fits, and every cue is kept.
#[test]
fn densest_real_file_still_fits() {
    let head = "[Events]\nFormat: Start, End, Text\n";
    let cue = "Dialogue: 0:00:00.00,0:00:00.01,x\n";
    let count = (INPUT_BYTES - head.len()) / cue.len();
    let text = format!("{head}{}", cue.repeat(count));
    let parsed = read_ass(text.as_bytes()).expect("the densest real file fits");
    assert_eq!(parsed.cues.len(), count);
    assert!(3 * count <= subtitles::ITEM_BUDGET);
}

/// A cue whose text is only white space has no text left: it is dropped
/// silently. A drawing removed from it is still counted, as for any cue with
/// no text left.
#[test]
fn white_space_only_cue_is_dropped_silently() {
    // The space sits between two tags: the line's own trim would take a
    // space at its end before the reader saw it.
    let events = format!(
        "{}{}{}",
        dialogue("Default", r"{\i1} {\i0}"),
        dialogue("Default", r"{\p1}m 0 0{\p0}{\i1} {\i0}"),
        dialogue("Default", "x")
    );
    let parsed = read(&plain(&events));
    assert_eq!(texts(&parsed), vec!["x"]);
    assert_eq!(parsed.skipped.count, 0);
    assert_eq!(parsed.losses, losses(&[(Loss::Drawing, 1)]));
}
