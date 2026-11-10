//! The TTML text oracle: a structure-aware generator of documents and a
//! brute-force model of what the reader must return for them. Shared by the
//! fuzz target (`fuzz/fuzz_targets/ttml.rs`, whose input bytes make the
//! choices) and the normal test `crates/subtitles/tests/ttml.rs` (a fixed-seed
//! generator), so the property runs on every `cargo test`.
//!
//! The generator writes one document: an optional `begin`, `end` and `dur` on
//! the body and on the div, the div as `par` or `seq`, one to three paragraphs
//! (each with optional timing, and `xml:space="preserve"` on some), and up to
//! [`ITEMS`] items per paragraph: words, `br`, white space between the items
//! (none, one space, several spaces, a line feed and a tab), and `span`s with
//! their own timing and italic, nested up to [`MAX_DEPTH`] deep. Times are
//! multiples of [`STEP_MS`], written as milliseconds, seconds or frames.
//! Every `end` is at or after its `begin`, so the document has no timing
//! fault: the faults are the unit tests' business.
//!
//! The model reads the generated tree and never the XML, and it calls no
//! function of the reader. It applies TTML2 (12.2 timing, 8.2.10 white space).
//! Where the standard leaves a choice open, it takes the choice the reader
//! documents (the accounting table of `ttml/mod.rs`):
//!  - white space and `br` are anonymous spans: they do not decide their
//!    container's end, but they are shown as long as the container is;
//!  - a word or white space lasts as long as its container's earliest explicit
//!    end (the paragraph's, when it has one); with none, it is open and lasts to
//!    the paragraph's end;
//!  - a timed span decides its container's end, even an empty one;
//!  - visible text with an empty interval is not shown, and its paragraph is
//!    `skipped` once (`text with no display time`), after the cues of the rest;
//!  - a paragraph with no end time is `skipped` once, with no cue.
//! A cue is one elementary interval of a paragraph (between two boundaries of
//! its pieces) that has text. Neighbours with the same text are not merged.
//!
//! The exact comparison covers the whole grammar above. The checks are: (1) each
//! visible word of a usable paragraph is in a cue on screen while it is visible,
//! (2) and (3) the lines of each cue, so the white space between words and the
//! line breaks, and (4) the intervals and the count of skipped paragraphs.

use subtitles::{ReadError, read_ttml};

/// Frames a second of an `f` time: the TTML2 default (`ttp:frameRate`, 30).
const FPS: u64 = 30;
/// Every generated time is a multiple of this, in milliseconds: 3 frames, so a
/// frame count is whole and the model's arithmetic is exact.
pub const STEP_MS: u64 = 100;
/// The words: ASCII tokens, and two multi-byte ones.
const WORDS: [&str; 12] = [
    "w0", "w1", "w2", "w3", "w4", "w5", "w6", "w7", "w8", "w9", "é", "日本",
];
/// The white space between items: none, one space, several spaces, a line
/// feed and a tab.
const GAPS: [&str; 4] = ["", " ", "   ", "\n\t"];
/// Items in one paragraph, spans' items included: keeps a document small.
const ITEMS: u32 = 24;
/// Span nesting below the paragraph: spans at depths 1 to `MAX_DEPTH`.
const MAX_DEPTH: u32 = 3;

/// How a time is written. The value is the same in all three.
#[derive(Clone, Copy, Debug)]
pub enum Form {
    Ms,
    Seconds,
    Frames,
}

#[derive(Clone, Copy, Debug)]
pub struct Stamp {
    pub ms: u64,
    pub form: Form,
}

/// The timing attributes of an element. `end` and `dur` are as written: `end`
/// relative to the reference begin, `dur` to the element's own begin.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timing {
    pub begin: Option<Stamp>,
    pub end: Option<Stamp>,
    pub dur: Option<Stamp>,
}

#[derive(Clone, Debug)]
pub enum Node {
    /// A visible word.
    Word(&'static str),
    /// White space, as written (never empty).
    Space(&'static str),
    Br,
    Span(Span),
}

#[derive(Clone, Debug)]
pub struct Span {
    pub timing: Timing,
    pub italic: bool,
    pub children: Vec<Node>,
}

#[derive(Clone, Debug)]
pub struct Para {
    pub timing: Timing,
    pub preserve: bool,
    pub children: Vec<Node>,
}

#[derive(Clone, Debug)]
pub struct Doc {
    pub body: Timing,
    pub div: Timing,
    /// The div's paragraphs are a `seq` (each begins where the last ends).
    pub seq: bool,
    pub paras: Vec<Para>,
}

/// Writes a document. `pick(n)` gives a choice in `0..n`; a fixed sequence of
/// choices gives the same document every time.
pub fn document(pick: &mut dyn FnMut(u32) -> u32) -> Doc {
    let body = timing(pick);
    let div = timing(pick);
    let seq = pick(4) == 0;
    let count = 1 + pick(3);
    let mut paras = Vec::new();
    for _ in 0..count {
        let (timing, children) = if pick(3) == 0 {
            timed_runs(pick)
        } else {
            // Most paragraphs get an end, so the cues are not all skipped; the
            // rest (about one in five) have none, which is the review's shape.
            let mut timing = timing(pick);
            if timing.end.is_none() && pick(3) != 0 {
                let from = timing.begin.map_or(0, |s| s.ms);
                timing.end = Some(stamp(from + u64::from(pick(41)) * STEP_MS, pick));
            }
            let mut budget = ITEMS;
            (timing, nodes(pick, 0, &mut budget))
        };
        let preserve = pick(3) == 0;
        paras.push(Para {
            timing,
            preserve,
            children,
        });
    }
    Doc {
        body,
        div,
        seq,
        paras,
    }
}

/// Timed spans with white space or a `br` between them, one word each, and
/// often no end on the paragraph: the shape of the review's findings (R1 and
/// R2). Each span begins in the first 3 s and lasts up to 3 s, so two of them
/// often overlap, which is where white space between words can be lost.
fn timed_runs(pick: &mut dyn FnMut(u32) -> u32) -> (Timing, Vec<Node>) {
    let mut children = Vec::new();
    for i in 0..2 + pick(3) {
        if i > 0 {
            if pick(4) == 0 {
                children.push(Node::Br);
            } else {
                gap(pick, &mut children);
            }
        }
        let begin = u64::from(pick(31)) * STEP_MS;
        let end = begin + u64::from(pick(31)) * STEP_MS;
        children.push(Node::Span(Span {
            timing: Timing {
                begin: Some(stamp(begin, pick)),
                end: Some(stamp(end, pick)),
                dur: None,
            },
            italic: pick(3) == 0,
            children: vec![Node::Word(WORDS[pick(WORDS.len() as u32) as usize])],
        }));
    }
    let end = (pick(2) == 0).then(|| stamp(u64::from(pick(41)) * STEP_MS, pick));
    let timing = Timing {
        begin: None,
        end,
        dur: None,
    };
    (timing, children)
}

/// A begin of 0 to 6 s, an end of 0 to 4 s after the begin, a duration of 0 to
/// 4 s: each present one time in two or three or four.
fn timing(pick: &mut dyn FnMut(u32) -> u32) -> Timing {
    let begin = (pick(2) == 0).then(|| u64::from(pick(61)) * STEP_MS);
    let from = begin.unwrap_or(0);
    let end = (pick(3) == 0).then(|| from + u64::from(pick(41)) * STEP_MS);
    let dur = (pick(4) == 0).then(|| u64::from(pick(41)) * STEP_MS);
    Timing {
        begin: begin.map(|ms| stamp(ms, pick)),
        end: end.map(|ms| stamp(ms, pick)),
        dur: dur.map(|ms| stamp(ms, pick)),
    }
}

fn stamp(ms: u64, pick: &mut dyn FnMut(u32) -> u32) -> Stamp {
    let form = match pick(3) {
        0 => Form::Ms,
        1 => Form::Frames,
        _ => Form::Seconds,
    };
    Stamp { ms, form }
}

/// The items of one container: 1 to 8 of them, white space between them and
/// around them. `budget` counts the paragraph's items, spans' included.
fn nodes(pick: &mut dyn FnMut(u32) -> u32, depth: u32, budget: &mut u32) -> Vec<Node> {
    let mut out = Vec::new();
    let count = 1 + pick(8);
    gap(pick, &mut out);
    for i in 0..count {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        if i > 0 {
            gap(pick, &mut out);
        }
        let node = match pick(10) {
            7 => Node::Br,
            5 | 6 | 8 if depth < MAX_DEPTH => Node::Span(span(pick, depth + 1, budget)),
            _ => Node::Word(WORDS[pick(WORDS.len() as u32) as usize]),
        };
        out.push(node);
    }
    gap(pick, &mut out);
    out
}

fn gap(pick: &mut dyn FnMut(u32) -> u32, out: &mut Vec<Node>) {
    let text = GAPS[pick(GAPS.len() as u32) as usize];
    if !text.is_empty() {
        out.push(Node::Space(text));
    }
}

fn span(pick: &mut dyn FnMut(u32) -> u32, depth: u32, budget: &mut u32) -> Span {
    let timing = timing(pick);
    let italic = pick(3) == 0;
    let children = nodes(pick, depth, budget);
    Span {
        timing,
        italic,
        children,
    }
}

impl Stamp {
    fn xml(self) -> String {
        match self.form {
            Form::Seconds if self.ms.is_multiple_of(1000) => format!("{}s", self.ms / 1000),
            Form::Frames => format!("{}f", self.ms * FPS / 1000),
            _ => format!("{}ms", self.ms),
        }
    }
}

impl Doc {
    /// The document as XML: the root declares TTML and the styling namespace
    /// (`tts:fontStyle` on the italic spans).
    pub fn xml(&self) -> String {
        let mut x = String::from(
            r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling"><body"#,
        );
        timing_attrs(&mut x, self.body);
        x.push_str("><div");
        timing_attrs(&mut x, self.div);
        if self.seq {
            x.push_str(r#" timeContainer="seq""#);
        }
        x.push('>');
        for para in &self.paras {
            x.push_str("<p");
            timing_attrs(&mut x, para.timing);
            if para.preserve {
                x.push_str(r#" xml:space="preserve""#);
            }
            x.push('>');
            nodes_xml(&para.children, &mut x);
            x.push_str("</p>\n");
        }
        x.push_str("</div></body></tt>");
        x
    }
}

fn timing_attrs(x: &mut String, t: Timing) {
    for (name, stamp) in [("begin", t.begin), ("end", t.end), ("dur", t.dur)] {
        if let Some(stamp) = stamp {
            x.push_str(&format!(r#" {name}="{}""#, stamp.xml()));
        }
    }
}

fn nodes_xml(nodes: &[Node], x: &mut String) {
    for node in nodes {
        match node {
            Node::Word(word) => x.push_str(word),
            Node::Space(space) => x.push_str(space),
            Node::Br => x.push_str("<br/>"),
            Node::Span(span) => {
                x.push_str("<span");
                timing_attrs(x, span.timing);
                if span.italic {
                    x.push_str(r#" tts:fontStyle="italic""#);
                }
                x.push('>');
                nodes_xml(&span.children, x);
                x.push_str("</span>");
            }
        }
    }
}

/// What the reader must return for a document.
#[derive(Default)]
pub struct Expected {
    /// The cues, in document order: start, end, and each line's text.
    pub cues: Vec<(u64, u64, Vec<String>)>,
    /// The paragraphs skipped, each counted once.
    pub skipped: usize,
    /// The visible words of the usable paragraphs: the word, and the interval
    /// where it is visible.
    pub words: Vec<(&'static str, u64, u64)>,
}

/// A piece of a paragraph's text, in document order: the interval it is shown
/// over (`end` `None` is open: the paragraph's end) and what it is.
#[derive(Clone, Copy)]
struct Piece {
    begin: u64,
    end: Option<u64>,
    kind: Kind,
}

#[derive(Clone, Copy)]
enum Kind {
    /// Text: a word, or white space, in the paragraph's white-space mode.
    Text {
        text: &'static str,
        preserve: bool,
        word: bool,
    },
    Break,
}

/// What a paragraph's content gives: its pieces, whether it has visible text,
/// and whether some visible text has no display time.
#[derive(Default)]
struct Gathered {
    pieces: Vec<Piece>,
    visible: bool,
    hidden: bool,
}

/// The scope a container's items are read in: the container's begin, its
/// earliest explicit end (its own or an ancestor's), and its white-space mode.
#[derive(Clone, Copy)]
struct Scope {
    begin: Option<u64>,
    limit: Option<u64>,
    preserve: bool,
}

/// An element's resolved begin, and its end (`None`: indefinite).
struct Element {
    begin: Option<u64>,
    end: Option<u64>,
}

/// TTML2 12.2: `begin` and `end` are relative to `base`, the reference begin,
/// and `dur` is relative to the element's own begin. The explicit end is the
/// earlier of `end` and `begin + dur` when both are given. The second value is
/// `None` when no end is given, `Some(None)` when one is given but has no value.
fn resolve(base: Option<u64>, t: Timing) -> (Option<u64>, Option<Option<u64>>) {
    let begin = match t.begin {
        None => base,
        Some(s) => base.map(|b| b + s.ms),
    };
    let end = t.end.map(|s| base.map(|b| b + s.ms));
    let dur = t.dur.map(|s| begin.map(|b| b + s.ms));
    let explicit = match (end, dur) {
        (None, None) => None,
        (None, Some(d)) => Some(d),
        (Some(e), None) => Some(e),
        (Some(e), Some(d)) => Some(e.zip(d).map(|(e, d)| e.min(d))),
    };
    (begin, explicit)
}

/// The earlier of two ends, where `None` is no end: it does not limit.
fn min_end(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        _ => a.or(b),
    }
}

/// An element with its content: its end is its explicit end, else the latest
/// end of its children (a child with no end makes it open), clamped by the
/// earliest explicit end on it or above it. The content's pieces go to `g`.
fn element(
    t: Timing,
    base: Option<u64>,
    parent_limit: Option<u64>,
    children: &[Node],
    preserve: bool,
    g: &mut Gathered,
) -> Element {
    let (begin, explicit) = resolve(base, t);
    let limit = min_end(parent_limit, explicit.flatten());
    let latest = walk(
        children,
        Scope {
            begin,
            limit,
            preserve,
        },
        g,
    );
    let own = match explicit {
        Some(end) => end,
        None => latest.flatten(),
    };
    let end = begin.and(min_end(own, limit));
    Element { begin, end }
}

/// The items of a container, in document order. Returns the container's latest
/// child end: `None` when no child decides it (no timed span and no visible
/// word), else `Some(end)`, where `end` is `None` when one child is open.
/// White space and `br` do not decide it.
fn walk(nodes: &[Node], scope: Scope, g: &mut Gathered) -> Option<Option<u64>> {
    let mut latest: Option<Option<u64>> = None;
    for node in nodes {
        let decides = match *node {
            Node::Word(word) => text(word, true, scope, g),
            Node::Space(space) => text(space, false, scope, g),
            Node::Br => {
                line_break(scope, g);
                None
            }
            Node::Span(ref span) => {
                let el = element(
                    span.timing,
                    scope.begin,
                    scope.limit,
                    &span.children,
                    scope.preserve,
                    g,
                );
                Some(el.end)
            }
        };
        if let Some(end) = decides {
            latest = Some(match latest {
                None => end,
                Some(prev) => match (prev, end) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    _ => None,
                },
            });
        }
    }
    latest
}

/// A text piece: a word is visible and white space is not. Its interval is the
/// container's; it is kept when the interval is not empty. Visible text with an
/// empty interval is hidden, which `expected` counts. Returns the container's
/// end for a visible word, which it decides.
fn text(text: &'static str, word: bool, scope: Scope, g: &mut Gathered) -> Option<Option<u64>> {
    let end = scope.begin.and(scope.limit);
    match scope.begin {
        Some(begin) if end.is_none_or(|e| e > begin) => g.pieces.push(Piece {
            begin,
            end,
            kind: Kind::Text {
                text,
                preserve: scope.preserve,
                word,
            },
        }),
        _ if word => g.hidden = true,
        _ => {}
    }
    if word {
        g.visible = true;
        Some(end)
    } else {
        None
    }
}

/// A `br` is a line break over its container's interval, when that is not
/// empty. It decides nothing.
fn line_break(scope: Scope, g: &mut Gathered) {
    let end = scope.begin.and(scope.limit);
    if let Some(begin) = scope.begin
        && end.is_none_or(|e| e > begin)
    {
        g.pieces.push(Piece {
            begin,
            end,
            kind: Kind::Break,
        });
    }
}

/// The model's result for a document (see [`Expected`]).
pub fn expected(doc: &Doc) -> Expected {
    let (body_begin, body_explicit) = resolve(Some(0), doc.body);
    let body_limit = min_end(None, body_explicit.flatten());
    let (div_begin, div_explicit) = resolve(body_begin, doc.div);
    let div_limit = min_end(body_limit, div_explicit.flatten());
    let mut cursor = div_begin;
    let mut want = Expected::default();
    for para in &doc.paras {
        let base = if doc.seq { cursor } else { div_begin };
        let mut g = Gathered::default();
        let el = element(
            para.timing,
            base,
            div_limit,
            &para.children,
            para.preserve,
            &mut g,
        );
        if doc.seq {
            cursor = el.end;
        }
        if !g.visible {
            continue;
        }
        match (el.begin, el.end) {
            (Some(begin), Some(end)) if end > begin => {
                want.cues.extend(slices(&g.pieces, begin, end));
                want.skipped += usize::from(g.hidden);
                want.words.extend(visible_words(&g.pieces, begin, end));
            }
            // No end time, or an interval that is empty: skipped, once.
            _ => want.skipped += 1,
        }
    }
    want
}

/// The cues of a paragraph shown over `[begin, end)`: one per elementary
/// interval, between two boundaries of its pieces, that has text.
fn slices(pieces: &[Piece], begin: u64, end: u64) -> Vec<(u64, u64, Vec<String>)> {
    let mut cuts = vec![begin, end];
    for p in pieces {
        cuts.push(p.begin.clamp(begin, end));
        cuts.push(p.end.unwrap_or(end).clamp(begin, end));
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut cues = Vec::new();
    for pair in cuts.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        let active: Vec<&Piece> = pieces
            .iter()
            .filter(|p| p.begin <= from && from < p.end.unwrap_or(end))
            .collect();
        let lines = line_texts(&active);
        if !lines.is_empty() {
            cues.push((from, to, lines));
        }
    }
    cues
}

/// The lines of the text shown together, the pieces in document order. A `br`
/// ends a line. In a `preserve` paragraph a line feed ends one and a tab is a
/// space; elsewhere each run of white space is one space, and a space after a
/// space does not repeat (8.2.10). A line is trimmed, and a line with no text
/// is dropped.
fn line_texts(active: &[&Piece]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for piece in active {
        match piece.kind {
            Kind::Break => lines.push(std::mem::take(&mut line)),
            Kind::Text { text, preserve, .. } => {
                for c in text.chars() {
                    match (c, preserve) {
                        ('\n', true) => lines.push(std::mem::take(&mut line)),
                        ('\t', true) => line.push(' '),
                        (' ' | '\t' | '\n', false) => {
                            if !line.ends_with(' ') {
                                line.push(' ');
                            }
                        }
                        _ => line.push(c),
                    }
                }
            }
        }
    }
    lines.push(line);
    lines
        .iter()
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

/// The visible words of a paragraph shown over `[begin, end)`, each with the
/// interval it is visible in (open pieces last to the paragraph's end).
fn visible_words(pieces: &[Piece], begin: u64, end: u64) -> Vec<(&'static str, u64, u64)> {
    pieces
        .iter()
        .filter_map(|p| match p.kind {
            Kind::Text {
                text, word: true, ..
            } => {
                let (from, to) = (p.begin.max(begin), p.end.unwrap_or(end).min(end));
                (from < to).then_some((text, from, to))
            }
            _ => None,
        })
        .collect()
}

/// A word, a line break and an italic span, each of a paragraph with a given
/// shape: the review's findings and one white-space case, as named documents.
/// R1: white space between two timed spans, in a paragraph with no end. R2:
/// two timed spans with a `br` between them, in a paragraph with no end. P1:
/// a preserved paragraph whose first run is white space, then an italic run
/// that starts with spaces (the line must still be trimmed).
pub fn named() -> Vec<(&'static str, Doc)> {
    vec![
        (
            "R1 white space between timed spans, no paragraph end",
            one_para(
                false,
                Timing::default(),
                vec![
                    timed("w1", 1000, 2000),
                    Node::Space(" "),
                    timed("w2", 1000, 2000),
                ],
            ),
        ),
        (
            "R2 timed spans with a br between them, no paragraph end",
            one_para(
                false,
                Timing::default(),
                vec![timed("w1", 1000, 2000), Node::Br, timed("w2", 1000, 2000)],
            ),
        ),
        (
            "P1 preserved line, first run white space, italic run with spaces",
            one_para(
                true,
                Timing {
                    end: ms(1000),
                    ..Timing::default()
                },
                vec![
                    Node::Space(" "),
                    Node::Span(Span {
                        timing: Timing::default(),
                        italic: true,
                        children: vec![Node::Space("  "), Node::Word("w1")],
                    }),
                ],
            ),
        ),
    ]
}

fn ms(ms: u64) -> Option<Stamp> {
    Some(Stamp { ms, form: Form::Ms })
}

fn timed(word: &'static str, begin: u64, end: u64) -> Node {
    Node::Span(Span {
        timing: Timing {
            begin: ms(begin),
            end: ms(end),
            dur: None,
        },
        italic: false,
        children: vec![Node::Word(word)],
    })
}

fn one_para(preserve: bool, timing: Timing, children: Vec<Node>) -> Doc {
    Doc {
        body: Timing::default(),
        div: Timing::default(),
        seq: false,
        paras: vec![Para {
            timing,
            preserve,
            children,
        }],
    }
}

/// The text of a cue's line: its runs joined.
fn line_text(line: &subtitles::Line) -> String {
    line.runs.iter().map(|run| run.text.as_str()).collect()
}

/// Checks the reader on `doc` against the model. Panics on the first
/// difference, and the message carries the document.
pub fn check(doc: &Doc) {
    let xml = doc.xml();
    let want = expected(doc);
    let got = read_ttml(xml.as_bytes());
    assert_eq!(
        got,
        read_ttml(xml.as_bytes()),
        "the verdict changed on {xml}"
    );
    if want.cues.is_empty() {
        let refused = match &got {
            Err(ReadError::NoCues) => want.skipped == 0,
            Err(ReadError::AllSkipped { count, .. }) => want.skipped > 0 && *count == want.skipped,
            _ => false,
        };
        assert!(
            refused,
            "want no cue and {} skipped, got {got:?} for {xml}",
            want.skipped
        );
        return;
    }
    let parsed = match got {
        Ok(parsed) => parsed,
        Err(e) => panic!("want cues, got {e:?} for {xml}"),
    };
    // (1) Conservation: each visible word of a usable paragraph is in a cue
    // that is on screen while the word is visible.
    for (word, from, to) in &want.words {
        let kept = parsed.cues.iter().any(|c| {
            c.start_ms < *to
                && *from < c.end_ms
                && c.lines.iter().any(|line| line_text(line).contains(*word))
        });
        assert!(
            kept,
            "word {word} visible in [{from}, {to}) is in no cue of {xml}"
        );
    }
    assert_eq!(
        parsed.skipped.count, want.skipped,
        "skipped paragraphs of {xml}"
    );
    // (4) The intervals of the cues, in document order.
    let spans: Vec<(u64, u64)> = parsed.cues.iter().map(|c| (c.start_ms, c.end_ms)).collect();
    let want_spans: Vec<(u64, u64)> = want.cues.iter().map(|c| (c.0, c.1)).collect();
    assert_eq!(spans, want_spans, "cue intervals of {xml}");
    // (2) and (3) The lines of each cue: the white space between words and the
    // line breaks.
    let lines: Vec<Vec<String>> = parsed
        .cues
        .iter()
        .map(|c| c.lines.iter().map(line_text).collect())
        .collect();
    let want_lines: Vec<Vec<String>> = want.cues.iter().map(|c| c.2.clone()).collect();
    assert_eq!(lines, want_lines, "cue lines of {xml}");
}
