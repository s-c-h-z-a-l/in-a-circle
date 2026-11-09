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

    // SRT text is written as it is; its readers do not read markup escapes.
    let (s, _) = srt(&cues);
    assert!(s.contains(&format!("\r\n{text}\r\n")), "{s:?}");
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
