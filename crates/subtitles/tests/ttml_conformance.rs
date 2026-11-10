//! Conformance over the W3C imsc-tests TTML files in `tests/data/imsc-tests`
//! (commit `b7f88fee6a181126931a254408440f38d8985224`, see the README there).
//!
//! Three checks, with different jobs:
//! - `imsc_files_parse_to_the_expected_outcome` is a REGRESSION GUARD, not
//!   evidence. Its rows in `expected.tsv` are what the reader gave when the
//!   file was written (`regenerate_expected_tsv` prints them). A row catches a
//!   change in behaviour; it does not show that the behaviour is right. The
//!   evidence that the reader is right is `hand-checked.tsv`: a person read
//!   each file against its own description and wrote what the file says should
//!   happen, and what the reader gives.
//! - `ttconv_agrees_on_times_and_text` is an INDEPENDENT ORACLE: Sandflow's
//!   `ttconv` 1.2.3 (`tt convert`), which is not this project's code. Its
//!   output is parsed here by a parser of its own, and compared with our own
//!   SRT and WebVTT output read back by our readers: the same start and end
//!   in milliseconds, and the same text with its italic, bold and underline.
//!   Nothing is normalised on both sides. The only markup removed is the
//!   colour and font markup that the reader drops and counts (`<font …>`,
//!   `</font>`, `<c.…>`, `</c>`); every other tag is read.
//! - `ttconv_comparison_set_is_classified_without_the_tool` runs everywhere:
//!   every file is refused by us, excluded for a stated reason, or compared.
//!
//! Structural exclusions, found by the test. A file whose cues overlap in
//! time is not compared: ttconv merges the paragraphs on screen at one moment
//! into one cue, and the reader keeps one cue per paragraph. A file with a
//! `span` that has its own timing is not compared either: the reader slices
//! the paragraph by such spans, and ttconv snapshots the whole document.
//!
//! The divergence list is `DIVERGENT_FILES` below: one line per file, with the
//! reason it is not compared. An entry marked OPEN is a real mismatch the
//! reader does not fix yet; it is listed so that nothing is left out silently.
//! The list was written from ttconv's source (1.2.3), not from a run: the
//! first CI run is the first run of this comparison.
//!
//! Standard library, the `xml` crate (a dependency of the reader), and the
//! `tt` command of ttconv 1.2.3 when it is installed (CI only). A missing `tt`
//! skips the comparison with a note, unless `INACIRCLE_REQUIRE_TTCONV=1`,
//! which makes it a failure.

use std::collections::BTreeMap;
use std::fs;
use std::panic;
use std::path::Path;
use std::process::{Command, Stdio};

use subtitles::{
    Cue, Line, Parsed, ReadError, Run, read_srt, read_ttml, read_vtt, sort_cues, write_srt,
    write_vtt,
};

/// The vendored data folder, relative to this crate.
const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/imsc-tests");

/// The four W3C suites that hold `.ttml` files.
const SUITES: &[&str] = &["imsc1", "imsc1_1", "imsc1_2", "imsc1_3"];

const GUARD: &str = include_str!("data/imsc-tests/expected.tsv");

const HAND_CHECKED: &str = include_str!("data/imsc-tests/hand-checked.tsv");

/// Set to `1` on the CI runner, which installs `tt` (ttconv 1.2.3): a missing
/// `tt` then fails the comparison instead of skipping it.
const REQUIRE_TTCONV: &str = "INACIRCLE_REQUIRE_TTCONV";

/// Files the ttconv comparison leaves out, each with its reason. A file is
/// listed here only when its content differs from ttconv's, by design or by an
/// open finding; a difference nobody has explained is a failure, not a line in
/// this table.
const DIVERGENT_FILES: &[(&str, &str)] = &[
    // A paragraph that a `set` changes in visibility is refused and counted (13.1.3);
    // ttconv plays the set and shows that paragraph, so these files cannot match.
    (
        "imsc1/ttml/timing/MediaParTiming002.ttml",
        "a paragraph refused by its set (visibility changes over time): ttconv shows it",
    ),
    (
        "imsc1/ttml/timing/MediaSeqTiming007.ttml",
        "a paragraph refused by its set (visibility changes over time): ttconv shows it",
    ),
    // tts:visibility: ttconv's ISD prunes only `display="none"`, so it keeps the
    // text that `visibility="hidden"` hides; we do not show it.
    (
        "imsc1/ttml/visibility/Visibility003.ttml",
        "visibility hidden text: ttconv keeps it, we hide it",
    ),
    // oblique: ttconv's writers keep only normal and italic; the reader carries oblique as italic (the spec).
    (
        "imsc1/ttml/fontStyle/FontStyle003.ttml",
        "oblique is carried as italic here; ttconv drops it",
    ),
    // underline with other decorations: ttconv keeps underline only as the sole decoration.
    (
        "imsc1/ttml/textDecoration/TextDecoration012.ttml",
        "underline with line-through: ttconv drops the underline",
    ),
    (
        "imsc1/ttml/textDecoration/TextDecoration013.ttml",
        "underline with line-through and overline: ttconv drops the underline",
    ),
    // ruby: ttconv's SRT and VTT writers have no branch for ruby elements and drop their text.
    (
        "imsc1_1/ttml/linePadding/linepadding002.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/ruby/ruby001.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/ruby/ruby002.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/ruby/ruby003.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/ruby/ruby004.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/ruby/ruby005.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/ruby/ruby006.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/rubyAlign/rubyAlign001.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/rubyAlign/rubyAlign002.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/rubyAlign/rubyAlign003.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/rubyAlign/rubyAlign004.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/rubyReserve/rubyReserve001.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/rubyReserve/rubyReserve002.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/rubyReserve/rubyReserve003.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/shear/shear002.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    (
        "imsc1_1/ttml/shear/shear003.ttml",
        "ruby: ttconv drops ruby element text",
    ),
    // Times of 100 hours or more are skipped and counted; ttconv writes a three-digit hour.
    (
        "imsc1/ttml/timing/TimeExpressions001.ttml",
        "times of 100 hours or more: we skip and count them",
    ),
    // Preserve mode keeps the space before a line feed in ttconv; we trim line ends.
    (
        "imsc1/ttml/space/space-preserve-001.ttml",
        "a space before a line feed in preserve mode: we trim it, ttconv keeps it",
    ),
];

/// Files whose WebVTT comparison is left out, with the reason; their SRT
/// comparison stays. Every one has a `div` inside a `div` (checked by the
/// classification test). ttconv's WebVTT writer visits only the paragraphs that
/// are direct children of a `div` (`vtt/writer.py`, `add_isd`), so a single
/// paragraph inside nested divisions is dropped from its output, while its SRT
/// writer recurses (`srt/writer.py`, `append_element`) and agrees with us.
const VTT_DIVERGENT_FILES: &[(&str, &str)] = &[
    (
        "imsc1/ttml/div/Div001.ttml",
        "ttconv's WebVTT writer drops a paragraph in a nested div",
    ),
    (
        "imsc1/ttml/timing/MediaSeqTiming002.ttml",
        "ttconv's WebVTT writer drops a paragraph in a nested div",
    ),
    (
        "imsc1/ttml/timing/MediaSeqTiming003.ttml",
        "ttconv's WebVTT writer drops a paragraph in a nested div",
    ),
    (
        "imsc1/ttml/timing/MediaSeqTiming004.ttml",
        "ttconv's WebVTT writer drops a paragraph in a nested div",
    ),
    (
        "imsc1/ttml/timing/MediaSeqTiming005.ttml",
        "ttconv's WebVTT writer drops a paragraph in a nested div",
    ),
];

/// The number of files the comparison covers on the vendored set: the
/// classification gives 229 compared, 22 refused, 22 overlapping, 12 with timed
/// spans, 10 with a style animation, 2 with no end and 24 listed. A file that
/// leaves the comparison changes this count and fails the run until the count
/// and the reasons are reviewed together.
const COMPARED_FLOOR: usize = 229;

/// Every vendored `.ttml` file, as its path under the data folder, sorted.
fn ttml_files() -> Vec<String> {
    let root = Path::new(ROOT);
    let mut out = Vec::new();
    for suite in SUITES {
        let mut stack = vec![root.join(suite)];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).expect("a vendored folder") {
                let path = entry.expect("a readable entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "ttml") {
                    let rel = path.strip_prefix(root).expect("inside the data folder");
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    out.sort();
    out
}

/// The outcome class of a refused or read document, as one short label.
fn outcome(result: &Result<Parsed, ReadError>) -> String {
    match result {
        Ok(_) => "ok".into(),
        Err(e) => match e {
            ReadError::NotUtf8 { .. } => "not-utf8".into(),
            ReadError::TooLarge => "too-large".into(),
            ReadError::TooComplex => "too-complex".into(),
            ReadError::NoCues => "no-cues".into(),
            ReadError::Malformed { .. } => "malformed".into(),
            ReadError::NotTtml => "not-ttml".into(),
            ReadError::Unsupported(reason) => format!("unsupported: {reason}"),
            ReadError::ImageBased => "image-based".into(),
            ReadError::AllSkipped { reason, .. } => format!("all-skipped: {reason}"),
            ReadError::TextTooLarge => "text-too-large".into(),
        },
    }
}

/// A cue's text as one string: runs joined, lines joined by the two characters
/// `\n`, so the guard stays one row per file (a tab is written `\t`).
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
        .join("\\n")
        .replace('\t', "\\t")
}

/// The guard's four observed columns for one file: outcome, cue count, the
/// first cue's start (in time order) and its text. A panic is reported with
/// the file name, since it is the one failure the guard must name.
fn observed(path: &str) -> [String; 4] {
    let bytes = fs::read(Path::new(ROOT).join(path)).expect("a vendored file");
    let result = panic::catch_unwind(|| read_ttml(&bytes))
        .unwrap_or_else(|_| panic!("{path}: the reader panicked"));
    let label = outcome(&result);
    match result {
        Ok(parsed) => {
            let mut cues = parsed.cues;
            sort_cues(&mut cues);
            let first = cues.first();
            [
                label,
                cues.len().to_string(),
                first.map_or("-".into(), |c| c.start_ms.to_string()),
                first.map_or("-".into(), text_of),
            ]
        }
        Err(_) => [label, "0".into(), "-".into(), "-".into()],
    }
}

/// The data rows of a TSV: the lines that are neither comments nor the header.
fn tsv_rows(text: &'static str) -> Vec<Vec<&'static str>> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.starts_with("file\t"))
        .map(|l| l.split('\t').collect())
        .collect()
}

/// Prints the guard rows (path, observed columns) for `expected.tsv`. Run as
/// `cargo test -p subtitles --test ttml_conformance -- --ignored --nocapture
/// regenerate_expected_tsv`. The rows are the reader's output and are only
/// as good as the reader: a regenerated file is a new guard, to be reviewed
/// against `hand-checked.tsv`.
#[test]
#[ignore = "prints the guard rows; run by hand"]
fn regenerate_expected_tsv() {
    for path in ttml_files() {
        let [outcome, cues, start, text] = observed(&path);
        println!("{path}\t{outcome}\t{cues}\t{start}\t{text}");
    }
}

#[test]
fn guard_has_one_row_per_vendored_file() {
    let guarded: Vec<&str> = tsv_rows(GUARD).iter().map(|r| r[0]).collect();
    assert_eq!(
        guarded,
        ttml_files(),
        "expected.tsv and the data folder differ"
    );
}

#[test]
fn imsc_files_parse_to_the_expected_outcome() {
    let mut mismatches = Vec::new();
    for row in tsv_rows(GUARD) {
        let path = row[0];
        let [outcome, cues, start, text] = observed(path);
        let want = [row[1], row[2], row[3], row[4]];
        let got = [
            outcome.as_str(),
            cues.as_str(),
            start.as_str(),
            text.as_str(),
        ];
        if want != got {
            mismatches.push(format!("{path}: want {want:?}, got {got:?}"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} files differ from the guard:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

/// The hand-checks name files of the guard and give a verdict each. A row
/// with any other verdict fails the run, so a row cannot be left as a note.
#[test]
fn hand_checks_name_guarded_files_and_give_a_verdict() {
    let guarded: Vec<&str> = tsv_rows(GUARD).iter().map(|r| r[0]).collect();
    let rows = tsv_rows(HAND_CHECKED);
    for cols in &rows {
        assert_eq!(cols.len(), 4, "a hand-check row has four columns: {cols:?}");
        assert!(
            guarded.contains(&cols[0]),
            "{} is not in the guard",
            cols[0]
        );
        assert!(
            ["ok", "divergence", "OPEN"]
                .iter()
                .any(|v| cols[3].starts_with(v)),
            "{}: the verdict is not ok, divergence or OPEN",
            cols[0]
        );
    }
    assert!(
        rows.len() >= 25,
        "{} hand-checked files; at least 25 are needed",
        rows.len()
    );
}

/// Why a file is not compared with ttconv.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Excluded {
    /// The reader refuses the file; the guard says why.
    Refused,
    /// Two of its cues overlap in time.
    Overlapping,
    /// It has a `span` with its own `begin`, `end` or `dur`.
    TimedSpans,
    /// It animates a style property (see [`Scan::style_animation`]).
    StyleAnimation,
    /// The reader skipped a paragraph with no end (its first skip is that one).
    /// A paragraph with no end is skipped and counted (12.4, the spec); ttconv
    /// keeps it open-ended, with a default ten-second end (`srt/writer.py`,
    /// `finish`), so the cues cannot match.
    NoEnd,
    /// It is listed in `DIVERGENT_FILES`.
    Listed,
}

/// The reason in `DIVERGENT_FILES` for `path`, if it is listed.
fn listed(path: &str) -> Option<&'static str> {
    DIVERGENT_FILES
        .iter()
        .find(|(file, _)| *file == path)
        .map(|(_, reason)| *reason)
}

/// Whether a line carries a character that is not white space.
fn has_text(line: &Line) -> bool {
    line.runs
        .iter()
        .any(|r| r.text.chars().any(|c| !c.is_whitespace()))
}

/// True if two cues that carry text overlap in time. Sorted by start, a cue
/// that starts before the latest end seen so far overlaps an earlier cue.
fn overlapping(cues: &[Cue]) -> bool {
    let mut spans: Vec<(u64, u64)> = cues
        .iter()
        .filter(|c| c.lines.iter().any(has_text))
        .map(|c| (c.start_ms, c.end_ms))
        .collect();
    spans.sort();
    let mut latest_end = 0;
    for (start, end) in spans {
        if start < latest_end {
            return true;
        }
        latest_end = latest_end.max(end);
    }
    false
}

/// What the document's markup holds that the comparison has to leave out.
#[derive(Default, Debug, PartialEq, Eq)]
struct Scan {
    /// A `span` with its own `begin`, `end` or `dur`. The reader slices a
    /// paragraph by such spans; ttconv does not slice it the same way.
    timed_spans: bool,
    /// A `set` or `animate` that changes a style property other than
    /// visibility, display or opacity. The reader does not play it and counts it
    /// as styling, while ttconv starts a new cue at each time the animation
    /// changes the style (its ISD adds the animation's times as significant
    /// times, `isd.py`, `compute_sig_times`), so the cues cannot be matched.
    style_animation: bool,
    /// A `div` inside a `div`. ttconv's WebVTT writer reads only the paragraphs
    /// that are direct children of a `div` (`vtt/writer.py`, `add_isd`), so a
    /// single paragraph inside nested divisions is lost from its output.
    div_in_div: bool,
}

/// The TTML styling namespaces (the TTML 2 and the DFXP one).
const STYLING_NS: [&str; 2] = [
    "http://www.w3.org/ns/ttml#styling",
    "http://www.w3.org/2006/10/ttaf1#styling",
];

/// Scans the markup for what the comparison leaves out (see [`Scan`]).
fn scan(bytes: &[u8]) -> Scan {
    let mut out = Scan::default();
    let Ok(text) = xml::decode(bytes) else {
        return out;
    };
    let mut reader = xml::Reader::new(&text, xml::Options::default());
    let mut divs_open = 0usize;
    while let Ok(Some(event)) = reader.next() {
        if let xml::Event::End(name) = &event
            && name.local == "div"
        {
            divs_open -= 1;
        }
        if let xml::Event::Start(element) = event {
            if element.name.local == "div" {
                out.div_in_div |= divs_open > 0;
                divs_open += 1;
            }
            let timed = ["begin", "end", "dur"]
                .iter()
                .any(|name| element.attr(None, name).is_some());
            if element.name.local == "span" && timed {
                out.timed_spans = true;
            }
            let animation = matches!(element.name.local, "set" | "animate" | "animation");
            let changes_style = element.attrs().any(|(name, _)| {
                name.ns.is_some_and(|ns| STYLING_NS.contains(&ns))
                    && !matches!(name.local, "display" | "visibility" | "opacity")
            });
            if animation && changes_style {
                out.style_animation = true;
            }
        }
    }
    out
}

/// The file's parsed cues, or why the file is not compared.
fn read_for_comparison(path: &str) -> Result<Parsed, Excluded> {
    if listed(path).is_some() {
        return Err(Excluded::Listed);
    }
    let bytes = fs::read(Path::new(ROOT).join(path)).expect("a vendored file");
    let parsed = read_ttml(&bytes).map_err(|_| Excluded::Refused)?;
    let scan = scan(&bytes);
    if scan.timed_spans {
        return Err(Excluded::TimedSpans);
    }
    if scan.style_animation {
        return Err(Excluded::StyleAnimation);
    }
    if overlapping(&parsed.cues) {
        return Err(Excluded::Overlapping);
    }
    if parsed
        .skipped
        .first
        .is_some_and(|(_, reason)| reason == "no end time")
    {
        return Err(Excluded::NoEnd);
    }
    Ok(parsed)
}

/// Style of a piece of text: italic, bold and underline. ttconv's `<i>`,
/// `<b>` and `<u>` and our runs carry the same three flags, in this order.
type Style = (bool, bool, bool);

/// One cue as the comparison sees it: its start and end in milliseconds, and
/// its lines, each a list of pieces of one style. A line with no text is not
/// kept: neither writer carries one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Shown {
    start: u64,
    end: u64,
    lines: Vec<Vec<(String, Style)>>,
}

/// Appends `text` in `style`, joined to the last piece when the style is the
/// same, so a tag that changes nothing never splits a piece.
fn push(out: &mut Vec<(String, Style)>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    match out.last_mut() {
        Some((last, last_style)) if *last_style == style => last.push_str(text),
        _ => out.push((text.to_owned(), style)),
    }
}

/// The pieces of one of our lines: its runs, with their flags.
fn pieces_of(line: &Line) -> Vec<(String, Style)> {
    let mut out = Vec::new();
    for run in &line.runs {
        push(&mut out, &run.text, (run.italic, run.bold, run.underline));
    }
    out
}

/// The length of the markup tag that starts `s`, if the comparison reads it as
/// a tag: the style tags ttconv writes (`<i>`, `<b>`, `<u>` and their closers),
/// and the colour and font tags it writes and that the reader drops and counts
/// (`<font …>`, `</font>`, `<c.…>`, `</c>`). Any other `<` is text.
fn tag_len(s: &str) -> Option<usize> {
    const FIXED: [&str; 8] = [
        "<i>", "</i>", "<b>", "</b>", "<u>", "</u>", "</font>", "</c>",
    ];
    if let Some(tag) = FIXED.iter().find(|t| s.starts_with(**t)) {
        return Some(tag.len());
    }
    if s.starts_with("<font ") || s.starts_with("<c.") {
        return s.find('>').map(|i| i + 1);
    }
    None
}

/// The pieces of a line of ttconv's output, read from its markup.
fn markup(line: &str) -> Vec<(String, Style)> {
    let mut out = Vec::new();
    let mut style: Style = (false, false, false);
    let mut rest = line;
    loop {
        let Some(at) = rest.find('<') else {
            push(&mut out, rest, style);
            return out;
        };
        push(&mut out, &rest[..at], style);
        let tail = &rest[at..];
        match tag_len(tail) {
            Some(len) => {
                match &tail[..len] {
                    "<i>" => style.0 = true,
                    "</i>" => style.0 = false,
                    "<b>" => style.1 = true,
                    "</b>" => style.1 = false,
                    "<u>" => style.2 = true,
                    "</u>" => style.2 = false,
                    _ => {}
                }
                rest = &tail[len..];
            }
            None => {
                push(&mut out, "<", style);
                rest = &tail[1..];
            }
        }
    }
}

/// The lines of ttconv's output that carry text.
fn text_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<Vec<(String, Style)>> {
    lines
        .map(markup)
        .filter(|l| l.iter().any(|(t, _)| t.chars().any(|c| !c.is_whitespace())))
        .collect()
}

/// `HH:MM:SS` then `sep` and three digits of milliseconds, as ttconv writes a
/// time (two or more hour digits, two minute and second digits, three
/// millisecond digits). None for anything else.
fn clock(s: &str, sep: char) -> Option<u64> {
    let (hms, ms) = s.trim().rsplit_once(sep)?;
    let mut parts = hms.split(':');
    let (h, m, sec) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || h.len() < 2 || m.len() != 2 || sec.len() != 2 || ms.len() != 3 {
        return None;
    }
    let h: u64 = h.parse().ok()?;
    let m: u64 = m.parse().ok()?;
    let sec: u64 = sec.parse().ok()?;
    let ms: u64 = ms.parse().ok()?;
    if m > 59 || sec > 59 {
        return None;
    }
    Some(((h * 60 + m) * 60 + sec) * 1000 + ms)
}

/// The cues of ttconv's SRT output. Its format: blocks separated by a blank
/// line, each a number counted from 1, a `start --> end` line, then the text
/// with `\n` line breaks.
fn ttconv_srt(text: &str) -> Result<Vec<Shown>, String> {
    let text = text.replace("\r\n", "\n");
    let body = text.trim_matches('\n');
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<Shown> = Vec::new();
    for block in body.split("\n\n") {
        let mut lines = block.lines();
        let number = lines.next().ok_or("an empty block")?;
        if number.parse::<usize>().ok() != Some(out.len() + 1) {
            return Err(format!(
                "block number {number:?} where {} is due",
                out.len() + 1
            ));
        }
        let timing = lines.next().ok_or("a block with no timing line")?;
        let (from, to) = timing
            .split_once(" --> ")
            .ok_or_else(|| format!("no timing in {timing:?}"))?;
        let start = clock(from, ',').ok_or_else(|| format!("bad start {from:?}"))?;
        let end = clock(to, ',').ok_or_else(|| format!("bad end {to:?}"))?;
        let lines = text_lines(lines);
        if !lines.is_empty() {
            out.push(Shown { start, end, lines });
        }
    }
    Ok(out)
}

/// The cues of ttconv's WebVTT output: a `WEBVTT` header, optional `STYLE`
/// blocks, then cues with an optional number line, a `start --> end` line
/// (settings may follow the end) and the text.
fn ttconv_vtt(text: &str) -> Result<Vec<Shown>, String> {
    let text = text.replace("\r\n", "\n");
    let body = text.trim_matches('\n');
    let mut blocks = body.split("\n\n");
    if blocks.next() != Some("WEBVTT") {
        return Err("no WEBVTT header".into());
    }
    let mut out = Vec::new();
    for block in blocks {
        if block.starts_with("STYLE\n") {
            continue;
        }
        let mut lines = block.lines();
        let first = lines.next().ok_or("an empty block")?;
        let timing = if first.contains(" --> ") {
            first
        } else {
            lines.next().ok_or("a cue number with no timing")?
        };
        let (from, rest) = timing
            .split_once(" --> ")
            .ok_or_else(|| format!("no timing in {timing:?}"))?;
        let to = rest.split_whitespace().next().ok_or("no end time")?;
        let start = clock(from, '.').ok_or_else(|| format!("bad start {from:?}"))?;
        let end = clock(to, '.').ok_or_else(|| format!("bad end {to:?}"))?;
        let lines = text_lines(lines);
        if !lines.is_empty() {
            out.push(Shown { start, end, lines });
        }
    }
    Ok(out)
}

/// Our cues as the comparison reads them: the file our writer produces, read
/// back by our reader. A file with no cue is `NoCues` and reads as empty.
fn ours(parsed: &Parsed, vtt: bool) -> Result<Vec<Shown>, String> {
    let mut cues = parsed.cues.clone();
    sort_cues(&mut cues);
    let mut buf = Vec::new();
    let back = if vtt {
        write_vtt(&cues, &mut buf).map_err(|e| e.to_string())?;
        read_vtt(&buf)
    } else {
        write_srt(&cues, &mut buf).map_err(|e| e.to_string())?;
        read_srt(&buf)
    };
    let back = match back {
        Ok(back) => back.cues,
        Err(ReadError::NoCues) => Vec::new(),
        Err(e) => return Err(format!("our reader refused our own output: {e}")),
    };
    Ok(back
        .iter()
        .map(|c| Shown {
            start: c.start_ms,
            end: c.end_ms,
            lines: c
                .lines
                .iter()
                .map(pieces_of)
                .filter(|l| l.iter().any(|(t, _)| t.chars().any(|c| !c.is_whitespace())))
                .collect(),
        })
        .filter(|s| !s.lines.is_empty())
        .collect())
}

/// Where two cue lists first differ, in words a log line can carry.
fn describe(ours: &[Shown], theirs: &[Shown]) -> String {
    if ours.len() != theirs.len() {
        return format!("{} cues from us, {} from ttconv", ours.len(), theirs.len());
    }
    match ours.iter().zip(theirs).position(|(a, b)| a != b) {
        Some(i) => format!("cue {i}: ours {:?}, ttconv {:?}", ours[i], theirs[i]),
        None => "equal".into(),
    }
}

/// True if `tt` runs and is ttconv: its help lists the `convert` command. A
/// missing tool is an error when `required`, and a skip with a note otherwise.
fn ttconv_present(required: bool) -> bool {
    let found = Command::new("tt")
        .arg("--help")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| {
            o.status.success() && String::from_utf8_lossy(&o.stdout).contains("convert")
        });
    if !found {
        assert!(!required, "tt (ttconv) is missing but {REQUIRE_TTCONV}=1");
        eprintln!("skipping: tt (ttconv 1.2.3) is not on PATH; CI runs this comparison");
    }
    found
}

/// Runs `tt convert` on `input` to the `otype` format and returns what it wrote.
fn run_tt(input: &Path, output: &Path, otype: &str) -> Result<String, String> {
    let run = Command::new("tt")
        .args(["convert", "-i"])
        .arg(input)
        .arg("-o")
        .arg(output)
        .args(["--itype", "TTML", "--otype", otype])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("tt does not start: {e}"))?;
    if !run.status.success() {
        let log = String::from_utf8_lossy(&run.stderr);
        let last = log.lines().last().unwrap_or("");
        return Err(format!("tt exits {:?}: {last}", run.status.code()));
    }
    fs::read_to_string(output).map_err(|e| format!("no output written: {e}"))
}

/// The classification, run everywhere: every file is refused, excluded with its
/// reason, or compared. It prints the counts, so a run shows how much of the set
/// the oracle covers. Every listed file must be vendored.
#[test]
fn ttconv_comparison_set_is_classified_without_the_tool() {
    let files = ttml_files();
    for (file, _) in DIVERGENT_FILES {
        assert!(
            files.iter().any(|f| f == file),
            "{file} is listed but not vendored"
        );
    }
    for (file, _) in VTT_DIVERGENT_FILES {
        assert!(
            read_for_comparison(file).is_ok(),
            "{file} is WebVTT-listed but not compared"
        );
        let found = scan(&fs::read(Path::new(ROOT).join(file)).expect("a vendored file"));
        assert!(
            found.div_in_div,
            "{file} is WebVTT-listed but has no div in a div"
        );
    }
    let mut counts: BTreeMap<Excluded, usize> = BTreeMap::new();
    let mut compared = 0;
    for path in &files {
        match read_for_comparison(path) {
            Ok(_) => compared += 1,
            Err(why) => *counts.entry(why).or_insert(0) += 1,
        }
    }
    eprintln!("ttconv comparison set: {compared} compared; excluded {counts:?}");
    assert!(
        compared >= COMPARED_FLOOR,
        "{compared} files compared; the floor is {COMPARED_FLOOR}"
    );
}

/// The ttconv comparison. For each file in the comparison set, ttconv writes
/// SRT and WebVTT; both are read back and compared with ours, cue by cue, by
/// start and end in milliseconds and by text with its italic, bold and
/// underline. Every difference is collected and reported at the end with its
/// file, so one run shows all of them.
#[test]
fn ttconv_agrees_on_times_and_text() {
    let required = std::env::var_os(REQUIRE_TTCONV).is_some_and(|v| v == "1");
    if !ttconv_present(required) {
        return;
    }
    let work = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ttconv");
    fs::create_dir_all(&work).expect("a scratch folder for ttconv's output");
    let mut compared = 0;
    let mut mismatches = Vec::new();
    for path in ttml_files() {
        let Ok(parsed) = read_for_comparison(&path) else {
            continue;
        };
        let input = Path::new(ROOT).join(&path);
        let stem = path.replace('/', "_");
        for (vtt, otype, ext) in [(false, "SRT", "srt"), (true, "VTT", "vtt")] {
            if vtt && VTT_DIVERGENT_FILES.iter().any(|(file, _)| *file == path) {
                continue;
            }
            let output = work.join(format!("{stem}.{ext}"));
            let theirs = run_tt(&input, &output, otype).and_then(|text| {
                if vtt {
                    ttconv_vtt(&text)
                } else {
                    ttconv_srt(&text)
                }
            });
            let theirs = match theirs {
                Ok(theirs) => theirs,
                Err(e) => {
                    mismatches.push(format!("{path} ({otype}): ttconv output: {e}"));
                    continue;
                }
            };
            match ours(&parsed, vtt) {
                Ok(ours) if ours == theirs => {}
                Ok(ours) => {
                    mismatches.push(format!("{path} ({otype}): {}", describe(&ours, &theirs)))
                }
                Err(e) => mismatches.push(format!("{path} ({otype}): {e}")),
            }
        }
        compared += 1;
    }
    eprintln!("ttconv comparison: {compared} files compared in SRT and VTT");
    assert!(
        compared >= COMPARED_FLOOR,
        "{compared} files compared; the floor is {COMPARED_FLOOR}"
    );
    assert!(
        mismatches.is_empty(),
        "{} differences from ttconv:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

/// The parser of ttconv's output, checked on hand-written samples of the shape
/// its writers produce (`srt/writer.py`, `srt/paragraph.py`, `vtt/writer.py`,
/// `vtt/cue.py`, `vtt/style.py`), so the parser is verified without the tool.
#[cfg(test)]
mod parser_self_test {
    use super::*;

    /// SRT: a number, a timing line in `HH:MM:SS,mmm`, text with `<i>`, `<b>`,
    /// `<u>` and `<font color>`, and a blank line between blocks.
    #[test]
    fn reads_the_srt_writer_shape() {
        let text = "1\n00:00:01,000 --> 00:00:02,500\n<font color=\"#ffffffff\"><i>Hi</i></font>\nsecond <b>line</b>\n\n2\n01:00:00,001 --> 01:00:03,000\n<u>under</u>\n";
        let cues = ttconv_srt(text).expect("the sample reads");
        assert_eq!(cues.len(), 2);
        assert_eq!((cues[0].start, cues[0].end), (1000, 2500));
        assert_eq!(
            cues[0].lines,
            vec![
                vec![("Hi".to_owned(), (true, false, false))],
                vec![
                    ("second ".to_owned(), (false, false, false)),
                    ("line".to_owned(), (false, true, false)),
                ],
            ]
        );
        assert_eq!((cues[1].start, cues[1].end), (3_600_001, 3_603_000));
        assert_eq!(
            cues[1].lines,
            vec![vec![("under".to_owned(), (false, false, true))]]
        );
    }

    /// WebVTT: the header, a STYLE block with a colour class, a cue number, a
    /// timing line (settings after the end time), and `<c.…>` colour markup.
    #[test]
    fn reads_the_vtt_writer_shape() {
        let text = "WEBVTT\n\nSTYLE\n::cue {\n  background-color: transparent;\n}\n::cue(.white) {\n  color: #ffffffff;\n}\n\n1\n00:00:01.000 --> 00:00:02.000\n<c.white><b>Hi</b></c>\n\n2\n00:00:03.250 --> 00:00:04.000 line:0%\nplain\n";
        let cues = ttconv_vtt(text).expect("the sample reads");
        assert_eq!(cues.len(), 2);
        assert_eq!((cues[0].start, cues[0].end), (1000, 2000));
        assert_eq!(
            cues[0].lines,
            vec![vec![("Hi".to_owned(), (false, true, false))]]
        );
        assert_eq!((cues[1].start, cues[1].end), (3250, 4000));
        assert_eq!(
            cues[1].lines,
            vec![vec![("plain".to_owned(), (false, false, false))]]
        );
    }

    /// A `<` that starts no known tag is text, so a difference in such text is
    /// seen by the comparison and not hidden by it.
    #[test]
    fn a_less_than_that_starts_no_known_tag_is_text() {
        assert_eq!(
            markup("a < b <span>c</span>"),
            vec![("a < b <span>c</span>".to_owned(), (false, false, false))]
        );
    }

    /// Times are read exactly, and a malformed one is refused, not guessed.
    #[test]
    fn clock_times_are_exact_and_strict() {
        assert_eq!(clock("00:00:01,500", ','), Some(1500));
        assert_eq!(clock("100:00:00.001", '.'), Some(360_000_001));
        assert_eq!(clock("00:61:00,000", ','), None, "minutes over 59");
        assert_eq!(clock("0:00:01,500", ','), None, "one hour digit");
        assert_eq!(clock("00:00:01,50", ','), None, "two millisecond digits");
        assert_eq!(clock("00:00:01.500", ','), None, "a dot for a comma");
    }

    /// A block number out of order is an error: a shifted or missing block
    /// cannot pass as a match.
    #[test]
    fn a_wrong_block_number_is_an_error() {
        assert!(ttconv_srt("2\n00:00:01,000 --> 00:00:02,000\nx\n").is_err());
    }

    /// The animation rule: a style animation is found, a visibility one is not.
    #[test]
    fn style_animation_is_scanned_and_visibility_is_not() {
        let style = br#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling"><body><div><p begin="0s" end="1s">x<set begin="1s" tts:color="red"/></p></div></body></tt>"#;
        let visibility = br#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling"><body><div><p begin="0s" end="1s">x<set begin="1s" tts:visibility="hidden"/></p></div></body></tt>"#;
        assert!(scan(style).style_animation);
        assert!(!scan(visibility).style_animation);
    }

    /// Touching cues do not overlap; cues that share time do.
    #[test]
    fn overlap_is_strict() {
        let cue = |start_ms, end_ms| Cue {
            start_ms,
            end_ms,
            top: false,
            lines: vec![Line {
                voice: None,
                runs: vec![Run {
                    text: "x".into(),
                    italic: false,
                    bold: false,
                    underline: false,
                }],
            }],
        };
        assert!(!overlapping(&[cue(0, 1000), cue(1000, 2000)]));
        assert!(overlapping(&[cue(0, 1500), cue(1000, 2000)]));
    }
}
