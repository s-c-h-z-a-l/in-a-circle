//! TTML reader: structure and timing (TTML2 sections 8, 10.4 and 12, Appendix I).
//! Fixtures are hand-written in `tests/data/ttml`; each test names the rule it
//! checks and the section that states it.

use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use subtitles::{Cue, INPUT_BYTES, ITEM_BUDGET, Parsed, ReadError, read_ttml};

/// Set to `1` on a runner that must have `xmllint`: a missing tool then fails
/// the test instead of skipping the comparison.
const REQUIRE_TOOLS: &str = "INACIRCLE_REQUIRE_SUBTITLE_TOOLS";

const TT: &str = r#"<tt xmlns="http://www.w3.org/ns/ttml">"#;

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

/// 12.3.1 and 7.2.11: a clock time base, a drop-frame time code and a
/// wall-clock time refuse the document: converting them would give wrong times.
#[test]
fn unconverted_time_bases_refuse_the_document() {
    let clock = "<tt xmlns=\"http://www.w3.org/ns/ttml\" \
        xmlns:ttp=\"http://www.w3.org/ns/ttml#parameter\" ttp:timeBase=\"clock\"><body/></tt>";
    assert_eq!(
        read_ttml(clock.as_bytes()),
        Err(ReadError::Unsupported("clock time base"))
    );
    let drop = "<tt xmlns=\"http://www.w3.org/ns/ttml\" \
        xmlns:ttp=\"http://www.w3.org/ns/ttml#parameter\" ttp:timeBase=\"smpte\" \
        ttp:dropMode=\"dropNTSC\"><body><div>\
        <p begin=\"00:00:01:00\" end=\"00:00:02:00\">x</p></div></body></tt>";
    assert_eq!(
        read_ttml(drop.as_bytes()),
        Err(ReadError::Unsupported("drop-frame time code"))
    );
    assert_eq!(
        read_ttml(doc("<p begin=\"wallclock(12:00:00)\" end=\"1s\">x</p>").as_bytes()),
        Err(ReadError::Unsupported("wall-clock time"))
    );
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

/// Elements the reader does not convert are skipped with their content and
/// counted once each: `set`, `image` and a foreign element, in a paragraph.
#[test]
fn unsupported_elements_are_skipped_and_counted() {
    let parsed = read(&format!(
        "{TT}<body><div><p begin=\"0s\" end=\"1s\">a<set begin=\"0s\">hidden</set>\
         b<image>x</image><x:foo xmlns:x=\"urn:x\">lost</x:foo>c</p></div></body></tt>"
    ));
    assert_eq!(texts(&parsed), vec!["abc"]);
    assert_eq!(parsed.skipped.count, 3);
    assert_eq!(parsed.skipped.first, Some((1, "unsupported element")));
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
