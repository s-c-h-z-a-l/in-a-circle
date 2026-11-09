use std::collections::BTreeMap;

use subtitles::{
    Cue, Line, Loss, MAX_MS, Run, read_srt, read_vtt, sort_cues, write_srt, write_vtt,
};

const BOM: &str = "\u{feff}";

fn run(text: &str) -> Run {
    Run {
        text: text.into(),
        italic: false,
        bold: false,
        underline: false,
    }
}

fn plain(text: &str) -> Line {
    Line {
        voice: None,
        runs: vec![run(text)],
    }
}

fn cue(start_ms: u64, end_ms: u64, lines: Vec<Line>) -> Cue {
    Cue {
        start_ms,
        end_ms,
        top: false,
        lines,
    }
}

fn srt(cues: &[Cue]) -> (String, BTreeMap<Loss, usize>) {
    let mut out = Vec::new();
    let losses = write_srt(cues, &mut out).expect("writing to a Vec cannot fail");
    (
        String::from_utf8(out).expect("the writer emits UTF-8"),
        losses,
    )
}

fn vtt(cues: &[Cue]) -> (String, BTreeMap<Loss, usize>) {
    let mut out = Vec::new();
    let losses = write_vtt(cues, &mut out).expect("writing to a Vec cannot fail");
    (
        String::from_utf8(out).expect("the writer emits UTF-8"),
        losses,
    )
}

#[test]
fn writers_keep_italic_bold_underline_and_top() {
    let styled = Line {
        voice: None,
        runs: vec![
            run("plain "),
            Run {
                text: "all".into(),
                italic: true,
                bold: true,
                underline: true,
            },
        ],
    };
    let mut top = cue(1_000, 2_000, vec![styled]);
    top.top = true;

    let (s, _) = srt(std::slice::from_ref(&top));
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:01,000 --> 00:00:02,000\r\n{\\an8}plain <i><b><u>all</u></b></i>\r\n\r\n"
    );
    let (v, _) = vtt(std::slice::from_ref(&top));
    assert_eq!(
        v,
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000 line:0\nplain <i><b><u>all</u></b></i>\n\n"
    );
}

#[test]
fn srt_writer_numbers_from_one_with_bom_and_crlf() {
    let cues = [
        cue(0, 1_000, vec![plain("a")]),
        cue(1_000, 2_000, vec![plain("b")]),
    ];
    let (s, _) = srt(&cues);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\na\r\n\r\n\
         2\r\n00:00:01,000 --> 00:00:02,000\r\nb\r\n\r\n"
    );
    // Every line feed is part of a CRLF pair.
    assert_eq!(s.matches('\n').count(), s.matches("\r\n").count());
}

#[test]
fn vtt_writer_header_lf_no_bom_and_line_zero_for_top() {
    let mut top = cue(0, 1_000, vec![plain("up")]);
    top.top = true;
    let (v, losses) = vtt(&[top, cue(1_000, 2_000, vec![plain("down")])]);
    assert_eq!(
        v,
        "WEBVTT\n\n00:00:00.000 --> 00:00:01.000 line:0\nup\n\n\
         00:00:01.000 --> 00:00:02.000\ndown\n\n"
    );
    assert!(!v.contains('\r') && !v.starts_with(BOM));
    assert!(losses.is_empty());
}

#[test]
fn empty_cues_and_blank_lines_are_dropped() {
    let mut top_empty_first = cue(
        1_000,
        2_000,
        vec![plain(""), plain("x"), plain(""), plain("y")],
    );
    top_empty_first.top = true;
    let cues = [
        // No text at all.
        cue(0, 1_000, vec![plain("")]),
        // Starts with empty lines: the top marker goes on the first written line.
        top_empty_first,
        // No lines.
        cue(2_000, 3_000, vec![]),
        // A speaker with no text: not written, so not counted as a voiced cue.
        cue(
            3_000,
            4_000,
            vec![Line {
                voice: Some("Ann".into()),
                runs: vec![],
            }],
        ),
    ];
    let (s, losses) = srt(&cues);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:01,000 --> 00:00:02,000\r\n{\\an8}x\r\ny\r\n\r\n"
    );
    assert!(losses.is_empty());

    let (v, _) = vtt(&cues);
    assert_eq!(
        v,
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000 line:0\nx\ny\n\n"
    );
}

#[test]
fn arrow_and_markup_are_escaped_in_vtt() {
    let text = "a --> b & <c> d &amp; e";
    let cues = [cue(0, 1_000, vec![plain(text)])];

    let (v, _) = vtt(&cues);
    assert_eq!(
        v,
        "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\na --&gt; b &amp; &lt;c&gt; d &amp;amp; e\n\n"
    );

    // SRT has no escapes its reader reads, so the `<` of `<c>` is a look-alike;
    // `-->` and `&` are not markup and stay.
    let (s, losses) = srt(&cues);
    assert!(
        s.contains("\r\na --> b & \u{ff1c}c> d &amp; e\r\n"),
        "{s:?}"
    );
    assert_eq!(losses, BTreeMap::from([(Loss::Brackets, 1)]));
}

/// The `>` that closes `<v Name>` follows the name, so a name ending in `--`
/// would put `-->` in the line and end the cue on read back. The trailing dashes
/// are written as references, so the name reads back whole and the cue stays.
#[test]
fn voice_name_ending_in_dashes_does_not_read_as_an_arrow() {
    let voiced = Line {
        voice: Some("Ann--".into()),
        runs: vec![run("hi")],
    };
    let cues = [cue(0, 1_000, vec![voiced])];
    let (v, losses) = vtt(&cues);
    let back = read_vtt(v.as_bytes()).expect("our output is usable");
    assert_eq!(back.skipped.count, 0, "{v:?}");
    assert_eq!(back.cues.len(), 1);
    assert_eq!(back.cues[0].lines[0].voice.as_deref(), Some("Ann--"));
    assert!(losses.is_empty());
}

#[test]
fn voice_is_a_name_prefix_in_srt_and_a_tag_in_vtt() {
    let voiced = |name: &str, text: &str| Line {
        voice: Some(name.into()),
        runs: vec![run(text)],
    };
    let cues = [
        cue(0, 1_000, vec![voiced("Ann Lee", "hi"), plain("there")]),
        cue(1_000, 2_000, vec![voiced("Bo", "one"), voiced("Bo", "two")]),
    ];

    let (s, losses) = srt(&cues);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\nAnn Lee: hi\r\nthere\r\n\r\n\
         2\r\n00:00:01,000 --> 00:00:02,000\r\nBo: one\r\nBo: two\r\n\r\n"
    );
    // Counted per cue, not per line.
    assert_eq!(losses, BTreeMap::from([(Loss::VoiceNames, 2)]));

    let (v, losses) = vtt(&cues);
    assert_eq!(
        v,
        "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\n<v Ann Lee>hi</v>\nthere\n\n\
         00:00:01.000 --> 00:00:02.000\n<v Bo>one</v>\n<v Bo>two</v>\n\n"
    );
    assert!(losses.is_empty());
}

#[test]
fn overlapping_cues_are_kept_in_given_order() {
    // Given out of start order and overlapping: the writer does not sort.
    let cues = [
        cue(5_000, 15_000, vec![plain("B")]),
        cue(0, 10_000, vec![plain("A")]),
    ];

    let (s, _) = srt(&cues);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:05,000 --> 00:00:15,000\r\nB\r\n\r\n\
         2\r\n00:00:00,000 --> 00:00:10,000\r\nA\r\n\r\n"
    );
    let (v, _) = vtt(&cues);
    assert_eq!(
        v,
        "WEBVTT\n\n00:00:05.000 --> 00:00:15.000\nB\n\n00:00:00.000 --> 00:00:10.000\nA\n\n"
    );
}

#[test]
fn time_formats_at_the_limits() {
    let cues = [
        cue(0, MAX_MS, vec![plain("x")]),
        cue(3_661_005, 3_661_006, vec![plain("y")]),
    ];

    let (s, _) = srt(&cues);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 99:59:59,999\r\nx\r\n\r\n\
         2\r\n01:01:01,005 --> 01:01:01,006\r\ny\r\n\r\n"
    );
    let (v, _) = vtt(&cues);
    assert_eq!(
        v,
        "WEBVTT\n\n00:00:00.000 --> 99:59:59.999\nx\n\n\
         01:01:01.005 --> 01:01:01.006\ny\n\n"
    );
}

#[test]
fn sort_is_stable_on_equal_starts() {
    let mut cues = vec![
        cue(5_000, 6_000, vec![plain("b1")]),
        cue(0, 1_000, vec![plain("a")]),
        cue(5_000, 7_000, vec![plain("b2")]),
        cue(5_000, 8_000, vec![plain("b3")]),
    ];
    sort_cues(&mut cues);
    let texts: Vec<&str> = cues
        .iter()
        .map(|c| c.lines[0].runs[0].text.as_str())
        .collect();
    assert_eq!(texts, ["a", "b1", "b2", "b3"]);

    // Small inputs sort the same whether or not the sort is stable, so a long
    // run with three shared starts is needed to catch an unstable sort.
    const COUNT: u64 = 300;
    let mut many: Vec<Cue> = (0..COUNT)
        .map(|i| cue((i % 3) * 1_000, 0, vec![plain(&i.to_string())]))
        .collect();
    sort_cues(&mut many);
    let order: Vec<u64> = many
        .iter()
        .map(|c| c.lines[0].runs[0].text.parse().expect("a number"))
        .collect();
    // Stable: within one start, the original (numeric) order stays.
    let mut expected: Vec<u64> = (0..COUNT).collect();
    expected.sort_by_key(|i| i % 3);
    assert_eq!(order, expected);
}

#[test]
fn empty_input_list_writes_only_the_header() {
    let (v, losses) = vtt(&[]);
    assert_eq!(v, "WEBVTT\n\n");
    assert!(losses.is_empty());

    // An SRT has no header: no cues, no bytes (not even the BOM).
    let (s, losses) = srt(&[]);
    assert_eq!(s, "");
    assert!(losses.is_empty());
}

#[test]
fn line_breaks_in_text_become_spaces() {
    // A blank line would end the cue early, so a break inside a run is a space.
    let cues = [cue(0, 1_000, vec![plain("a\n\nb")])];
    let (s, _) = srt(&cues);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\na  b\r\n\r\n"
    );
    let (v, _) = vtt(&cues);
    assert_eq!(v, "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\na  b\n\n");
}

/// The text of each line of a cue, runs joined.
fn texts(cue: &Cue) -> Vec<String> {
    cue.lines
        .iter()
        .map(|l| l.runs.iter().map(|r| r.text.as_str()).collect())
        .collect()
}

/// A line of white space only is no text. The SRT reader trims each line, so
/// it would read a tab or a no-break space as a blank line, end the cue there
/// and lose the lines after it. Both writers drop such a line, and a cue of
/// only such lines is dropped as an empty cue.
#[test]
fn whitespace_only_line_is_dropped_in_srt_and_vtt() {
    let cues = [
        cue(
            0,
            1_000,
            vec![plain("a"), plain("\t"), plain("\u{a0}"), plain("b")],
        ),
        cue(1_000, 2_000, vec![plain(" \r\n "), plain("\u{3000}")]),
    ];
    let (s, _) = srt(&cues);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\na\r\nb\r\n\r\n"
    );
    let (v, _) = vtt(&cues);
    assert_eq!(v, "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\na\nb\n\n");

    // Read back, "b" is still in the cue and nothing is skipped.
    let back = read_srt(s.as_bytes()).expect("the SRT output reads back");
    assert_eq!(back.skipped.count, 0);
    assert_eq!(back.cues.len(), 1);
    assert_eq!(texts(&back.cues[0]), ["a", "b"]);
    let back = read_vtt(v.as_bytes()).expect("the WebVTT output reads back");
    assert_eq!(back.skipped.count, 0);
    assert_eq!(back.cues.len(), 1);
    assert_eq!(texts(&back.cues[0]), ["a", "b"]);
}

/// A NUL cannot be shown, and WebVTT's parse rule reads it as U+FFFD, so both
/// writers write U+FFFD for it. Written raw, a WebVTT NUL would read back
/// changed.
#[test]
fn nul_is_written_as_the_replacement_character() {
    let cues = [cue(0, 1_000, vec![plain("x\0y")])];
    let (s, _) = srt(&cues);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\nx\u{fffd}y\r\n\r\n"
    );
    let (v, _) = vtt(&cues);
    assert_eq!(v, "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nx\u{fffd}y\n\n");
}

/// Text the SRT reader keeps as text gets no look-alike: a `<` followed by a
/// URL or an address is not a tag for the reader, so nothing is written
/// differently and no `Brackets` loss is reported.
#[test]
fn angle_text_that_is_not_a_tag_is_written_as_it_is() {
    let cues = [cue(
        0,
        1_000,
        vec![plain("Sync by <www.addic7ed.com> <a-b> <3")],
    )];
    let (s, losses) = srt(&cues);
    assert!(
        s.contains("Sync by <www.addic7ed.com> <a-b> <3\r\n"),
        "{s:?}"
    );
    assert!(losses.is_empty());
}

/// A bare name that ends a line, with white space after it, is text: the
/// reader trims the line's end, so no `>` follows the name. It is written as it
/// is and reads back as the same text (the fuzz seed `fuzz/corpus/srt/97e6a1052bcc3c2ebcf1ea77a059f14908ea946b` is this case).
#[test]
fn bare_angle_name_before_trailing_space_reads_back_as_text() {
    let cues = [cue(0, 1_000, vec![plain("a <u\u{b}")])];
    let (s, losses) = srt(&cues);
    assert!(losses.is_empty());
    let back = read_srt(s.as_bytes()).expect("the SRT output reads back");
    assert_eq!(back.cues[0].lines[0].runs[0].text, "a <u");
}

/// Text that the SRT reader would take for a tag is written with a look-alike
/// (U+FF1C), so the file reads back with the text and no markup. The VTT text
/// `a &lt;b&gt; c` holds the literal `<b>`.
#[test]
fn literal_tag_like_text_is_not_read_back_as_markup() {
    let parsed = read_vtt(b"WEBVTT\n\n00:00:01.000 --> 00:00:02.000\na &lt;b&gt; c\n\n")
        .expect("the input has a cue");
    let mut out = Vec::new();
    let losses = write_srt(&parsed.cues, &mut out).expect("writing to a Vec cannot fail");
    assert_eq!(losses, BTreeMap::from([(Loss::Brackets, 1)]));
    let back = read_srt(&out).expect("the SRT output reads back");
    assert_eq!(back.skipped.count, 0);
    assert_eq!(
        back.cues[0].lines,
        vec![Line {
            voice: None,
            runs: vec![run("a \u{ff1c}b> c")],
        }]
    );
    // A `<` that ends a run is judged on the whole plain text. No `>` follows
    // it, so it is text, and the next run's `<i>` is the only tag the reader
    // sees after it: nothing is rewritten and `b` stays italic.
    let split = |second: &str| {
        [cue(
            0,
            1_000,
            vec![Line {
                voice: None,
                runs: vec![
                    run("a <"),
                    Run {
                        italic: true,
                        ..run(second)
                    },
                ],
            }],
        )]
    };
    let (s, losses) = srt(&split("b"));
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\na <<i>b</i>\r\n\r\n"
    );
    assert!(losses.is_empty());
    // With a `>` in the next run, the `<` opens a tag and is the look-alike.
    let (s, losses) = srt(&split("b>"));
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\na \u{ff1c}<i>b></i>\r\n\r\n"
    );
    assert_eq!(losses, BTreeMap::from([(Loss::Brackets, 1)]));
}

/// Only a `<` that opens a tag (`<` then a letter, or `</` then a letter) is
/// changed; `>`, `<` before a space, a digit or a sign stay as they are.
#[test]
fn non_tag_angle_bracket_is_left_alone() {
    let cues = [cue(
        0,
        1_000,
        vec![plain("a < b"), plain("<3 love > you"), plain("x <- y >")],
    )];
    let (s, losses) = srt(&cues);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\na < b\r\n<3 love > you\r\nx <- y >\r\n\r\n"
    );
    assert!(losses.is_empty());
}

/// A `<` in a later run is judged at its own place in the line. The first run
/// has text, so the `<b` of the second run starts at byte 2, not at 0.
#[test]
fn bracket_in_a_later_run_is_judged_at_its_place() {
    let line = Line {
        voice: None,
        runs: vec![run("a "), run("<b>c")],
    };
    let (s, losses) = srt(&[cue(0, 1_000, vec![line])]);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\na \u{ff1c}b>c\r\n\r\n"
    );
    assert_eq!(losses, BTreeMap::from([(Loss::Brackets, 1)]));
}

/// The look-alike is counted as a loss once per cue, however many `<` the cue
/// has; a cue with none adds nothing.
#[test]
fn srt_writer_reports_brackets_loss_once_per_cue() {
    let cues = [
        cue(0, 1_000, vec![plain("<a"), plain("<b>"), plain("c")]),
        cue(1_000, 2_000, vec![plain("<c>")]),
        cue(2_000, 3_000, vec![plain("no brackets")]),
    ];
    let (_, losses) = srt(&cues);
    assert_eq!(losses, BTreeMap::from([(Loss::Brackets, 2)]));
}

/// Text that the SRT reader would take for an override block (`{\` then a
/// closing `}`) is written with a look-alike (U+FF5B), so the file reads back
/// with the text and no style applied. The VTT text `a {\b1} c` holds the
/// literal block.
#[test]
fn literal_override_like_text_is_not_read_back_as_override() {
    let parsed = read_vtt(b"WEBVTT\n\n00:00:01.000 --> 00:00:02.000\na {\\b1} c\n\n")
        .expect("the input has a cue");
    let mut out = Vec::new();
    let losses = write_srt(&parsed.cues, &mut out).expect("writing to a Vec cannot fail");
    assert_eq!(losses, BTreeMap::from([(Loss::Brackets, 1)]));
    let back = read_srt(&out).expect("the SRT output reads back");
    assert_eq!(back.skipped.count, 0);
    assert_eq!(
        back.cues[0].lines,
        vec![Line {
            voice: None,
            runs: vec![run("a \u{ff5b}\\b1} c")],
        }]
    );
}

/// The `{\an8}` that the writer puts before a top cue's first line is its own
/// marker, not text: it stays. Text that looks like the same marker is written
/// with the look-alike.
#[test]
fn own_top_marker_is_not_rewritten() {
    let mut top = cue(0, 1_000, vec![plain("x")]);
    top.top = true;
    let (s, losses) = srt(&[top]);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\n{\\an8}x\r\n\r\n"
    );
    assert!(losses.is_empty());

    let mut literal = cue(0, 1_000, vec![plain("{\\an8}y")]);
    literal.top = true;
    let (s, losses) = srt(&[literal]);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\n{\\an8}\u{ff5b}\\an8}y\r\n\r\n"
    );
    assert_eq!(losses, BTreeMap::from([(Loss::Brackets, 1)]));
}

/// A cue with both a tag-like `<` and an override-like `{\` counts once, as
/// one look-alike cue; each cue counts once however many it has.
#[test]
fn cue_with_both_brackets_counts_once() {
    let cues = [
        cue(0, 1_000, vec![plain("<b"), plain("{\\b1}x")]),
        cue(1_000, 2_000, vec![plain("{\\i1}")]),
    ];
    let (_, losses) = srt(&cues);
    assert_eq!(losses, BTreeMap::from([(Loss::Brackets, 2)]));
}

/// A `{` that does not open an override block (no backslash after it) stays,
/// and so does one with no closing `}` after a backslash only as text.
#[test]
fn plain_brace_is_left_alone() {
    let cues = [cue(
        0,
        1_000,
        vec![plain("{"), plain("{x}"), plain("a { b")],
    )];
    let (s, losses) = srt(&cues);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\n{\r\n{x}\r\na { b\r\n\r\n"
    );
    assert!(losses.is_empty());
}

/// A text line the SRT reader would take for a timing line starts a cue on
/// read back, and its text is lost. The writer writes the `>` of its `-->` as
/// U+FF1E, so the text reads back, in one cue. The SRT input holds the text
/// behind a tag, which the reader keeps as text.
#[test]
fn timing_shaped_text_line_is_not_read_back_as_a_cue() {
    let parsed =
        read_srt(b"1\n00:00:01,000 --> 00:00:02,000\n<font>1:2:3,599 --> 99:59:59,999</font>\n\n")
            .expect("the input has a cue");
    let mut out = Vec::new();
    let losses = write_srt(&parsed.cues, &mut out).expect("writing to a Vec cannot fail");
    assert_eq!(losses, BTreeMap::from([(Loss::Brackets, 1)]));
    let back = read_srt(&out).expect("the SRT output reads back");
    assert_eq!(back.skipped.count, 0);
    assert_eq!(back.cues.len(), 1);
    assert_eq!(
        back.cues[0].lines,
        vec![Line {
            voice: None,
            runs: vec![run("1:2:3,599 --\u{ff1e} 99:59:59,999")],
        }]
    );
    // The writer's own timing line is not rewritten.
    assert!(
        String::from_utf8(out)
            .expect("UTF-8")
            .contains("\r\n00:00:01,000 --> 00:00:02,000\r\n")
    );
}

/// A cue with several timing-shaped text lines counts once, as the other
/// look-alikes do; each cue counts once.
#[test]
fn timing_shaped_text_counts_one_loss_per_cue() {
    let cues = [
        cue(
            0,
            1_000,
            vec![
                plain("1:2:3,599 --> 99:59:59,999"),
                plain("00:00:05,000 --> 00:00:06,000"),
            ],
        ),
        cue(1_000, 2_000, vec![plain("00:00:07,000 --> 00:00:08,000")]),
    ];
    let (_, losses) = srt(&cues);
    assert_eq!(losses, BTreeMap::from([(Loss::Brackets, 2)]));
}

/// Text that is not a timing line keeps its arrows, and so does a text line
/// that a top cue's `{\an8}` marker makes non-timing: the marker is part of the
/// line the reader gets.
#[test]
fn ordinary_arrow_text_is_left_alone() {
    let cues = [cue(
        0,
        1_000,
        vec![
            plain("a --> b"),
            plain("00:00:01,000"),
            plain("-->"),
            plain("00:00:01,000 -->"),
        ],
    )];
    let (s, losses) = srt(&cues);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\na --> b\r\n00:00:01,000\r\n-->\r\n00:00:01,000 -->\r\n\r\n"
    );
    assert!(losses.is_empty());

    let mut top = cue(0, 1_000, vec![plain("1:2:3,599 --> 99:59:59,999")]);
    top.top = true;
    let (s, losses) = srt(&[top]);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\n{\\an8}1:2:3,599 --> 99:59:59,999\r\n\r\n"
    );
    assert!(losses.is_empty());
}

/// A `<` with no `>` after it in the line is text for the SRT reader, so the
/// writer leaves it: VTT `if x&lt;y then` is `if x<y then` in SRT, with no
/// look-alike and no `Brackets` loss, and it reads back the same.
#[test]
fn angle_with_no_closing_bracket_is_not_rewritten() {
    let parsed = read_vtt(b"WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nif x&lt;y then\n\n")
        .expect("the input has a cue");
    let mut out = Vec::new();
    let losses = write_srt(&parsed.cues, &mut out).expect("writing to a Vec cannot fail");
    assert!(losses.is_empty());
    assert!(out.ends_with(b"if x<y then\r\n\r\n"), "{out:?}");
    let back = read_srt(&out).expect("the SRT output reads back");
    assert_eq!(back.cues[0].lines[0].runs[0].text, "if x<y then");
}

/// A `<` before another `<` is text for the reader, and the later `<` that
/// opens a tag is rewritten. That rewrite leaves the earlier `<` the next
/// bracket the reader sees, so `<b <c>` rewrites both, and reads back as text.
#[test]
fn angle_before_another_angle_is_rewritten_with_the_tag_it_would_join() {
    let (s, losses) = srt(&[cue(0, 1_000, vec![plain("<b <c>")])]);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\n\u{ff1c}b \u{ff1c}c>\r\n\r\n"
    );
    assert_eq!(losses, BTreeMap::from([(Loss::Brackets, 1)]));
    let back = read_srt(s.as_bytes()).expect("the SRT output reads back");
    assert_eq!(back.cues[0].lines[0].runs[0].text, "\u{ff1c}b \u{ff1c}c>");
}

/// An earlier `<` with a later `<` (and no `>` after that one) is text for the
/// reader, so nothing in `a <b y< z>` is rewritten.
#[test]
fn angle_before_text_with_no_closing_bracket_is_not_rewritten() {
    let (s, losses) = srt(&[cue(0, 1_000, vec![plain("a <b y< z>")])]);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\na <b y< z>\r\n\r\n"
    );
    assert!(losses.is_empty());
}

/// A `{\` with no `}` after it is text for the reader; a closed one is an
/// override block and is rewritten.
#[test]
fn unclosed_override_is_text_and_a_closed_one_is_rewritten() {
    let (s, losses) = srt(&[cue(0, 1_000, vec![plain("a {\\x")])]);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\na {\\x\r\n\r\n"
    );
    assert!(losses.is_empty());
    let (s, losses) = srt(&[cue(0, 1_000, vec![plain("a {\\x}")])]);
    assert_eq!(
        s,
        "\u{feff}1\r\n00:00:00,000 --> 00:00:01,000\r\na \u{ff5b}\\x}\r\n\r\n"
    );
    assert_eq!(losses, BTreeMap::from([(Loss::Brackets, 1)]));
}

/// Every line of one to six characters over brackets, a slash, `b` and a space
/// (no timing line can come of it: there is no `-->`), except those the reader
/// trims. The SRT output reads back as the same text, where each change is a
/// look-alike for a `<` or `{`, and a line the reader keeps as plain text is
/// written unchanged with no `Brackets` loss. Covers `<b <b>` and `<b y< z>`.
#[test]
fn every_short_line_reads_back_and_only_real_markup_is_rewritten() {
    const ALPHABET: [char; 8] = ['<', '>', '{', '\\', '}', '/', 'b', ' '];
    const MAX_CHARS: usize = 6;
    let mut level = vec![String::new()];
    let mut lines = Vec::new();
    for _ in 0..MAX_CHARS {
        level = level
            .iter()
            .flat_map(|s| ALPHABET.iter().map(move |c| format!("{s}{c}")))
            .collect();
        lines.extend(level.iter().cloned());
    }
    for t in lines
        .iter()
        .filter(|t| !t.starts_with(' ') && !t.ends_with(' '))
    {
        let (s, losses) = srt(&[cue(0, 1_000, vec![plain(t)])]);
        let back = read_srt(s.as_bytes()).expect("the SRT output reads back");
        assert_eq!(back.skipped.count, 0, "{t:?}: {s:?}");
        let [only] = back.cues.as_slice() else {
            panic!("{t:?}: {s:?}")
        };
        let [line] = only.lines.as_slice() else {
            panic!("{t:?}: {s:?}")
        };
        let [run] = line.runs.as_slice() else {
            panic!("{t:?}: {s:?}")
        };
        assert!(!run.italic && !run.bold && !run.underline, "{t:?}: {s:?}");
        assert_eq!(run.text.chars().count(), t.chars().count(), "{t:?}: {s:?}");
        let mut rewritten = false;
        for (want, got) in t.chars().zip(run.text.chars()) {
            match (want, got) {
                (w, g) if w == g => {}
                ('<', '\u{ff1c}') | ('{', '\u{ff5b}') => rewritten = true,
                _ => panic!("{t:?} reads back as {:?}: {s:?}", run.text),
            }
        }
        assert_eq!(
            losses.contains_key(&Loss::Brackets),
            rewritten,
            "{t:?}: {s:?}"
        );
        // The reader's own view of the line as it is: when it keeps the text
        // whole, the writer has nothing to change.
        let raw = format!("1\r\n00:00:00,000 --> 00:00:01,000\r\n{t}\r\n\r\n");
        if let Ok(parsed) = read_srt(raw.as_bytes())
            && let [only] = parsed.cues.as_slice()
            && let [line] = only.lines.as_slice()
            && let [run] = line.runs.as_slice()
            && run.text == *t
            && !run.italic
            && !run.bold
            && !run.underline
        {
            assert!(!rewritten, "{t:?} is plain text to the reader: {s:?}");
        }
    }
}
