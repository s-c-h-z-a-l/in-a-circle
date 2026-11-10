//! TTML reader: structure, timing, styles and regions (TTML2 sections 8, 10, 11
//! and 12, Appendix I).
//! Fixtures are hand-written in `tests/data/ttml`; each test names the rule it
//! checks and the section that states it.

use std::collections::BTreeMap;
use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use subtitles::{Cue, INPUT_BYTES, ITEM_BUDGET, Loss, Parsed, ReadError, read_ttml};

/// Set to `1` on a runner that must have `xmllint`: a missing tool then fails
/// the test instead of skipping the comparison.
const REQUIRE_TOOLS: &str = "INACIRCLE_REQUIRE_SUBTITLE_TOOLS";

const TT: &str =
    r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling">"#;

fn read(text: &str) -> Parsed {
    read_ttml(text.as_bytes()).expect("the input has a usable cue")
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

/// The lines of the first cue, each as its text.
fn lines_of(cue: &Cue) -> Vec<String> {
    cue.lines
        .iter()
        .map(|line| line.runs.iter().map(|r| r.text.as_str()).collect())
        .collect()
}

/// A document with `body` as its body content.
fn doc(body: &str) -> String {
    format!("{TT}<body><div>{body}</div></body></tt>")
}

fn data(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/ttml")
        .join(name);
    fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn plain_document_reads_its_cues_and_ignores_head_metadata() {
    // 8.1.1, 8.1.2: the head is not content, and its metadata is not a loss.
    let parsed = read_ttml(&data("simple.ttml")).expect("usable");
    assert_eq!(texts(&parsed), vec!["First line", "Second\nline"]);
    assert_eq!(spans(&parsed), vec![(1_000, 2_500), (3_000, 4_000)]);
    assert_eq!(parsed.skipped.count, 0);
    assert!(parsed.losses.is_empty());
    assert!(parsed.cues.iter().all(|c| !c.top));
}

/// `seq` children follow one another. Each begin is relative
/// to the active end of the previous sibling (I.2.2). A begin or an end with no
/// `dur` still counts from that point.
#[test]
fn seq_children_follow_each_other() {
    let parsed = read_ttml(&data("seq.ttml")).expect("usable");
    assert_eq!(texts(&parsed), vec!["one", "two", "three"]);
    assert_eq!(
        spans(&parsed),
        vec![(10_000, 12_000), (13_000, 15_000), (15_000, 16_000)]
    );
}

/// `end` and `dur` together take the earlier of the two (12.2.2).
#[test]
fn end_and_dur_take_the_earlier() {
    let parsed = read_ttml(&data("begin_dur.ttml")).expect("usable");
    assert_eq!(
        spans(&parsed),
        vec![(1_000, 3_000), (1_000, 3_000), (6_000, 7_500)]
    );
}

/// Frame times in a document: frames at 30 fps with the NTSC multiplier
/// (`frameRateMultiplier="1000 1001"`) and the media time base (I.2.2). 15
/// frames are 15 * 1001 / 30000 s, so `00:00:01:15` is 1.5005 s: 1 501 ms.
#[test]
fn frame_times_round_to_the_nearest_millisecond() {
    let parsed = read_ttml(&data("frames.ttml")).expect("usable");
    assert_eq!(spans(&parsed), vec![(1_501, 2_000), (3_000, 4_501)]);
}

/// A paragraph with two timed spans becomes three cues, each
/// with the text active in it (12.2, 12.4). Neighbouring cues with the same
/// text are not merged.
#[test]
fn timed_spans_split_the_paragraph() {
    let parsed = read_ttml(&data("spans.ttml")).expect("usable");
    assert_eq!(
        texts(&parsed),
        vec!["Hello", "Hello big", "Hello world", "Hello"]
    );
    assert_eq!(
        spans(&parsed),
        vec![(0, 1_000), (1_000, 2_000), (2_000, 3_000), (3_000, 4_000)]
    );
}

/// A container's interval bounds its children (I.2.2, 12.2.3): a child's end
/// is clamped by the container's end, and a child with no end ends with its
/// container. A `begin` is relative to the container's begin.
#[test]
fn nested_containers_inherit_and_clamp_the_interval() {
    let parsed = read(&format!(
        "{TT}<body><div begin=\"2s\" end=\"6s\">\
         <p begin=\"1s\" end=\"10s\">clamped</p>\
         <p begin=\"3s\">open</p></div></body></tt>"
    ));
    assert_eq!(texts(&parsed), vec!["clamped", "open"]);
    assert_eq!(spans(&parsed), vec![(3_000, 6_000), (5_000, 6_000)]);
    // A span's own end is clamped by its paragraph's end.
    let parsed = read(&doc(
        "<p begin=\"0s\" end=\"8s\"><span begin=\"1s\" end=\"20s\">x</span></p>",
    ));
    assert_eq!(spans(&parsed), vec![(1_000, 8_000)]);
    assert_eq!(texts(&parsed), vec!["x"]);
}

/// A paragraph whose timing is wrong is skipped and counted at its line; the
/// paragraphs around it convert.
#[test]
fn bad_paragraph_in_the_middle_is_skipped_rest_converts() {
    let text = doc("<p begin=\"1s\" end=\"2s\">one</p>\n\
         <p begin=\"1:2:3\" end=\"4s\">broken</p>\n\
         <p begin=\"5s\" end=\"6s\">three</p>");
    let parsed = read(&text);
    assert_eq!(texts(&parsed), vec!["one", "three"]);
    assert_eq!(parsed.skipped.count, 1);
    // The broken paragraph is on the second line of the body.
    assert_eq!(parsed.skipped.first, Some((2, "bad timestamp")));
}

/// 12.2.3 and 12.4: a paragraph with no end (no `end` or `dur`, in a container
/// that has none either) has no end time. It is skipped, and so is its text.
#[test]
fn paragraph_without_an_end_is_skipped() {
    let parsed = read(&doc(
        "<p begin=\"1s\">no end</p>\n<p begin=\"2s\" end=\"3s\">ok</p>",
    ));
    assert_eq!(texts(&parsed), vec!["ok"]);
    assert_eq!(parsed.skipped.count, 1);
    assert_eq!(parsed.skipped.first, Some((1, "no end time")));
    // With the end in the container, the same paragraph ends with it.
    let parsed = read(&format!(
        "{TT}<body><div end=\"5s\"><p begin=\"1s\">ends with its div</p></div></body></tt>"
    ));
    assert_eq!(spans(&parsed), vec![(1_000, 5_000)]);
}

#[test]
fn malformed_xml_fails_with_the_line() {
    // The `</div>` on line 7 closes nothing it should: the paragraph is open.
    match read_ttml(&data("malformed.ttml")) {
        Err(ReadError::Malformed { line, reason }) => {
            assert_eq!(line, 7);
            assert!(!reason.is_empty());
        }
        other => panic!("expected Malformed, got {other:?}"),
    }
}

#[test]
fn not_ttml_root_fails() {
    assert_eq!(
        read_ttml(&data("foreign_root.xhtml")),
        Err(ReadError::NotTtml)
    );
    assert_eq!(
        read_ttml(b"<?xml version=\"1.0\"?><tt xmlns=\"urn:other\"><body/></tt>"),
        Err(ReadError::NotTtml)
    );
}

/// The DFXP 2006 namespace (and its parameter namespace) is read.
#[test]
fn old_dfxp_namespace_is_read() {
    let parsed = read_ttml(&data("dfxp_old_namespace.dfxp")).expect("usable");
    assert_eq!(texts(&parsed), vec!["Old DFXP namespace"]);
    assert_eq!(spans(&parsed), vec![(1_000, 2_000)]);
}

/// Namespace prefixes are the XML crate's job: `tt:p` is the TTML `p`.
#[test]
fn prefixed_tt_elements_are_read() {
    let parsed = read(
        "<tt:tt xmlns:tt=\"http://www.w3.org/ns/ttml\">\
         <tt:body><tt:div>\
         <tt:p begin=\"1s\" end=\"2s\">prefixed<tt:br/>lines</tt:p>\
         </tt:div></tt:body></tt:tt>",
    );
    assert_eq!(texts(&parsed), vec!["prefixed\nlines"]);
    assert_eq!(spans(&parsed), vec![(1_000, 2_000)]);
}

/// 8.2.10: `default` collapses white space to one space and trims the line;
/// `preserve` keeps the spaces, and a line feed is a line break. The mode is
/// inherited, and `default` on a child takes it back.
#[test]
fn whitespace_default_and_preserve() {
    let parsed = read(&doc("<p begin=\"0s\" end=\"1s\">  a \n   b\t c  </p>"));
    assert_eq!(lines_of(&parsed.cues[0]), vec!["a b c"]);

    let parsed = read(&doc(
        "<p begin=\"0s\" end=\"1s\" xml:space=\"preserve\">  a  b\n c</p>",
    ));
    assert_eq!(lines_of(&parsed.cues[0]), vec!["a  b", "c"]);

    // The mode of the div applies to the paragraph; the paragraph's `default`
    // takes it back.
    let parsed = read(&format!(
        "{TT}<body><div xml:space=\"preserve\">\
         <p begin=\"0s\" end=\"1s\">x  y</p>\
         <p begin=\"1s\" end=\"2s\" xml:space=\"default\">x  y</p>\
         </div></body></tt>"
    ));
    assert_eq!(lines_of(&parsed.cues[0]), vec!["x  y"]);
    assert_eq!(lines_of(&parsed.cues[1]), vec!["x y"]);
}

/// 8.1.7: `br` is a forced line break. Two in a row make no empty line.
#[test]
fn br_is_a_line_break() {
    let parsed = read(&doc(
        "<p begin=\"0s\" end=\"1s\">one<br/>two<br></br><br/>three</p>",
    ));
    assert_eq!(texts(&parsed), vec!["one\ntwo\nthree"]);
}

/// 8.1.7 and 12.4: a `br` in a span that has its own time breaks the line
/// only while that span is active.
#[test]
fn timed_break_is_active_only_in_its_span() {
    let parsed = read(&doc(
        "<p begin=\"0s\" end=\"2s\">a<span begin=\"1s\"><br/>b</span></p>",
    ));
    assert_eq!(texts(&parsed), vec!["a", "a\nb"]);
    assert_eq!(spans(&parsed), vec![(0, 1_000), (1_000, 2_000)]);
}

#[test]
fn utf16_input_is_decoded() {
    let text = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/ttml")
            .join("simple.ttml"),
    )
    .expect("a data file");
    let expected = read(&text);
    // The declaration must name the bytes' encoding: UTF-16 with its mark.
    let declared_utf16 = text.replace("encoding=\"UTF-8\"", "encoding=\"UTF-16\"");
    let mut utf16 = vec![0xFF, 0xFE];
    for unit in declared_utf16.encode_utf16() {
        utf16.extend(unit.to_le_bytes());
    }
    assert_eq!(read_ttml(&utf16).expect("usable"), expected);
    let mut with_utf8_bom = vec![0xEF, 0xBB, 0xBF];
    with_utf8_bom.extend(text.as_bytes());
    assert_eq!(read_ttml(&with_utf8_bom).expect("usable"), expected);
}

/// Malformed TTML, and a declared encoding that is not UTF-8 or UTF-16, are
/// refused as a whole; the input limit is the one of every subtitle reader.
#[test]
fn bad_encodings_and_limits_are_refused() {
    assert!(matches!(
        read_ttml(b"<?xml version=\"1.0\" encoding=\"ISO-8859-2\"?><tt/>"),
        Err(ReadError::Unsupported(_))
    ));
    let mut huge = vec![b' '; INPUT_BYTES + 1];
    huge[..TT.len()].copy_from_slice(TT.as_bytes());
    assert_eq!(read_ttml(&huge), Err(ReadError::TooLarge));
}

/// 12.4.1: text in a `seq` paragraph that has no time of its own is zero long
/// and is never shown. It is counted, not dropped; the timed span still converts.
#[test]
fn text_with_no_display_time_is_counted() {
    let parsed = read(&doc(
        "<p begin=\"0s\" timeContainer=\"seq\">Hello<span dur=\"2s\">World</span></p>",
    ));
    assert_eq!(texts(&parsed), vec!["World"]);
    assert_eq!(spans(&parsed), vec![(0, 2_000)]);
    assert_eq!(parsed.skipped.count, 1);
    assert_eq!(parsed.skipped.first, Some((1, "text with no display time")));
}

/// Text outside a paragraph is counted with the text reason; white space is not.
#[test]
fn text_outside_any_paragraph_is_counted() {
    let parsed = read(&format!(
        "{TT}<body>stray\n  <div><p begin=\"0s\" end=\"1s\">x</p></div></body></tt>"
    ));
    assert_eq!(texts(&parsed), vec!["x"]);
    assert_eq!(parsed.skipped.count, 1);
    assert_eq!(parsed.skipped.first, Some((1, "text outside any cue")));
}

/// A `condition` is not evaluated: the element is skipped with the reason.
#[test]
fn conditional_element_is_skipped() {
    let parsed = read(&doc(
        "<p begin=\"0s\" end=\"1s\" condition=\"ttp:profile()\">cond</p>\
         <p begin=\"2s\" end=\"3s\">plain</p>",
    ));
    assert_eq!(texts(&parsed), vec!["plain"]);
    assert_eq!(parsed.skipped.first, Some((1, "conditional element")));
}

/// A `p` whose end is before its begin is skipped (12.2.3).
#[test]
fn end_before_begin_is_skipped() {
    let parsed = read(&doc(
        "<p begin=\"5s\" end=\"2s\">backwards</p><p begin=\"0s\" end=\"1s\">ok</p>",
    ));
    assert_eq!(texts(&parsed), vec!["ok"]);
    assert_eq!(parsed.skipped.first, Some((1, "ends before it starts")));
}

/// 100 hours or more is past what a subtitle can carry. An
/// expression that says so is out of range; a paragraph whose time adds up to
/// it through its containers is past [`subtitles::MAX_MS`].
#[test]
fn time_at_or_past_100_hours_is_skipped() {
    let parsed = read(&doc(
        "<p begin=\"99:59:59.000\" end=\"100:00:01.000\">late</p>\
         <p begin=\"0s\" end=\"1s\">ok</p>",
    ));
    assert_eq!(texts(&parsed), vec!["ok"]);
    assert_eq!(parsed.skipped.first, Some((1, "time out of range")));
    let parsed = read(&format!(
        "{TT}<body><div begin=\"60:00:00.000\">\
         <p begin=\"50:00:00.000\" end=\"51:00:00.000\">late</p>\
         <p begin=\"0s\" end=\"1s\">ok</p></div></body></tt>"
    ));
    assert_eq!(texts(&parsed), vec!["ok"]);
    assert_eq!(parsed.skipped.first, Some((1, "time is 100 hours or more")));
}

/// An unknown `timeContainer` is a fault: the paragraphs under it are skipped
/// with its reason, not read as `par`.
#[test]
fn bad_time_container_skips_its_paragraphs() {
    let parsed = read(&format!(
        "{TT}<body><div timeContainer=\"sequential\">\
         <p begin=\"0s\" end=\"1s\">x</p></div>\
         <div><p begin=\"2s\" end=\"3s\">y</p></div></body></tt>"
    ));
    assert_eq!(texts(&parsed), vec!["y"]);
    assert_eq!(parsed.skipped.count, 1);
    assert_eq!(parsed.skipped.first, Some((1, "invalid timeContainer")));
}

/// Nothing but white space and metadata: no cue, so the file is refused.
#[test]
fn document_without_a_cue_is_refused() {
    assert_eq!(
        read_ttml(doc("<p begin=\"0s\" end=\"1s\"> </p>").as_bytes()),
        Err(ReadError::NoCues)
    );
    assert_eq!(
        read_ttml(format!("{TT}</tt>").as_bytes()),
        Err(ReadError::NoCues)
    );
}

/// 8 MiB of `<br/>` in one paragraph: each one is an element, a piece and a
/// copy in the one slice, so the item budget refuses it whole. Memory is what
/// the budget bounds; the figure is measured on the CLI.
#[test]
fn dense_one_paragraph_of_breaks_is_refused() {
    let head = format!("{TT}<body><div><p begin=\"0s\" end=\"1s\">");
    let tail = "</p></div></body></tt>";
    let count = (INPUT_BYTES - head.len() - tail.len()) / "<br/>".len();
    let text = format!("{head}{}{tail}", "<br/>".repeat(count));
    assert!(text.len() <= INPUT_BYTES);
    assert_eq!(read_ttml(text.as_bytes()), Err(ReadError::TooComplex));
}

/// The other dense shape: 8 MiB of empty `span` elements around one word. Each
/// empty span is one element and holds no piece, so the document fits the
/// budget and reads as one cue.
#[test]
fn dense_one_paragraph_of_empty_spans_is_bounded() {
    let head = format!("{TT}<body><div><p begin=\"0s\" end=\"1s\">x");
    let tail = "</p></div></body></tt>";
    let count = (INPUT_BYTES - head.len() - tail.len()) / "<span/>".len();
    assert!(count < ITEM_BUDGET, "the shape is within the budget");
    let text = format!("{head}{}{tail}", "<span/>".repeat(count));
    let parsed = read_ttml(text.as_bytes()).expect("the spans fit the budget");
    assert_eq!(texts(&parsed), vec!["x"]);
}

/// The best of three timed runs, so one slow run does not decide the ratio.
fn best_of_three(input: &[u8]) -> Duration {
    (0..3)
        .map(|_| {
            let start = Instant::now();
            black_box(read_ttml(black_box(input)).is_ok());
            start.elapsed()
        })
        .min()
        .unwrap_or_default()
}

/// A document of `n` units, each unit one construct of the shape.
type Shape = fn(usize) -> String;

/// Each shape is read at 1 MiB and at 4 MiB, both under INPUT_BYTES. Linear
/// work makes the 4 MiB run about 4 times the 1 MiB run; a quadratic reader
/// makes it 16 times, so the limit, 8 times, is halfway on a log scale. The
/// unit is the length of one construct, so both sizes hold whole units.
#[test]
fn ttml_linear_on_hostile_shapes() {
    const SMALL: usize = 1 << 20;
    const LARGE: usize = 4 << 20;
    let spans: Shape = |n| {
        doc(&format!(
            "<p begin=\"0s\" end=\"100s\">{}</p>",
            "<span>x</span>".repeat(n)
        ))
    };
    let empty: Shape = |n| {
        doc(&format!(
            "{}<p begin=\"0s\" end=\"1s\">x</p>",
            "<p/>".repeat(n)
        ))
    };
    let breaks: Shape = |n| {
        doc(&format!(
            "<p begin=\"0s\" end=\"100s\">x{}</p>",
            "<span begin=\"1s\"><br/></span>".repeat(n)
        ))
    };
    let shapes = [
        (
            "4 MiB of spans in one paragraph",
            "<span>x</span>".len(),
            spans,
        ),
        ("empty paragraphs before one cue", "<p/>".len(), empty),
        (
            "timed breaks in one paragraph",
            "<span begin=\"1s\"><br/></span>".len(),
            breaks,
        ),
    ];
    for (name, unit, shape) in shapes {
        let small = shape(SMALL / unit).into_bytes();
        let large = shape(LARGE / unit).into_bytes();
        assert!(large.len() <= INPUT_BYTES, "{name}: under the input limit");
        // Each shape is within the item budget at 4 MiB, so each reads whole.
        assert!(read_ttml(&large).is_ok(), "{name}: the large shape reads");
        let t_small = best_of_three(&small);
        let t_large = best_of_three(&large);
        assert!(
            t_large < t_small * 8,
            "{name}: 4x the input took {t_large:?} against {t_small:?}; linear is about 4x"
        );
    }
}

/// Nesting is bounded by the XML reader's depth limit (`MAX_DEPTH`, 256
/// elements open): past it the file is `Malformed`, at any size, and inside
/// it the file reads.
#[test]
fn nesting_past_the_depth_limit_is_malformed() {
    let open = |n: usize| {
        format!(
            "{TT}<body>{}<p begin=\"0s\" end=\"1s\">deep</p>{}</body></tt>",
            "<div>".repeat(n),
            "</div>".repeat(n)
        )
    };
    assert_eq!(texts(&read(&open(250))), vec!["deep"]);
    assert!(matches!(
        read_ttml(open(300).as_bytes()),
        Err(ReadError::Malformed {
            reason: "elements nested too deeply",
            ..
        })
    ));
    assert!(matches!(
        read_ttml(open(100_000).as_bytes()),
        Err(ReadError::Malformed { .. })
    ));
}

/// The reader stops at every cut of each fixture without a panic. A cut
/// file is usually malformed; the call is what is checked.
#[test]
fn reader_never_panics_on_every_prefix() {
    for name in [
        "simple.ttml",
        "seq.ttml",
        "begin_dur.ttml",
        "frames.ttml",
        "spans.ttml",
        "malformed.ttml",
        "dfxp_old_namespace.dfxp",
    ] {
        let bytes = data(name);
        for end in 0..=bytes.len() {
            let _ = read_ttml(&bytes[..end]);
        }
        for start in 0..=bytes.len() {
            let _ = read_ttml(&bytes[start..]);
        }
    }
}

/// `xmllint` agrees that the well-formed fixtures are well-formed (and that
/// the malformed one is not). A missing `xmllint` skips, unless the runner
/// requires it.
#[test]
fn xmllint_agrees_the_fixtures_are_well_formed() {
    let found = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("xmllint").is_file()))
        .unwrap_or(false);
    if !found {
        if std::env::var_os(REQUIRE_TOOLS).is_some_and(|v| v == "1") {
            panic!("xmllint is required ({REQUIRE_TOOLS}=1) but not installed");
        }
        eprintln!("skipping: xmllint is not installed");
        return;
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/ttml");
    for entry in fs::read_dir(&dir).expect("the fixture folder") {
        let path = entry.expect("an entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let wellformed = name != "malformed.ttml";
        let status = std::process::Command::new("xmllint")
            .args(["--noout", "--nonet"])
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("xmllint runs");
        assert_eq!(status.success(), wellformed, "xmllint on {name}");
    }
}

/// A paragraph's text is copied into every slice it is active in. A 4 MiB
/// text under eleven timed spans would be copied eleven times, 44 MiB, past
/// the 32 MiB output limit: the conversion could not be written anyway, so the
/// reader refuses it whole. The same text in one slice converts.
#[test]
fn copied_text_is_capped_at_the_output_size() {
    let text = "x".repeat(4 << 20);
    let spans: String = (0..5)
        .map(|i| {
            format!(
                "<span begin=\"{}s\" end=\"{}s\">y</span>",
                2 * i + 1,
                2 * i + 2
            )
        })
        .collect();
    let timed = doc(&format!("<p begin=\"0s\" end=\"20s\">{text}{spans}</p>"));
    assert!(timed.len() <= INPUT_BYTES);
    assert_eq!(read_ttml(timed.as_bytes()), Err(ReadError::TooComplex));
    let whole = doc(&format!("<p begin=\"0s\" end=\"20s\">{text}</p>"));
    assert_eq!(read(&whole).cues.len(), 1);
}

/// Each slice copies every piece active in it, so the copies, not the pieces,
/// are the work. 4,000 blank spans that begin at different times and end
/// together make about 8 million copies over 4,000 slices. Each copy is an
/// item, so the budget refuses the paragraph; uncharged, the work would
/// finish with a usable file.
#[test]
fn copies_of_overlapping_pieces_are_charged() {
    let spans: String = (0..4_000)
        .map(|i| format!("<span begin=\"{i}ms\" end=\"100s\"> </span>"))
        .collect();
    let text = doc(&format!("<p begin=\"0s\" end=\"100s\">x{spans}</p>"));
    assert_eq!(read_ttml(text.as_bytes()), Err(ReadError::TooComplex));
}

/// A cue's runs as (text, italic, bold, underline), in order.
fn runs(cue: &Cue) -> Vec<(String, bool, bool, bool)> {
    cue.lines
        .iter()
        .flat_map(|line| &line.runs)
        .map(|r| (r.text.clone(), r.italic, r.bold, r.underline))
        .collect()
}

fn run(text: &str, italic: bool, bold: bool, underline: bool) -> (String, bool, bool, bool) {
    (text.to_owned(), italic, bold, underline)
}

const STYLED: &str =
    r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling">"#;

/// A document with `head` as its head content and `body` as its body content.
fn styled(head: &str, body: &str) -> String {
    format!("{STYLED}<head>{head}</head><body><div>{body}</div></body></tt>")
}

fn loss(parsed: &Parsed, kind: Loss) -> usize {
    parsed.losses.get(&kind).copied().unwrap_or(0)
}

/// Referenced styles merge left to right, the later one winning (10.4.1.2).
/// `plain` after `ital` takes the italic off, and `bold` after it adds bold.
#[test]
fn referenced_styles_apply_left_to_right_later_wins() {
    let parsed = read_ttml(&data("style-chain.ttml")).expect("usable");
    assert_eq!(texts(&parsed)[0], "first");
    assert_eq!(
        runs(&parsed.cues[0]),
        vec![run("first", false, true, false)]
    );
    let plain = read(&styled(
        r#"<styling><style xml:id="a" tts:fontStyle="italic"/><style xml:id="b" tts:fontStyle="normal"/></styling>"#,
        r#"<p begin="0s" end="1s" style="a b">x</p>"#,
    ));
    assert_eq!(runs(&plain.cues[0]), vec![run("x", false, false, false)]);
    assert_eq!(parsed.skipped.count, 0);
}

/// A style that references styles merges them under its own declarations (10.4.1.3),
/// so `both` is italic and bold from its references and underlined from itself.
#[test]
fn chained_references_merge_under_the_style_own_declarations() {
    let parsed = read_ttml(&data("style-chain.ttml")).expect("usable");
    assert_eq!(runs(&parsed.cues[1]), vec![run("second", true, true, true)]);
}

/// An inline declaration beats every reference (10.4.1.1): the span's inline
/// `normal` beats the referenced `italic`. Without it, the reference holds.
#[test]
fn inline_overrides_referenced_style() {
    let parsed = read_ttml(&data("style-chain.ttml")).expect("usable");
    assert_eq!(texts(&parsed)[2], "third fourth");
    assert_eq!(
        runs(&parsed.cues[2]),
        vec![run("third fourth", false, false, false)]
    );
    let referenced = read(&styled(
        r#"<styling><style xml:id="ital" tts:fontStyle="italic"/></styling>"#,
        r#"<p begin="0s" end="1s"><span style="ital">x</span></p>"#,
    ));
    assert_eq!(
        runs(&referenced.cues[0]),
        vec![run("x", true, false, false)]
    );
}

/// Region style inheritance (10.4.2.2): a region's inheritable values reach the
/// paragraph flowed into it when no ancestor gives them.
#[test]
fn region_style_applies_below_the_paragraph() {
    let parsed = read(&format!(
        "{STYLED}<head><layout><region xml:id=\"r\" tts:fontStyle=\"italic\" tts:fontWeight=\"bold\"/></layout></head>\
         <body><div><p begin=\"0s\" end=\"1s\" region=\"r\">x</p></div></body></tt>"
    ));
    assert_eq!(runs(&parsed.cues[0]), vec![run("x", true, true, false)]);
}

/// Content style inheritance (10.4.2.1): an inheritable value on `body` reaches
/// the spans of its paragraphs. A value the ancestor gives beats the region's, so
/// the body's `none` keeps the region's underline off. A non-inheritable value
/// (`tts:displayAlign`) does not reach the span, and counts once for its cue.
#[test]
fn inheritable_properties_flow_to_spans_non_inheritable_do_not() {
    let parsed = read_ttml(&data("inheritance.ttml")).expect("usable");
    assert_eq!(
        runs(&parsed.cues[0]),
        vec![run("a", true, false, false), run("b", true, true, false)]
    );
    assert_eq!(runs(&parsed.cues[1]), vec![run("c", true, true, false)]);
    assert_eq!(runs(&parsed.cues[2]), vec![run("d", true, true, false)]);
    assert_eq!(loss(&parsed, Loss::Position), 1, "one cue, one count");
    let two = read(&doc(
        r#"<p begin="0s" end="1s" tts:displayAlign="after"><span>a</span><span>b</span></p>"#,
    ));
    assert_eq!(loss(&two, Loss::Position), 1, "two spans, still one count");
}

/// `fontStyle` and `fontWeight` take their values (10.2.22, 10.2.24); `oblique`
/// is an italic, and `textDecoration` underline is carried (10.3.37).
#[test]
fn italic_bold_underline_from_styles_and_inline() {
    let parsed = read(&doc(
        r#"<p begin="0s" end="1s" tts:fontStyle="oblique" tts:fontWeight="bold" tts:textDecoration="underline">x</p>"#,
    ));
    assert_eq!(runs(&parsed.cues[0]), vec![run("x", true, true, true)]);
    assert_eq!(parsed.skipped.count, 0);
    assert!(parsed.losses.is_empty());
}

/// TTML2 10.2.24 gives `fontWeight` two values, `normal` and `bold`. A number is
/// not one of them, so it is not bold, and the value counts as `Styling`.
#[test]
fn numeric_font_weight_is_not_bold_and_is_counted() {
    let parsed = read(&doc(r#"<p begin="0s" end="1s" tts:fontWeight="700">x</p>"#));
    assert_eq!(runs(&parsed.cues[0]), vec![run("x", false, false, false)]);
    assert_eq!(loss(&parsed, Loss::Styling), 1);
}

/// Each decoration is inherited on its own (10.2.43.1): the span's `noLineThrough`
/// clears the paragraph's line-through for its text, and the paragraph's text
/// that has no clearing keeps it, counted in its cue.
#[test]
fn decoration_is_inherited_per_component() {
    let parsed = read(&doc(
        r#"<p tts:textDecoration="lineThrough"><span begin="0s" end="1s" tts:textDecoration="noLineThrough">x</span><span begin="1s" end="2s">y</span></p>"#,
    ));
    assert_eq!(parsed.cues.len(), 2);
    assert_eq!(
        loss(&parsed, Loss::Styling),
        1,
        "only the second cue keeps line-through"
    );
}

/// A style that references styles in a loop is refused at the reference that
/// closes it, counted as `Styling`, and the rest of the chain is read (10.4.1.3).
/// A chain of 40 references is cut at the cap ([`MAX_CHAIN`], 32): the style at
/// its bottom (`s0`, italic) does not reach past it. A chain of 20 is read whole.
#[test]
fn style_cycles_and_deep_chains_are_refused_not_followed() {
    let parsed = read_ttml(&data("style-cycle.ttml")).expect("usable");
    assert_eq!(runs(&parsed.cues[0]), vec![run("loop", true, true, false)]);
    assert_eq!(
        runs(&parsed.cues[1]),
        vec![run("self", false, false, false)]
    );
    assert_eq!(
        loss(&parsed, Loss::Styling),
        2,
        "each cue with a refused reference"
    );

    let chain = |n: usize, body: &str| {
        let mut head = String::from(r#"<style xml:id="s0" tts:fontStyle="italic"/>"#);
        for k in 1..n {
            head.push_str(&format!(r#"<style xml:id="s{k}" style="s{}"/>"#, k - 1));
        }
        read(&styled(
            &format!("<styling>{head}</styling>"),
            &format!(r#"<p begin="0s" end="1s" style="s{}">{body}</p>"#, n - 1),
        ))
    };
    let deep = chain(40, "deep");
    assert_eq!(runs(&deep.cues[0]), vec![run("deep", false, false, false)]);
    assert_eq!(loss(&deep, Loss::Styling), 1);
    let short = chain(20, "short");
    assert_eq!(runs(&short.cues[0]), vec![run("short", true, false, false)]);
    assert_eq!(loss(&short, Loss::Styling), 0);
}

/// A `style` reference that names no style is a `Styling` loss (10.4.1.2: each
/// IDREF must reference a style with a styling ancestor). The text is kept.
#[test]
fn unknown_style_reference_is_a_styling_loss() {
    let parsed = read(&doc(r#"<p begin="0s" end="1s" style="missing">x</p>"#));
    assert_eq!(texts(&parsed), vec!["x"]);
    assert_eq!(loss(&parsed, Loss::Styling), 1);
}

/// A declaration the reader does not carry is counted in each cue it reaches,
/// once per cue: the paragraph's colour is in both cues of its two spans.
#[test]
fn losses_count_once_per_cue() {
    let parsed = read(&doc(
        r#"<p begin="0s" end="2s" tts:color="red"><span begin="0s" end="1s">a</span><span begin="1s" end="2s">b</span></p>"#,
    ));
    assert_eq!(parsed.cues.len(), 2);
    assert_eq!(loss(&parsed, Loss::Styling), 2);
}

/// Every `tts:` declaration on a paragraph is carried, or counted as the loss its
/// kind names (the accounting table of `ttml/mod.rs`). One row each: the value
/// the reader keeps, or the loss a paragraph carrying it gets.
#[test]
fn every_style_declaration_is_carried_or_counted() {
    let rows: [(&str, Option<Loss>); 15] = [
        (r#"tts:fontStyle="italic""#, None),
        (r#"tts:fontWeight="bold""#, None),
        (r#"tts:textDecoration="underline""#, None),
        (r#"tts:textDecoration="none""#, None),
        (r#"tts:textDecoration="lineThrough""#, Some(Loss::Styling)),
        (r#"tts:fontStyle="sideways""#, Some(Loss::Styling)),
        (r#"tts:color="red""#, Some(Loss::Styling)),
        (r#"tts:backgroundColor="blue""#, Some(Loss::Styling)),
        (r#"tts:fontSize="10px""#, Some(Loss::Styling)),
        (r#"tts:showBackground="always""#, Some(Loss::Styling)),
        (r#"tts:unknownThing="1""#, Some(Loss::Styling)),
        (r#"tts:textAlign="center""#, Some(Loss::Position)),
        (r#"tts:padding="1px""#, Some(Loss::Position)),
        (r#"tts:displayAlign="after""#, Some(Loss::Position)),
        (r#"tts:origin="0% 0%""#, Some(Loss::Position)),
    ];
    for (attr, want) in rows {
        let parsed = read(&doc(&format!(r#"<p begin="0s" end="1s" {attr}>x</p>"#)));
        let got: Vec<Loss> = parsed.losses.keys().copied().collect();
        assert_eq!(got, want.into_iter().collect::<Vec<_>>(), "{attr}");
        assert_eq!(texts(&parsed), vec!["x"], "{attr}: the text is kept");
    }
}

/// `top` (10.2.15, 10.2.16, 10.2.31): the anchor of the region is in the top third
/// of the root container. `before` is the origin, `center` the middle, `after` the
/// far edge. A percentage needs no root size; a px placement is read against the
/// root's `tts:extent` (1920 by 1080 px in the fixture).
#[test]
fn region_in_the_top_third_is_top() {
    let parsed = read_ttml(&data("regions.ttml")).expect("usable");
    let tops: Vec<bool> = parsed.cues.iter().map(|c| c.top).collect();
    assert_eq!(tops, vec![true, true, true, false, true, false]);
    assert_eq!(
        loss(&parsed, Loss::Position),
        1,
        "the bottom region is placed at the bottom, so its exact place is lost"
    );
}

/// A px placement without a root size (no `tts:extent` on `tt`) has no fraction,
/// so the paragraph is not top, and its placement counts as `Position`.
#[test]
fn px_placement_without_root_extent_is_not_top() {
    let parsed = read(&styled(
        r#"<layout><region xml:id="r" tts:origin="0px 0px"/></layout>"#,
        r#"<p begin="0s" end="1s" region="r">x</p>"#,
    ));
    assert!(!parsed.cues[0].top);
    assert_eq!(loss(&parsed, Loss::Position), 1);
}

/// A paragraph with no region is never top: the layout's default placement is
/// not the top (a player puts it at the bottom), and a document without a layout
/// has no region at all.
#[test]
fn no_region_is_never_top() {
    let parsed = read_ttml(&data("regions.ttml")).expect("usable");
    assert!(!parsed.cues[5].top, "the paragraph without a region");
    let plain = read(&doc(r#"<p begin="0s" end="1s">x</p>"#));
    assert!(!plain.cues[0].top);
    assert!(plain.losses.is_empty());
}

/// A region named by a paragraph but not defined: the placement is lost (not top,
/// `Position`). The text is kept.
#[test]
fn region_named_but_not_defined_is_a_position_loss() {
    let parsed = read(&styled(
        r#"<layout><region xml:id="r" tts:origin="0% 0%"/></layout>"#,
        r#"<p begin="0s" end="1s" region="nope">x</p>"#,
    ));
    assert_eq!(texts(&parsed), vec!["x"]);
    assert!(!parsed.cues[0].top);
    assert_eq!(loss(&parsed, Loss::Position), 1);
}

/// A style flood is linear: 100,000 `style` elements (4 MiB), and a paragraph
/// that references one style a million times (2 MiB of `a a a ...`). The
/// reference count is charged to no budget, so the time is what bounds it, and
/// it must grow with the input. Each shape is read at a quarter and in full.
#[test]
fn ttml_style_flood_is_linear() {
    let defs = |n: usize| {
        let body: String = (0..n)
            .map(|i| format!(r#"<style xml:id="s{i}" tts:color="red"/>"#))
            .collect();
        styled(
            &format!("<styling>{body}</styling>"),
            r#"<p begin="0s" end="1s">x</p>"#,
        )
    };
    let refs = |n: usize| {
        let ids = "a ".repeat(n);
        styled(
            r#"<styling><style xml:id="a" tts:fontStyle="italic"/></styling>"#,
            &format!(r#"<p begin="0s" end="1s" style="{}">x</p>"#, ids.trim_end()),
        )
    };
    for (name, shape, n) in [
        ("100 000 styles", defs as fn(usize) -> String, 100_000usize),
        (
            "1 000 000 references",
            refs as fn(usize) -> String,
            1_000_000usize,
        ),
    ] {
        let large = shape(n).into_bytes();
        let small = shape(n / 4).into_bytes();
        assert!(large.len() <= INPUT_BYTES, "{name}: under the input limit");
        assert!(read_ttml(&large).is_ok(), "{name}: the large shape reads");
        let t_small = best_of_three(&small);
        let t_large = best_of_three(&large);
        assert!(
            t_large < t_small * 8,
            "{name}: 4x the input took {t_large:?} against {t_small:?}; linear is about 4x"
        );
    }
}

/// The peak resident memory of this process (`VmHWM`, kB), from `/proc`. Zero
/// where `/proc` is not there, so the figure is printed as unknown then.
fn peak_rss_kib() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// The densest style table the input limit allows in its simplest form: 784,960
/// empty `style` elements (about 6 MiB). Each is an element and an entry, two
/// items, so the budget holds the table. The process peak is printed; run it
/// alone, since the peak is the whole process's:
/// `cargo test -p subtitles --test ttml style_flood_peak -- --ignored --nocapture`.
#[test]
#[ignore = "measurement: run alone with --nocapture"]
fn style_flood_peak_memory_is_reported() {
    let text = format!(
        "{STYLED}<head><styling>{}</styling></head><body><div><p begin=\"0s\" end=\"1s\">x</p></div></body></tt>",
        "<style/>".repeat(784_960)
    );
    assert!(text.len() <= INPUT_BYTES);
    let before = peak_rss_kib();
    let outcome = read_ttml(text.as_bytes());
    let after = peak_rss_kib();
    println!(
        "style flood: {} bytes, 784960 styles: {:?}; peak RSS before {:?} kiB, after {:?} kiB",
        text.len(),
        outcome.map(|p| p.cues.len()),
        before,
        after
    );
}

/// The same shape one style past the budget: 1,000,001 styles are 2,000,002
/// items, so the budget refuses the document, and the refusal allocates no more
/// than the entries that fit. Run alone, as the one above:
/// `cargo test -p subtitles --test ttml style_flood_past -- --ignored --nocapture`.
#[test]
#[ignore = "measurement: run alone with --nocapture"]
fn style_flood_past_the_budget_is_refused() {
    let text = format!(
        "{STYLED}<head><styling>{}</styling></head><body><div><p begin=\"0s\" end=\"1s\">x</p></div></body></tt>",
        "<style/>".repeat(1_000_001)
    );
    assert!(text.len() <= INPUT_BYTES);
    let before = peak_rss_kib();
    let outcome = read_ttml(text.as_bytes());
    let after = peak_rss_kib();
    println!(
        "style flood past budget: {} bytes, 1000001 styles: {:?}; peak RSS before {:?} kiB, after {:?} kiB",
        text.len(),
        outcome.map(|p| p.cues.len()),
        before,
        after
    );
    assert_eq!(read_ttml(text.as_bytes()), Err(ReadError::TooComplex));
}

const SMPTE_TT: &str = "http://www.smpte-ra.org/schemas/2052-1/2010/smpte-tt";

/// 12.3.1 and 7.2.11: a clock time base refuses the document; converting its
/// times would give wrong ones.
#[test]
fn clock_time_base_is_unsupported() {
    let clock = "<tt xmlns=\"http://www.w3.org/ns/ttml\" \
        xmlns:ttp=\"http://www.w3.org/ns/ttml#parameter\" ttp:timeBase=\"clock\"><body/></tt>";
    assert_eq!(
        read_ttml(clock.as_bytes()),
        Err(ReadError::Unsupported("clock time base"))
    );
}

/// 7.2.4: a drop-frame time code (`dropNTSC`, `dropPAL`) refuses the document.
#[test]
fn drop_frame_is_unsupported() {
    let drop = "<tt xmlns=\"http://www.w3.org/ns/ttml\" \
        xmlns:ttp=\"http://www.w3.org/ns/ttml#parameter\" ttp:timeBase=\"smpte\" \
        ttp:dropMode=\"dropNTSC\"><body><div>\
        <p begin=\"00:00:01:00\" end=\"00:00:02:00\">x</p></div></body></tt>";
    assert_eq!(
        read_ttml(drop.as_bytes()),
        Err(ReadError::Unsupported("drop-frame time code"))
    );
}

/// 12.3.1: a `wallclock()` expression refuses the document (the time base is
/// not clock, so the expression is an error for a presentation the reader makes).
#[test]
fn wallclock_expression_is_unsupported() {
    assert_eq!(
        read_ttml(doc("<p begin=\"wallclock(12:00:00)\" end=\"1s\">x</p>").as_bytes()),
        Err(ReadError::Unsupported("wall-clock time"))
    );
}

/// 13.1: `set` and `animate` change a style the reader does not apply. In a
/// paragraph they count once per cue of that paragraph (the paragraph's text is
/// shown as the same text, with the change left out).
#[test]
fn set_and_animate_are_counted() {
    let parsed = read(&doc(
        r#"<p begin="0s" end="1s">a<set begin="0s" dur="1s" attributeName="tts:color" to="red"/>b<animate begin="0s" dur="1s" attributeName="tts:color" to="blue"/>c</p>"#,
    ));
    assert_eq!(texts(&parsed), vec!["abc"]);
    assert_eq!(loss(&parsed, Loss::Styling), 1, "two elements, one cue");
    assert_eq!(parsed.skipped.count, 0);
    // In a div, a set has no paragraph of its own: every cue of the document
    // is counted, the upper bound the reader can give.
    let parsed = read(&doc(
        r#"<set begin="0s" dur="1s" attributeName="tts:color" to="red"/><p begin="0s" end="1s">a</p><p begin="2s" end="3s">b</p>"#,
    ));
    assert_eq!(texts(&parsed), vec!["a", "b"]);
    assert_eq!(loss(&parsed, Loss::Styling), 2);
}

/// A foreign-namespace element is pruned with its whole subtree (the standard
/// ignores it): its text is not shown, and it counts as `Elements` once per cue.
/// Outside a paragraph it counts on every cue of the document.
#[test]
fn foreign_namespace_content_is_pruned_and_counted() {
    let parsed = read(&doc(
        r#"<p begin="0s" end="1s">a<x:foo xmlns:x="urn:x">lost<span begin="0s" end="1s">inner</span></x:foo>c</p>"#,
    ));
    assert_eq!(texts(&parsed), vec!["ac"]);
    assert_eq!(loss(&parsed, Loss::Elements), 1);
    assert_eq!(parsed.skipped.count, 0);
    let parsed = read(&doc(
        r#"<x:foo xmlns:x="urn:x"><p begin="0s" end="1s">lost</p></x:foo><p begin="0s" end="1s">kept</p>"#,
    ));
    assert_eq!(texts(&parsed), vec!["kept"]);
    assert_eq!(loss(&parsed, Loss::Elements), 1);
    assert_eq!(parsed.skipped.count, 0);
}

/// An image is an `Elements` unit: the image is not shown, and the text beside
/// it converts (an image paragraph with text keeps the text too). A `div` with a
/// background image is one unit, and one `image` element is one unit.
#[test]
fn images_beside_text_count_as_elements() {
    let parsed = read(&format!(
        "<tt xmlns=\"http://www.w3.org/ns/ttml\" xmlns:smpte=\"{SMPTE_TT}\"><body>\
         <div><p begin=\"0s\" end=\"1s\">text</p></div>\
         <div><image begin=\"0s\" end=\"1s\" src=\"a.png\"/></div>\
         <div><p begin=\"2s\" end=\"3s\" smpte:backgroundImage=\"#i\">words</p></div>\
         </body></tt>"
    ));
    assert_eq!(texts(&parsed), vec!["text", "words"]);
    assert_eq!(
        loss(&parsed, Loss::Elements),
        2,
        "one image, one image paragraph"
    );
    assert_eq!(parsed.skipped.count, 0);
}

/// A document whose only subtitles are images has no text to convert: it is
/// refused as image-based, not as having no cues. A document with neither
/// images nor cues is `NoCues`.
#[test]
fn image_only_document_is_refused() {
    assert_eq!(
        read_ttml(doc("<image begin=\"0s\" end=\"1s\" src=\"a.png\"/>").as_bytes()),
        Err(ReadError::ImageBased)
    );
    assert_eq!(
        read_ttml(
            format!(
                "<tt xmlns=\"http://www.w3.org/ns/ttml\" xmlns:smpte=\"{SMPTE_TT}\"><body><div>\
                 <p begin=\"0s\" end=\"1s\" smpte:backgroundImage=\"#i\"/></div></body></tt>"
            )
            .as_bytes()
        ),
        Err(ReadError::ImageBased)
    );
    assert_eq!(
        read_ttml(doc("<p begin=\"0s\" end=\"1s\"></p>").as_bytes()),
        Err(ReadError::NoCues)
    );
}

/// 12.4 and 12.4.1: a paragraph in a body with no timing has no end (its
/// implicit duration is indefinite, and the document has no end to give it).
/// It is skipped with its reason, and a document with nothing else is refused.
#[test]
fn paragraph_without_any_timing_has_no_end() {
    assert_eq!(
        read_ttml(doc("<p>x</p>").as_bytes()),
        Err(ReadError::NoCues)
    );
    let parsed = read(&doc("<p>x</p><p begin=\"0s\" end=\"1s\">y</p>"));
    assert_eq!(texts(&parsed), vec!["y"]);
    assert_eq!(parsed.skipped.first, Some((1, "no end time")));
}

/// Ruby text stays in the cue (its base and its annotation both), and the ruby
/// declarations are counted as `Styling`.
#[test]
fn ruby_text_stays_and_counts_styling() {
    let parsed = read(&doc(
        r#"<p begin="0s" end="1s"><ruby>漢<rt>kan</rt></ruby></p>"#,
    ));
    assert_eq!(texts(&parsed), vec!["漢kan"]);
    assert_eq!(loss(&parsed, Loss::Styling), 1);
    assert_eq!(parsed.skipped.count, 0);
}

/// A duplicate `xml:id` among styles is an xml:id error, which is not fatal
/// (XML ID section 6). The first definition stands; each reference to the name
/// counts the declarations of the dropped one, so the references that would
/// have used them are counted, and no other cue is.
#[test]
fn duplicate_style_id_keeps_the_first_and_counts_its_references() {
    let parsed = read(&styled(
        r#"<styling><style xml:id="s" tts:fontStyle="italic"/><style xml:id="s" tts:fontWeight="bold" tts:color="red"/></styling>"#,
        r#"<p begin="0s" end="1s" style="s">x</p><p begin="2s" end="3s">y</p>"#,
    ));
    assert_eq!(texts(&parsed), vec!["x", "y"]);
    assert_eq!(runs(&parsed.cues[0]), vec![run("x", true, false, false)]);
    assert_eq!(runs(&parsed.cues[1]), vec![run("y", false, false, false)]);
    assert_eq!(loss(&parsed, Loss::Styling), 1);
}

/// A duplicate region `xml:id`: the first region places the paragraph, and the
/// reference counts the placement of the dropped one as `Position`.
#[test]
fn duplicate_region_id_keeps_the_first_and_counts_its_references() {
    let parsed = read(&styled(
        r#"<layout><region xml:id="r" tts:origin="10% 0%" tts:extent="80% 10%"/><region xml:id="r" tts:origin="10% 80%" tts:extent="80% 10%"/></layout>"#,
        r#"<p begin="0s" end="1s" region="r">x</p>"#,
    ));
    assert!(parsed.cues[0].top, "the first region is the top one");
    assert_eq!(loss(&parsed, Loss::Position), 1);
}

/// One row per construct the accounting table of `ttml/mod.rs` lists as
/// counted or skipped: the document, and exactly what it must count. A row
/// names its construct, so a failure says which one stopped being counted.
struct Row {
    construct: &'static str,
    input: String,
    losses: &'static [(Loss, usize)],
    skipped: usize,
    first: Option<(usize, &'static str)>,
}

fn row(
    construct: &'static str,
    input: String,
    losses: &'static [(Loss, usize)],
    skipped: usize,
    first: Option<(usize, &'static str)>,
) -> Row {
    Row {
        construct,
        input,
        losses,
        skipped,
        first,
    }
}

/// Every row is one document on one line, so every skip is on line 1.
fn accounting_rows() -> Vec<Row> {
    let body = |b: &str| format!("{TT}<body>{b}</body></tt>");
    vec![
        row(
            "metadata: silent",
            doc(
                r#"<metadata><x:foo xmlns:x="urn:x">m</x:foo></metadata><p begin="0s" end="1s">a</p>"#,
            ),
            &[],
            0,
            None,
        ),
        row(
            "head: foreign element (document)",
            styled(
                r#"<x:foo xmlns:x="urn:x">f</x:foo>"#,
                r#"<p begin="0s" end="1s">a</p>"#,
            ),
            &[(Loss::Elements, 1)],
            0,
            None,
        ),
        row(
            "head: unknown tt element (document)",
            styled("<foo/>", r#"<p begin="0s" end="1s">a</p>"#),
            &[(Loss::Elements, 1)],
            0,
            None,
        ),
        row(
            "styling: unknown tt element (document)",
            styled(
                "<styling><foo/></styling>",
                r#"<p begin="0s" end="1s">a</p>"#,
            ),
            &[(Loss::Elements, 1)],
            0,
            None,
        ),
        row(
            "initial: every cue is Styling (document)",
            styled(
                r#"<styling><initial tts:color="red"/></styling>"#,
                r#"<p begin="0s" end="1s">a</p><p begin="2s" end="3s">b</p>"#,
            ),
            &[(Loss::Styling, 2)],
            0,
            None,
        ),
        row(
            "set in a paragraph: Styling per cue",
            doc(r#"<p begin="0s" end="1s">a<set begin="0s" dur="1s"/>b</p>"#),
            &[(Loss::Styling, 1)],
            0,
            None,
        ),
        row(
            "animate in a div: Styling on every cue",
            doc(
                r#"<animate begin="0s" dur="1s"/><p begin="0s" end="1s">a</p><p begin="2s" end="3s">b</p>"#,
            ),
            &[(Loss::Styling, 2)],
            0,
            None,
        ),
        row(
            "animation in a paragraph: Styling per cue",
            doc(r#"<p begin="0s" end="1s">a<animation begin="0s" dur="1s"/>b</p>"#),
            &[(Loss::Styling, 1)],
            0,
            None,
        ),
        row(
            "foreign element in a paragraph: Elements per cue",
            doc(r#"<p begin="0s" end="1s">a<x:foo xmlns:x="urn:x">lost</x:foo>b</p>"#),
            &[(Loss::Elements, 1)],
            0,
            None,
        ),
        row(
            "foreign element in a div: Elements on every cue",
            doc(
                r#"<x:foo xmlns:x="urn:x"/><p begin="0s" end="1s">a</p><p begin="2s" end="3s">b</p>"#,
            ),
            &[(Loss::Elements, 2)],
            0,
            None,
        ),
        row(
            "unknown tt element in a paragraph: Elements per cue",
            doc(r#"<p begin="0s" end="1s">a<foo/>b</p>"#),
            &[(Loss::Elements, 1)],
            0,
            None,
        ),
        row(
            "image element in a div: one image unit",
            doc(r#"<image begin="0s" end="1s" src="a.png"/><p begin="0s" end="1s">a</p>"#),
            &[(Loss::Elements, 1)],
            0,
            None,
        ),
        row(
            "image element in a paragraph: one unit, its text converts",
            doc(r#"<p begin="0s" end="1s">a<image src="a.png"/>b</p><p begin="2s" end="3s">c</p>"#),
            &[(Loss::Elements, 1)],
            0,
            None,
        ),
        row(
            "smpte:backgroundImage on a paragraph: one unit, its text converts",
            format!(
                "<tt xmlns=\"http://www.w3.org/ns/ttml\" xmlns:smpte=\"{SMPTE_TT}\"><body><div>\
                 <p begin=\"0s\" end=\"1s\" smpte:backgroundImage=\"#i\">a</p>\
                 <p begin=\"2s\" end=\"3s\">c</p></div></body></tt>"
            ),
            &[(Loss::Elements, 1)],
            0,
            None,
        ),
        row(
            "smpte:backgroundImage on a div: one unit, the text under it converts",
            format!(
                "<tt xmlns=\"http://www.w3.org/ns/ttml\" xmlns:smpte=\"{SMPTE_TT}\"><body>\
                 <div smpte:backgroundImage=\"#i\"><p begin=\"0s\" end=\"1s\">a</p>\
                 <p begin=\"2s\" end=\"3s\">b</p></div>\
                 <div><p begin=\"4s\" end=\"5s\">c</p></div></body></tt>"
            ),
            &[(Loss::Elements, 1)],
            0,
            None,
        ),
        row(
            "smpte:backgroundImage on a span: its paragraph is one unit",
            format!(
                "<tt xmlns=\"http://www.w3.org/ns/ttml\" xmlns:smpte=\"{SMPTE_TT}\"><body><div>\
                 <p begin=\"0s\" end=\"1s\">a<span smpte:backgroundImage=\"#i\">b</span></p>\
                 <p begin=\"2s\" end=\"3s\">c</p></div></body></tt>"
            ),
            &[(Loss::Elements, 1)],
            0,
            None,
        ),
        row(
            "smpte:backgroundImage on a body: one unit, the text converts",
            format!(
                "<tt xmlns=\"http://www.w3.org/ns/ttml\" xmlns:smpte=\"{SMPTE_TT}\"><body smpte:backgroundImage=\"#i\">\
                 <div><p begin=\"0s\" end=\"1s\">a</p></div></body></tt>"
            ),
            &[(Loss::Elements, 1)],
            0,
            None,
        ),
        row(
            "ruby: the text stays, Styling per cue",
            doc(r#"<p begin="0s" end="1s"><ruby>漢<rt>kan</rt></ruby></p>"#),
            &[(Loss::Styling, 1)],
            0,
            None,
        ),
        row(
            "condition on a paragraph: skipped, its text may or may not show",
            doc(
                r#"<p begin="0s" end="1s" condition="ttp:profile()">cond</p><p begin="2s" end="3s">plain</p>"#,
            ),
            &[],
            1,
            Some((1, "conditional element")),
        ),
        row(
            "span outside a paragraph: its text cannot be shown, skipped",
            doc(r#"<span begin="0s" end="1s">lost</span><p begin="0s" end="1s">x</p>"#),
            &[],
            1,
            Some((1, "unsupported element")),
        ),
        row(
            "duplicate style id: the references count the dropped declarations",
            styled(
                r#"<styling><style xml:id="s" tts:fontStyle="italic"/><style xml:id="s" tts:color="red"/></styling>"#,
                r#"<p begin="0s" end="1s" style="s">a</p>"#,
            ),
            &[(Loss::Styling, 1)],
            0,
            None,
        ),
        row(
            "duplicate region id: the references count the dropped placement",
            styled(
                r#"<layout><region xml:id="r" tts:origin="10% 0%"/><region xml:id="r" tts:origin="10% 80%"/></layout>"#,
                r#"<p begin="0s" end="1s" region="r">a</p>"#,
            ),
            &[(Loss::Position, 1)],
            0,
            None,
        ),
        row(
            "style reference not defined: Styling",
            doc(r#"<p begin="0s" end="1s" style="nope">a</p>"#),
            &[(Loss::Styling, 1)],
            0,
            None,
        ),
        row(
            "tts:fontFamily: not carried, Styling",
            doc(r#"<p begin="0s" end="1s" tts:fontFamily="serif">a</p>"#),
            &[(Loss::Styling, 1)],
            0,
            None,
        ),
        row(
            "tts:textAlign: not carried, Position",
            doc(r#"<p begin="0s" end="1s" tts:textAlign="center">a</p>"#),
            &[(Loss::Position, 1)],
            0,
            None,
        ),
        row(
            "region named and not defined: Position",
            doc(r#"<p begin="0s" end="1s" region="nope">a</p>"#),
            &[(Loss::Position, 1)],
            0,
            None,
        ),
        row(
            "text outside any cue: skipped",
            body(r#"stray<div><p begin="0s" end="1s">x</p></div>"#),
            &[],
            1,
            Some((1, "text outside any cue")),
        ),
        row(
            "bad timestamp: the paragraph skipped",
            doc(r#"<p begin="x" end="1s">a</p><p begin="2s" end="3s">b</p>"#),
            &[],
            1,
            Some((1, "bad timestamp")),
        ),
        row(
            "text with no display time: skipped",
            doc(r#"<p begin="0s" timeContainer="seq">Hello<span dur="2s">World</span></p>"#),
            &[],
            1,
            Some((1, "text with no display time")),
        ),
        row(
            "invalid xml:space: the paragraph skipped",
            doc(r#"<p begin="0s" end="1s" xml:space="maybe">a</p><p begin="2s" end="3s">b</p>"#),
            &[],
            1,
            Some((1, "invalid xml:space")),
        ),
        row(
            "paragraph with no end: skipped",
            doc(r#"<p>x</p><p begin="0s" end="1s">y</p>"#),
            &[],
            1,
            Some((1, "no end time")),
        ),
        row(
            "time at or past 100 hours: skipped",
            body(
                r#"<div begin="60:00:00.000"><p begin="50:00:00.000" end="51:00:00.000">late</p></div><div><p begin="0s" end="1s">ok</p></div>"#,
            ),
            &[],
            1,
            Some((1, "time is 100 hours or more")),
        ),
    ]
}

/// Review focus 4: every construct the accounting table lists is counted
/// exactly: its losses by kind, and the skipped count and its first line.
#[test]
fn every_ignored_construct_is_counted() {
    for row in accounting_rows() {
        let parsed = read_ttml(row.input.as_bytes())
            .unwrap_or_else(|e| panic!("{}: refused: {e}", row.construct));
        let losses: BTreeMap<Loss, usize> = row.losses.iter().copied().collect();
        assert_eq!(parsed.losses, losses, "{}: losses", row.construct);
        assert_eq!(
            parsed.skipped.count, row.skipped,
            "{}: skipped",
            row.construct
        );
        assert_eq!(
            parsed.skipped.first, row.first,
            "{}: first skip",
            row.construct
        );
    }
}
