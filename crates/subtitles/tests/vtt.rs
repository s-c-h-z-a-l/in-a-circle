//! WebVTT reader: the standard's rules, the web-platform-tests files in
//! `tests/data/wpt-webvtt` (checked against `expected.tsv` and the cue counts
//! their HTML asserts), the limits, and the linear-time guard.

use std::collections::BTreeMap;
use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use subtitles::{
    Cue, INPUT_BYTES, Line, Loss, MAX_MS, Parsed, ReadError, Run, Skipped, read_vtt, write_vtt,
};

fn read(text: &str) -> Parsed {
    read_vtt(text.as_bytes()).expect("the input has a usable cue")
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

fn run(text: &str, italic: bool) -> Run {
    Run {
        text: text.into(),
        italic,
        bold: false,
        underline: false,
    }
}

/// One row of `expected.tsv`: the cue count, the first cue, and the reason
/// for any difference from the web-platform-tests count.
struct Row {
    count: usize,
    first: Option<(u64, u64, String)>,
    note: String,
}

/// `expected.tsv`: file, count, first start and end in ms, first text, note.
/// The first line is the header; an empty count-0 row has empty first fields.
fn table(dir: &Path) -> BTreeMap<String, Row> {
    let text = fs::read_to_string(dir.join("expected.tsv")).expect("the expected table");
    text.lines()
        .skip(1)
        .map(|line| {
            let f: Vec<&str> = line.splitn(6, '\t').collect();
            assert_eq!(f.len(), 6, "six columns: {line:?}");
            let first = (!f[2].is_empty()).then(|| {
                (
                    f[2].parse().expect("a start in ms"),
                    f[3].parse().expect("an end in ms"),
                    f[4].to_owned(),
                )
            });
            let row = Row {
                count: f[1].parse().expect("a count"),
                first,
                note: f[5].to_owned(),
            };
            (f[0].to_owned(), row)
        })
        .collect()
}

/// The cue count the file's HTML test asserts, when it asserts one.
fn wpt_count(html: &str) -> Option<usize> {
    let rest = html.split("assert_equals(cues.length, ").nth(1)?;
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

#[test]
fn wpt_files_parse_to_the_expected_cues() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/wpt-webvtt");
    let mut rows = table(&dir);
    let mut files = 0;
    for entry in fs::read_dir(&dir).expect("the data folder") {
        let path = entry.expect("a readable entry").path();
        if path.extension().is_none_or(|x| x != "vtt") {
            continue;
        }
        let name = path
            .file_stem()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        let bytes = fs::read(&path).expect("a readable data file");
        let cues = match read_vtt(&bytes) {
            Ok(parsed) => parsed.cues,
            Err(ReadError::NoCues) => Vec::new(),
            Err(e) => panic!("{name}: {e}"),
        };
        for cue in &cues {
            assert!(
                cue.start_ms <= cue.end_ms && cue.end_ms <= MAX_MS,
                "{name}: a cue span is out of order or past the limit"
            );
        }
        let row = rows
            .remove(&name)
            .unwrap_or_else(|| panic!("{name}: no row in expected.tsv"));
        assert_eq!(cues.len(), row.count, "{name}: cue count");
        let first = cues.first().map(|c| (c.start_ms, c.end_ms, text_of(c)));
        assert_eq!(first, row.first, "{name}: first cue");
        let html = fs::read_to_string(path.with_extension("html")).expect("the test's HTML");
        // A count that differs from the test's must be explained in the note.
        if let Some(wpt) = wpt_count(&html).filter(|&wpt| wpt != row.count) {
            assert!(
                row.note.contains(&format!("WPT: {wpt}")),
                "{name}: {} cues against the test's {wpt}, and the note does not say why",
                row.count
            );
        }
        files += 1;
    }
    assert!(rows.is_empty(), "rows without a file: {:?}", rows.keys());
    // The vendored set: 42 files from the pinned commit, as the README says.
    assert_eq!(files, 42);
}

#[test]
fn header_with_bom_and_trailing_text() {
    // The byte order mark is skipped, text after the signature on its line is
    // ignored, and metadata lines in the header are neither cues nor reported.
    let parsed = read(
        "\u{feff}WEBVTT - a title\nKind: captions\nLanguage: en\n\n00:00:01.000 --> 00:00:02.000\nhello\n",
    );
    assert_eq!(texts(&parsed), vec!["hello"]);
    assert_eq!(parsed.skipped, Skipped::default());
    // The signature ends at a space, a tab, a line end or the end of the file.
    assert_eq!(
        read_vtt(b"WEBVTTX\n\n00:00:01.000 --> 00:00:02.000\nhi\n"),
        Err(ReadError::NoCues)
    );
    assert_eq!(
        read_vtt(b"WEBVTT\t\n00:00:01.000 --> 00:00:02.000\nhi\n")
            .expect("a tab ends the signature")
            .cues
            .len(),
        1
    );
}

#[test]
fn note_style_region_blocks_are_skipped() {
    let parsed = read(
        "WEBVTT\n\nNOTE a comment\nover two lines\n\n00:00:01.000 --> 00:00:02.000\nfirst\n\nSTYLE\n::cue { color: lime }\n\nREGION\nid:r1\n\n00:00:03.000 --> 00:00:04.000\nsecond\n",
    );
    assert_eq!(texts(&parsed), vec!["first", "second"]);
    // The first cue came before the style and region blocks, so only the
    // second loses its styling. Comments are not losses.
    assert_eq!(parsed.losses, losses(&[(Loss::Settings, 1)]));
    assert_eq!(parsed.skipped, Skipped::default());
}

#[test]
fn line_zero_is_top_line_fifty_percent_is_not() {
    // Rows 0 to 4 and percentages up to 33 are the top third: a top cue. Any
    // other valid placement is a Position loss, once per cue.
    let cases = [
        ("0", true),
        ("-0", true),
        ("4", true),
        ("5", false),
        ("-1", false),
        ("1.5", false),
        ("0%", true),
        ("33%", true),
        ("34%", false),
        ("50%", false),
        ("100%", false),
    ];
    for (value, top) in cases {
        let parsed = read(&format!(
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000 line:{value}\ntext\n"
        ));
        assert_eq!(parsed.cues[0].top, top, "line:{value}");
        let position = usize::from(!top);
        assert_eq!(
            parsed.losses.get(&Loss::Position).copied().unwrap_or(0),
            position,
            "line:{value}"
        );
    }
    // A value the standard does not accept is ignored: no top, no loss.
    let parsed = read("WEBVTT\n\n00:00:01.000 --> 00:00:02.000 line:1e2 line:%\ntext\n");
    assert!(!parsed.cues[0].top);
    assert!(parsed.losses.is_empty());
    // Several placement settings in one cue are one loss.
    let parsed =
        read("WEBVTT\n\n00:00:01.000 --> 00:00:02.000 position:10% align:start size:50%\ntext\n");
    assert_eq!(parsed.losses, losses(&[(Loss::Position, 1)]));
}

#[test]
fn line_value_with_alignment_or_decimal_percentage() {
    // `line:<number>[,<align>]` and `line:<percentage>[,<align>]`, where the
    // percentage may have a fraction. The top test is the same as for a plain
    // value; the alignment does not move the cue. Each case is (top, Position
    // losses) for a cue with that one setting.
    let placed = |setting: &str| {
        let parsed = read(&format!(
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000 {setting}\ntext\n"
        ));
        (
            parsed.cues[0].top,
            parsed.losses.get(&Loss::Position).copied().unwrap_or(0),
        )
    };
    assert_eq!(placed("line:0,start"), (true, 0));
    assert_eq!(placed("line:4,center"), (true, 0));
    assert_eq!(placed("line:10.5%"), (true, 0));
    assert_eq!(placed("line:33.0%"), (true, 0));
    assert_eq!(placed("line:90%,end"), (false, 1));
    assert_eq!(placed("line:100%,start"), (false, 1));
    assert_eq!(placed("line:33.4%"), (false, 1));
    assert_eq!(placed("line:-1"), (false, 1));
    assert_eq!(placed("line:0%"), (true, 0));
    assert_eq!(placed("line:100%"), (false, 1));
    // Not valid: ignored, with no top and no loss.
    for invalid in [
        "line:abc",
        "line:,start",
        "line:0,",
        "line:0,middle",
        "line:0,start,end",
        "line:10.%",
        "line:.5%",
        "line:-5%",
    ] {
        assert_eq!(placed(invalid), (false, 0), "{invalid}");
    }
}

#[test]
fn voice_tag_sets_the_line_voice() {
    let parsed = read(
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n<v Anna>hi</v>\n\n00:00:03.000 --> 00:00:04.000\n<v Bob Smith>one\ntwo</v>\n\n00:00:05.000 --> 00:00:06.000\n<v Cy>hello</v> there\n",
    );
    let voiced = |cue: &Cue| -> Vec<(Option<String>, String)> {
        cue.lines
            .iter()
            .map(|l| {
                let text: String = l.runs.iter().map(|r| r.text.as_str()).collect();
                (l.voice.as_deref().map(String::from), text)
            })
            .collect()
    };
    // The speaker carries across the lines of a cue until </v>.
    assert_eq!(
        voiced(&parsed.cues[0]),
        vec![(Some("Anna".into()), "hi".into())]
    );
    assert_eq!(
        voiced(&parsed.cues[1]),
        vec![
            (Some("Bob Smith".into()), "one".into()),
            (Some("Bob Smith".into()), "two".into()),
        ]
    );
    // Text after </v> on the same line has no speaker, so it is a line of its own.
    assert_eq!(
        voiced(&parsed.cues[2]),
        vec![(Some("Cy".into()), "hello".into()), (None, " there".into())]
    );
    assert!(parsed.losses.is_empty());
}

#[test]
fn voice_classes_are_not_part_of_the_name() {
    // The classes (`.a.b`) follow the tag name and end at the first white
    // space; the speaker is what follows that. A `v` with classes and no name
    // has no speaker.
    let parsed = read(
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n<v.loud Esme>It</v>\n\n00:00:03.000 --> 00:00:04.000\n<v Esme>It</v>\n\n00:00:05.000 --> 00:00:06.000\n<v.a.b Name With Spaces>It</v>\n\n00:00:07.000 --> 00:00:08.000\n<v.loud>It</v>\n\n00:00:09.000 --> 00:00:10.000\n<c.yellow>styled</c>\n",
    );
    let voices: Vec<Option<String>> = parsed
        .cues
        .iter()
        .map(|c| c.lines[0].voice.as_deref().map(String::from))
        .collect();
    assert_eq!(
        voices,
        vec![
            Some("Esme".into()),
            Some("Esme".into()),
            Some("Name With Spaces".into()),
            None,
            None,
        ]
    );
    assert_eq!(texts(&parsed), vec!["It", "It", "It", "It", "styled"]);
    // Only `<c.class>` is reported as styling; the classes of `<v>` are not.
    assert_eq!(parsed.losses, losses(&[(Loss::Styling, 1)]));
}

#[test]
fn karaoke_timestamp_tags_keep_text_and_report() {
    let parsed = read(
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n<00:00:01.250>one <00:00:01.500>two\n\n00:00:03.000 --> 00:00:04.000\n<c.yellow>styled</c> <c>plain</c> <lang en>x</lang>\n",
    );
    assert_eq!(texts(&parsed), vec!["one two", "styled plain x"]);
    // Karaoke is counted once per cue, however many timestamp tags it has;
    // a plain <c> has nothing to report.
    assert_eq!(
        parsed.losses,
        losses(&[(Loss::Karaoke, 1), (Loss::Styling, 1)])
    );
}

#[test]
fn textless_cue_with_only_styling_reports_nothing() {
    // Decision: only a loss that removed text counts for a cue with no text.
    // Styling, karaoke, position and the settings of a STYLE block removed no
    // text, so these cues are dropped silently. Only the first cue is kept.
    let text = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nkept\n\n\
        STYLE\n::cue { color: red }\n\n\
        00:00:03.000 --> 00:00:04.000\n<c.red></c>\n\n\
        00:00:05.000 --> 00:00:06.000 line:50%\n<00:00:05.500>\n\n\
        00:00:07.000 --> 00:00:08.000 position:10%\n<v Anna></v>\n\n";
    let parsed = read(text);
    assert_eq!(texts(&parsed), vec!["kept"]);
    assert!(parsed.losses.is_empty());
    assert_eq!(parsed.skipped, Skipped::default());
    assert_eq!(
        read_vtt("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n<c.red></c>\n".as_bytes()),
        Err(ReadError::NoCues)
    );
}

#[test]
fn empty_cue_alone_is_silent() {
    // A cue with no text at all is not a loss: nothing is counted or skipped.
    let parsed =
        read("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n\n00:00:03.000 --> 00:00:04.000\nkept\n");
    assert_eq!(texts(&parsed), vec!["kept"]);
    assert!(parsed.losses.is_empty());
    assert_eq!(parsed.skipped, Skipped::default());
}

#[test]
fn drawing_syntax_is_text_in_webvtt() {
    // WebVTT has no drawing commands, so `{\p1}` is text: the cue is kept and
    // nothing is counted. The drawing rule of the ASS and SRT readers does not
    // apply here.
    let parsed = read("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n{\\p1}m 0 0 l 1 1{\\p0}\n");
    assert_eq!(texts(&parsed), vec!["{\\p1}m 0 0 l 1 1{\\p0}"]);
    assert!(parsed.losses.is_empty());
}

#[test]
fn entities_are_decoded() {
    let parsed = read(
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n&amp; &lt;i&gt; &lrm;&rlm;&nbsp;x &unknown; & &amp\n",
    );
    assert_eq!(
        texts(&parsed),
        vec!["& <i> \u{200e}\u{200f}\u{a0}x &unknown; & &amp"]
    );
    // The decoded `<i>` is text, not a tag.
    assert!(parsed.cues[0].lines[0].runs.iter().all(|r| !r.italic));
}

#[test]
fn numeric_character_references_are_decoded() {
    // Decimal and hex references to a valid character decode, as do the quote
    // entities. A decoded `<` is text, not a tag.
    let parsed = read(
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nIt&#39;s &quot;me&quot; &#x1F600; &#X41; &#60;i&#62; &apos;&#00000065;\n",
    );
    assert_eq!(texts(&parsed), vec!["It's \"me\" \u{1f600} A <i> 'A"]);
    assert!(parsed.cues[0].lines[0].runs.iter().all(|r| !r.italic));
    // `&amp;` is the one `&` that decodes, so the reference after it is text.
    let parsed = read("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n&amp;#39;\n");
    assert_eq!(texts(&parsed), vec!["&#39;"]);
}

#[test]
fn references_that_are_not_valid_stay_as_written() {
    // NUL, a surrogate, a value past U+10FFFF, a line break (the readers split
    // lines at them), a missing digit or `;`, nine digits (past the cap), and a
    // name that is not in the set: each stays as the text it was.
    for literal in [
        "&#0;",
        "&#xD800;",
        "&#x110000;",
        "&#10;",
        "&#xD;",
        "&#;",
        "&#x;",
        "&#x41",
        "&#000000065;",
        "&unknown;",
    ] {
        let parsed = read(&format!(
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n{literal}\n"
        ));
        assert_eq!(texts(&parsed), vec![literal], "{literal}");
    }
}

#[test]
fn nul_in_cue_text_is_the_replacement_character() {
    // The standard replaces each NUL in cue text with U+FFFD. The WPT table
    // checks first cues only, so the rule is pinned here on a later cue.
    let parsed = read(
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nfirst\n\n00:00:03.000 --> 00:00:04.000\n\u{0}text\u{0}2\n",
    );
    assert_eq!(texts(&parsed)[1], "\u{fffd}text\u{fffd}2");
}

#[test]
fn bad_cue_in_the_middle_is_skipped_rest_converts() {
    // 100 cues of three lines each (timing, text, empty), after the header and
    // its empty line. Cue 40 has `;` for the dot (a bad timestamp), cue 60
    // ends before it starts, and cue 80 starts at 100 hours.
    let mut file = String::from("WEBVTT\n\n");
    for i in 1..=100 {
        let timing = match i {
            40 => "00:00;000 --> 00:00:02.000",
            60 => "00:00:05.000 --> 00:00:04.000",
            80 => "100:00:00.000 --> 100:00:01.000",
            _ => "00:00:01.000 --> 00:00:02.000",
        };
        file += &format!("{timing}\ncue {i}\n\n");
    }
    let parsed = read(&file);
    let expected: Vec<String> = (1..=100)
        .filter(|i| ![40, 60, 80].contains(i))
        .map(|i| format!("cue {i}"))
        .collect();
    assert_eq!(texts(&parsed), expected);
    // Cue 40's timing is the 120th line: the header is lines 1 and 2, and
    // each cue takes three lines.
    let bad_line = 3 + 39 * 3;
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 3,
            first: Some((bad_line, "bad timestamp")),
        }
    );
}

#[test]
fn end_at_100_hours_is_skipped_and_99_59_59_999_is_kept() {
    // The end is checked against the limit as the start is: 100:00:00.000 is
    // past it, and the limit itself (99:59:59.999) is still a cue.
    let parsed = read(
        "WEBVTT\n\n00:00:01.000 --> 100:00:00.000\nlate end\n\n00:00:02.000 --> 99:59:59.999\nlast\n",
    );
    assert_eq!(texts(&parsed), vec!["last"]);
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 1,
            first: Some((3, "time is 100 hours or more")),
        }
    );
}

#[test]
fn timing_line_ends_an_ignored_block_without_a_blank_line() {
    // A STYLE, REGION or NOTE block runs to a blank line. Without one, a valid
    // timing line still starts a cue, so the cue's text is not lost (a
    // deliberate deviation from the standard's block rule). The block's own
    // lines keep their meaning: STYLE and REGION are Settings, NOTE is silent.
    let style = read(
        "WEBVTT\n\nSTYLE\n::cue { color: lime }\n00:00:01.000 --> 00:00:02.000\nhello\n\n00:00:03.000 --> 00:00:04.000\nworld\n",
    );
    assert_eq!(texts(&style), vec!["hello", "world"]);
    assert_eq!(style.losses, losses(&[(Loss::Settings, 2)]));
    assert_eq!(style.skipped, Skipped::default());

    let region = read(
        "WEBVTT\n\nREGION\nid:r1\n00:00:01.000 --> 00:00:02.000\nhello\n\n00:00:03.000 --> 00:00:04.000\nworld\n",
    );
    assert_eq!(texts(&region), vec!["hello", "world"]);
    assert_eq!(region.losses, losses(&[(Loss::Settings, 2)]));

    let note = read(
        "WEBVTT\n\nNOTE a comment\n00:00:01.000 --> 00:00:02.000\nhello\n\n00:00:03.000 --> 00:00:04.000\nworld\n",
    );
    assert_eq!(texts(&note), vec!["hello", "world"]);
    assert!(note.losses.is_empty());
    assert_eq!(note.skipped, Skipped::default());

    // A `-->` in the block that is no timing line is part of the block: it
    // counts as nothing, and the block's cue after the blank line is read.
    let arrow_css = read(
        "WEBVTT\n\nSTYLE\n::cue { color: lime } --> x\n\n00:00:01.000 --> 00:00:02.000\nhello\n",
    );
    assert_eq!(texts(&arrow_css), vec!["hello"]);
    assert_eq!(arrow_css.losses, losses(&[(Loss::Settings, 1)]));
    assert_eq!(arrow_css.skipped, Skipped::default());

    // A block already reported (here a bad header line) does not swallow the
    // timing line after it either.
    let reported = read("WEBVTT\nbad --> x\n00:00:01.000 --> 00:00:02.000\nhello\n");
    assert_eq!(texts(&reported), vec!["hello"]);
    assert_eq!(
        reported.skipped,
        Skipped {
            count: 1,
            first: Some((2, "bad timestamp")),
        }
    );
}

#[test]
fn id_block_without_timing_is_text_outside_any_cue() {
    // A block whose first line has no `-->` is not a broken cue: it is text
    // outside any cue, reported as such when its block ends.
    let parsed = read("WEBVTT\n\nintro\n\n00:00:01.000 --> 00:00:02.000\nkept\n");
    assert_eq!(texts(&parsed), vec!["kept"]);
    assert_eq!(
        parsed.skipped,
        Skipped {
            count: 1,
            first: Some((3, "text outside any cue")),
        }
    );
}

#[test]
fn dropped_arrow_line_is_counted_however_its_block_ends() {
    // A `-->` line that is no timing line drops its block, and the block is one
    // skip. Whatever ends it counts: a timing line (the cue after it is kept),
    // a text line, a blank line or the end of the file.
    let dropped = |line: usize| Skipped {
        count: 1,
        first: Some((line, "bad timestamp")),
    };
    let timing_after = read(
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHello\nLook --> there\n00:00:03.000 --> 00:00:04.000\nWorld\n",
    );
    assert_eq!(texts(&timing_after), vec!["Hello", "World"]);
    assert_eq!(timing_after.skipped, dropped(5));

    let text_after =
        read("WEBVTT\n\nLook --> there\nmore\n\n00:00:03.000 --> 00:00:04.000\nWorld\n");
    assert_eq!(texts(&text_after), vec!["World"]);
    assert_eq!(text_after.skipped, dropped(3));

    let blank_after = read("WEBVTT\n\nLook --> there\n\n00:00:03.000 --> 00:00:04.000\nWorld\n");
    assert_eq!(texts(&blank_after), vec!["World"]);
    assert_eq!(blank_after.skipped, dropped(3));

    let at_end = read("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHi\nLook --> there");
    assert_eq!(texts(&at_end), vec!["Hi"]);
    assert_eq!(at_end.skipped, dropped(5));
}

#[test]
fn dotted_tags_other_than_c_are_not_styling() {
    // Only `<c.class>` is styling. An unknown tag with a dotted name, and a
    // closing `</c.class>`, change nothing and report nothing.
    let parsed = read(
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n<span.x>plain</span>\n\n00:00:03.000 --> 00:00:04.000\nplain</c.x>\n",
    );
    assert_eq!(texts(&parsed), vec!["plain", "plain"]);
    assert!(parsed.losses.is_empty());
}

/// One cue whose text is `unit` repeated to at least `bytes`, after `prefix`.
fn flood(prefix: &str, unit: &str, bytes: usize) -> Vec<u8> {
    let count = bytes / unit.len() + 1;
    format!("{prefix}{}\n", unit.repeat(count)).into_bytes()
}

/// The best of three timed runs, so one slow run does not decide the ratio.
fn best_of_three(input: &[u8]) -> Duration {
    (0..3)
        .map(|_| {
            let start = Instant::now();
            black_box(read_vtt(black_box(input)).is_ok());
            start.elapsed()
        })
        .min()
        .unwrap_or_default()
}

#[test]
fn vtt_linear_on_tag_floods() {
    // Each flood is read at 1 MiB and at 4 MiB, both under INPUT_BYTES. Linear
    // work makes the 4 MiB run about 4 times the 1 MiB run; a quadratic reader
    // makes it 16 times. The limit, 8 times, is halfway between on a log
    // scale, which leaves room for cache and allocator noise.
    const SMALL: usize = 1 << 20;
    const LARGE: usize = 4 << 20;
    const CUE: &str = "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\n";
    let floods = [
        ("italic tags on one line", CUE, "x<i>"),
        ("tag openers never closed", CUE, "x<a"),
        ("timestamp tags on one line", CUE, "x<00:00:01.000>"),
        ("speaker tags on one line", CUE, "x<v A>"),
        ("entities on one line", CUE, "x&amp;"),
        ("numeric references on one line", CUE, "x&#99999999"),
        (
            "numeric references with long digit runs",
            CUE,
            "&#9999999999999999999999",
        ),
        ("lone ampersands on one line", CUE, "x&"),
        (
            "minimal cues",
            "WEBVTT\n\n",
            "00:00.000 --> 00:01.000\nx\n\n",
        ),
    ];
    for (name, prefix, unit) in floods {
        let small = flood(prefix, unit, SMALL);
        let large = flood(prefix, unit, LARGE);
        assert!(read_vtt(&large).is_ok(), "{name}: the flood is usable text");
        let t_small = best_of_three(&small);
        let t_large = best_of_three(&large);
        assert!(
            t_large < t_small * 8,
            "{name}: 4x the input took {t_large:?} against {t_small:?}; linear is about 4x"
        );
    }
}

#[test]
fn reader_never_panics_on_every_prefix() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/wpt-webvtt");
    for name in [
        "nulls.vtt",
        "regions-edge-case.vtt",
        "whitespace-chars.vtt",
        "arrows.vtt",
    ] {
        let bytes = fs::read(dir.join(name)).expect("a data file");
        assert!(read_vtt(&bytes).is_ok(), "{name}: has usable cues");
        for end in 0..=bytes.len() {
            let _ = read_vtt(&bytes[..end]);
        }
        for start in 0..=bytes.len() {
            let _ = read_vtt(&bytes[start..]);
        }
    }
}

#[test]
fn round_trip_through_our_writer_keeps_the_cues() {
    // Our writer, read back by our reader: a consistency check of the two, not
    // independent evidence that either is right.
    let cues = vec![
        Cue {
            start_ms: 0,
            end_ms: 1_000,
            top: true,
            lines: vec![Line {
                voice: None,
                runs: vec![run("up ", false), run("it", true)],
            }],
        },
        Cue {
            start_ms: 1_000,
            end_ms: 2_500,
            top: false,
            lines: vec![
                Line {
                    voice: Some("Ann & Bo".into()),
                    runs: vec![run("a < b > c", false)],
                },
                Line {
                    voice: None,
                    runs: vec![
                        Run {
                            text: "x --> y".into(),
                            italic: false,
                            bold: true,
                            underline: false,
                        },
                        run(" z", false),
                    ],
                },
            ],
        },
    ];
    let mut out = Vec::new();
    write_vtt(&cues, &mut out).expect("writing to a Vec cannot fail");
    let parsed = read_vtt(&out).expect("our own output is usable");
    assert_eq!(parsed.cues, cues);
    assert_eq!(parsed.skipped, Skipped::default());
    assert!(parsed.losses.is_empty());
}

#[test]
fn input_limit_is_inclusive() {
    let head = b"WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nx\n\n";
    let mut exact = head.to_vec();
    exact.resize(INPUT_BYTES, b'\n');
    assert!(read_vtt(&exact).is_ok());
    exact.push(b'\n');
    assert_eq!(read_vtt(&exact), Err(ReadError::TooLarge));
}

#[test]
fn spans_are_read_in_file_order() {
    let parsed =
        read("WEBVTT\n\n00:00:05.000 --> 00:00:09.000\nA\n\n00:00:02.000 --> 00:00:03.000\nB\n");
    assert_eq!(spans(&parsed), vec![(5_000, 9_000), (2_000, 3_000)]);
}

/// The densest line shape: one cue of `x` lines, two bytes each, refused whole
/// past the item budget (see the SRT test of the same name).
#[test]
fn dense_one_cue_of_tiny_lines_is_refused() {
    let header = "WEBVTT\n\n00:00.000 --> 00:00.001\n";
    let text = format!("{header}{}", "x\n".repeat((INPUT_BYTES - header.len()) / 2));
    assert!(text.len() <= INPUT_BYTES);
    assert_eq!(read_vtt(text.as_bytes()), Err(ReadError::TooComplex));
}

/// The densest real file: the smallest WebVTT cues, 27 bytes each, about
/// 310,000 cues of three items. It fits, and every cue is kept.
#[test]
fn densest_real_file_still_fits() {
    let cue = "00:00.000 --> 00:00.001\nx\n\n";
    let head = "WEBVTT\n\n";
    let count = (INPUT_BYTES - head.len()) / cue.len();
    let text = format!("{head}{}", cue.repeat(count));
    let parsed = read_vtt(text.as_bytes()).expect("the densest real file fits");
    assert_eq!(parsed.cues.len(), count);
    assert!(3 * count <= subtitles::ITEM_BUDGET);
}

/// A cue whose text is only white space has no text left: it is dropped
/// silently, as an empty cue is, and a file of only such cues has no cue.
#[test]
fn white_space_only_cue_is_dropped_silently() {
    let parsed =
        read("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n\t\n\n00:00:03.000 --> 00:00:04.000\nx\n");
    assert_eq!(texts(&parsed), vec!["x"]);
    assert_eq!(parsed.skipped.count, 0);
    assert!(parsed.losses.is_empty());
    assert_eq!(
        read_vtt("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n\u{a0}\n".as_bytes()),
        Err(ReadError::NoCues)
    );
}
