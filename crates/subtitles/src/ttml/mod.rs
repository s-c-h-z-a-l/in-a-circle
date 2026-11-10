//! The TTML reader (TTML2, https://www.w3.org/TR/ttml2/; the DFXP namespace of
//! TTML 1 is read too, as many files use it). One pass over the `xml` crate's events. The
//! reader keeps one paragraph's pieces at a time: a paragraph is cut into cues
//! only when it ends, because its timed spans decide where the cuts are.
//! `time.rs` reads the time expressions, `slice.rs` cuts a paragraph.
//!
//! Timing (12.2, 12.4, I.2.2). An element's `begin`, `end` and `dur` are
//! relative to its reference begin: the begin of its container for `par` (the
//! default), the active end of its previous sibling for `seq` (the first child
//! takes the container's begin). `end` and `dur` together take the earlier of
//! the two. Without an explicit end, a container ends with its children: at the
//! latest child end (`seq`: at the end of the last child). Text and `br` are
//! anonymous spans (12.4): zero long in `seq`, and in `par` as long as their
//! container, which is indefinite unless it has an end. Every end is clamped to
//! the earliest explicit end of the element and its ancestors. A paragraph that
//! is still indefinite has no end time and is skipped.
//!
//! Accounting. Nothing is dropped without a count. Each
//! construct the reader meets, and where its content goes:
//!
//! | Construct | Goes to |
//! |---|---|
//! | XML that is not well-formed, or nested past `MAX_DEPTH` | `Malformed` (the whole file) |
//! | a DOCTYPE with entity declarations | `Unsupported` (the whole file) |
//! | a root that is not `tt` in the TTML or DFXP namespace | `NotTtml` |
//! | `ttp:*` on the root (TTML or DFXP parameter namespace) | `Params`; an unconverted time base or drop mode, or a bad value: `Unsupported` |
//! | other `ttp:*`, `ttm:*` attributes, `xml:lang`, `xml:id` | no content: nothing |
//! | `head` and what is in it | ignored without a count; styling and layout are read later |
//! | `metadata` elements, elements of the metadata namespaces | ignored without a count: metadata is not content (14) |
//! | `body`, `div` | containers: timing. A bad time or `timeContainer` makes the paragraphs under it `skipped` |
//! | `p` | cues (`slice.rs`), or `skipped` with its line and reason |
//! | `span` | a container. A `span` with its own timing slices its paragraph |
//! | `br` | a line-break piece. Metadata in it is ignored without a count; other content is counted as below |
//! | text in `p` or `span`, visible | a piece. Visible text with no display time (zero long, or outside its container) makes its paragraph `skipped` once: `text with no display time` |
//! | text in `p` or `span`, white space only | spacing only: a piece with no time of its own, not counted |
//! | visible text directly in `tt`, `body`, `div` or `br` | `skipped`: `text outside any cue` |
//! | `begin`, `end`, `dur` with a bad value, or `end` before `begin` | a fault on the element: its paragraphs are `skipped` with the reason |
//! | `xml:space` | white-space mode (8.2.10); another value is a fault |
//! | `condition` on `body`, `div`, `p`, `span` or `br` | the element and its content: `skipped`, `conditional element` (conditions are not evaluated) |
//! | any other element in `body`, `div`, `p`, `span` or `br` (`set`, `animate`, `image`, foreign elements) | the element and its content: `skipped` once per element, `unsupported element`. A later change counts `set` and `animate` as `Styling`, and foreign content as `Elements` |
//! | `style`, `region`, `tts:*` attributes | not read yet: styles and regions will count `Styling` and `Position`, the rest later |
//! | element frames, pieces, copies of pieces in slices, lines and cues | the item budget: past it `TooComplex` |
//! | text copied into cues | `TooComplex` past `OUTPUT_BYTES` |
//!
//! Lines and runs follow the `Cue` model: a `br` or a line feed in `preserve`
//! mode ends a line, lines are trimmed, a line with no text is not kept.

mod slice;
mod time;

use std::collections::BTreeMap;

use xml::{Element, ErrorKind, Event, Options, QName, Reader};

use crate::srt::STRAY;
use crate::{Budget, Cue, MAX_MS, Parsed, ReadError, Skipped};
use slice::{Content, Piece};
use time::{Params, TimeError, parse_time};

/// A construct the reader does not convert. The whole document is refused with
/// it: converting around it would give a wrong result, and a refusal says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unsupported(pub(crate) &'static str);

/// The TTML namespace (Table 5-1) and the DFXP namespace of TTML 1's first
/// edition, which many files still carry.
const TT_NS: &str = "http://www.w3.org/ns/ttml";
const DFXP_NS: &str = "http://www.w3.org/2006/10/ttaf1";
/// The parameter namespaces: the `ttp:` attributes of the root (5.1).
const TTP_NS: &str = "http://www.w3.org/ns/ttml#parameter";
const DFXP_TTP_NS: &str = "http://www.w3.org/2006/10/ttaf1#parameter";
/// The metadata namespaces (14): their elements and attributes are metadata.
const TTM_NS: &str = "http://www.w3.org/ns/ttml#metadata";
const DFXP_TTM_NS: &str = "http://www.w3.org/2006/10/ttaf1#metadata";

/// Reasons a paragraph or an element is `skipped`. They are shown with the
/// line, so each says what the user can check in the file.
const BAD_TIMESTAMP: &str = "bad timestamp";
/// A time expression out of its field's range (12.3.1), or in a unit the
/// reader does not count (the attribute is refused, not the document).
const TIME_RANGE: &str = "time out of range";
/// An absolute time at or past 100 hours: [`MAX_MS`] is the largest one.
const LATE: &str = "time is 100 hours or more";
const ENDS_BEFORE_START: &str = "ends before it starts";
/// An element whose end is indefinite, or that follows one in its sequence.
const NO_END: &str = "no end time";
/// Visible text whose interval is empty: zero long (a `seq` text, 12.4.1) or
/// outside the container it is in.
const NO_DISPLAY: &str = "text with no display time";
const BAD_TIME_CONTAINER: &str = "invalid timeContainer";
const BAD_SPACE: &str = "invalid xml:space";
const CONDITIONAL: &str = "conditional element";
const UNSUPPORTED_ELEMENT: &str = "unsupported element";

/// Reads a whole TTML document. The `xml` crate decodes it (UTF-8, or UTF-16
/// with a byte order mark) and checks its declared encoding. Cues come out in
/// document order; the caller sorts them.
pub(crate) fn parse(bytes: &[u8], budget: &mut Budget) -> Result<Parsed, ReadError> {
    let input = xml::decode(bytes).map_err(xml_error)?;
    let mut reader = Reader::new(
        &input,
        Options {
            max_depth: xml::MAX_DEPTH,
            html_entities: false,
        },
    );
    let mut doc = Doc::new(budget);
    loop {
        let event = match reader.next() {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(e) => return Err(xml_error(e)),
        };
        // The line where the event began: the start of the element or text.
        let line = reader.line();
        match event {
            Event::Start(el) => doc.start(&el, line)?,
            Event::End(_) => doc.end()?,
            Event::Text(t) => doc.text(&t, line)?,
        }
    }
    Ok(doc.finish())
}

/// The `xml` crate's refusal as the reader's. A declared encoding other than
/// UTF-8 or UTF-16 is not a broken file, so it is `Unsupported`.
fn xml_error(e: xml::Error) -> ReadError {
    match e.kind {
        ErrorKind::NotWellFormed(reason) => ReadError::Malformed {
            line: e.line,
            reason,
        },
        ErrorKind::TooDeep => ReadError::Malformed {
            line: e.line,
            reason: "elements nested too deeply",
        },
        ErrorKind::UnsupportedEncoding(_) => {
            ReadError::Unsupported("declared encoding is not UTF-8 or UTF-16")
        }
        ErrorKind::EntityDeclarations => ReadError::Unsupported("entity declarations"),
    }
}

/// What an element is in its place in the document.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Root,
    Body,
    Div,
    Paragraph,
    Span,
    Break,
}

/// The open elements the reader converts, and the `br` frames that only
/// hold the place of a `br` until its end.
struct Frame {
    kind: Kind,
    /// The absolute begin, `None` when it cannot be placed.
    begin: Option<u64>,
    /// The earliest explicit end of this element and its ancestors; `None`
    /// when there is none.
    limit: Option<u64>,
    /// `timeContainer="seq"`.
    seq: bool,
    /// `seq`: the active end of the last child, or the begin before any child.
    cursor: Option<u64>,
    /// `par`: the latest active end of the children, `None` once one is
    /// indefinite.
    latest: Option<u64>,
    has_child: bool,
    /// The explicit end. `None` when neither `end` nor `dur` is given;
    /// `Some(None)` when one is given but cannot be placed.
    explicit: Option<Option<u64>>,
    /// `xml:space="preserve"` applies (8.2.10).
    preserve: bool,
    /// A bad attribute on this element or an ancestor: a paragraph under it
    /// is `skipped` with this reason.
    fault: Option<&'static str>,
}

impl Frame {
    fn bare(kind: Kind, preserve: bool) -> Frame {
        Frame {
            kind,
            begin: None,
            limit: None,
            seq: false,
            cursor: None,
            latest: None,
            has_child: false,
            explicit: None,
            preserve,
            fault: None,
        }
    }

    /// The reference begin of a child (I.2.2): the active end of the previous
    /// sibling in `seq`, else this element's begin.
    fn base(&self) -> Option<u64> {
        if self.seq { self.cursor } else { self.begin }
    }

    /// The interval of a text or `br` child, as `(begin, end)`. In `seq` it is
    /// zero long (12.4, anonymous span with implicit duration zero). In `par`
    /// it lasts as long as the container, whose end it takes.
    fn leaf_time(&self) -> (Option<u64>, Option<u64>) {
        let base = self.base();
        let own = if self.seq { base } else { None };
        (base, base.and(min_end(own, self.limit)))
    }

    /// This element's active end, clamped by its ancestors. The implicit end of
    /// a container is its children's (see the module doc).
    fn end(&self) -> Option<u64> {
        self.begin?;
        let own = match self.explicit {
            Some(end) => end,
            None if self.seq => self.cursor,
            None if self.has_child => self.latest,
            None => None,
        };
        min_end(own, self.limit)
    }

    /// A child has ended at `end`: the next child of a `seq` begins there, and
    /// a `par` container ends with its latest child.
    fn child_ended(&mut self, end: Option<u64>) {
        if self.seq {
            self.cursor = end;
        } else {
            self.latest = if self.has_child {
                match (self.latest, end) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    _ => None,
                }
            } else {
                end
            };
        }
        self.has_child = true;
    }
}

/// The earlier of two ends, where `None` is no end at all.
fn min_end(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

/// The paragraph being read. At most one is open: `p` does not nest.
struct Para {
    line: u32,
    pieces: Vec<Piece>,
    /// The paragraph has visible text (not white space).
    visible: bool,
    /// Visible text with no display time was read: it is not in `pieces`.
    hidden: bool,
    /// A fault on a `span` inside the paragraph.
    fault: Option<&'static str>,
}

/// What an element is, given the element it is in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Its content is metadata or `head` contents: ignored, not counted.
    Silent,
    /// Not converted: the element and its content are `skipped` with this reason.
    Refused(&'static str),
    Open(Kind),
    Break,
}

/// The element's role under a parent of kind `parent`.
fn role(parent: Kind, name: QName) -> Role {
    if is_metadata(name) {
        return Role::Silent;
    }
    let local = if is_tt(name) { name.local } else { "" };
    match (parent, local) {
        (Kind::Root, "head") => Role::Silent,
        (Kind::Root, "body") => Role::Open(Kind::Body),
        (Kind::Body | Kind::Div, "div") => Role::Open(Kind::Div),
        (Kind::Body | Kind::Div, "p") => Role::Open(Kind::Paragraph),
        (Kind::Paragraph | Kind::Span, "span") => Role::Open(Kind::Span),
        (Kind::Paragraph | Kind::Span, "br") => Role::Break,
        _ => Role::Refused(UNSUPPORTED_ELEMENT),
    }
}

fn is_tt(name: QName) -> bool {
    matches!(name.ns, Some(TT_NS | DFXP_NS))
}

fn is_metadata(name: QName) -> bool {
    matches!(name.ns, Some(TTM_NS | DFXP_TTM_NS)) || (is_tt(name) && name.local == "metadata")
}

/// A time attribute: absent, a time, or a value that cannot be used (its
/// reason is the fault of the element).
#[derive(Clone, Copy)]
enum Stamp {
    Absent,
    Time(u64),
    Bad(&'static str),
}

fn stamp(el: &Element, name: &str, params: &Params) -> Result<Stamp, ReadError> {
    let Some(expr) = el.attr(None, name) else {
        return Ok(Stamp::Absent);
    };
    match parse_time(expr, params) {
        Ok(ms) => Ok(Stamp::Time(ms)),
        Err(TimeError::Syntax) => Ok(Stamp::Bad(BAD_TIMESTAMP)),
        Err(TimeError::OutOfRange) => Ok(Stamp::Bad(TIME_RANGE)),
        Err(TimeError::Unsupported(Unsupported(reason))) => Err(ReadError::Unsupported(reason)),
    }
}

/// The absolute begin of an element and its explicit end (see [`Frame`]), from
/// its reference begin `base`: `begin` and `end` are relative to `base`, `dur`
/// to the begin (I.2.2, 12.2.2). A bad value is a fault on the element.
fn timing(
    base: Option<u64>,
    [begin, end, dur]: [Stamp; 3],
    fault: &mut Option<&'static str>,
) -> (Option<u64>, Option<Option<u64>>) {
    for stamp in [begin, end, dur] {
        if let Stamp::Bad(reason) = stamp {
            note(fault, reason);
        }
    }
    let at = |origin: Option<u64>, s: Stamp| match s {
        Stamp::Time(t) => origin.map(|o| o.saturating_add(t)),
        Stamp::Absent | Stamp::Bad(_) => None,
    };
    let begin_at = match begin {
        Stamp::Absent => base,
        s => at(base, s),
    };
    let end_at = at(base, end);
    let dur_at = at(begin_at, dur);
    // 12.2.2: with both, the active duration is the lesser of `dur` and `end`
    // minus the begin.
    let explicit = match (end, dur) {
        (Stamp::Absent, Stamp::Absent) => None,
        (Stamp::Absent, _) => Some(dur_at),
        (_, Stamp::Absent) => Some(end_at),
        _ => Some(end_at.zip(dur_at).map(|(e, d)| e.min(d))),
    };
    if let (Some(b), Some(Some(e))) = (begin_at, explicit)
        && e < b
    {
        note(fault, ENDS_BEFORE_START);
    }
    (begin_at, explicit)
}

/// The white-space mode of an element (8.2.10): its own `xml:space`, else the
/// inherited one. Any other value is a fault, and the inherited mode applies.
fn space(el: &Element, inherited: bool) -> (bool, Option<&'static str>) {
    match el.attr(Some(xml::XML_NS), "space") {
        None => (inherited, None),
        Some("preserve") => (true, None),
        Some("default") => (false, None),
        Some(_) => (inherited, Some(BAD_SPACE)),
    }
}

/// Sets the fault unless one is already set: the first one stands.
fn note(slot: &mut Option<&'static str>, reason: &'static str) {
    if slot.is_none() {
        *slot = Some(reason);
    }
}

/// The state of one document being read.
struct Doc<'b> {
    budget: &'b mut Budget,
    /// The `tt` parameters, from the root.
    params: Option<Params>,
    /// The open elements the reader converts, root first.
    stack: Vec<Frame>,
    para: Option<Para>,
    /// Open elements inside an ignored one, the ignored one included. Their
    /// events are not read.
    ignoring: usize,
    cues: Vec<Cue>,
    skipped: Skipped,
    /// Text copied into cues so far, in bytes (see `slice::append`).
    copied: u64,
}

impl<'b> Doc<'b> {
    fn new(budget: &'b mut Budget) -> Doc<'b> {
        Doc {
            budget,
            params: None,
            stack: Vec::new(),
            para: None,
            ignoring: 0,
            cues: Vec::new(),
            skipped: Skipped::default(),
            copied: 0,
        }
    }

    fn start(&mut self, el: &Element, line: u32) -> Result<(), ReadError> {
        if self.ignoring > 0 {
            self.ignoring += 1;
            return Ok(());
        }
        let Some(parent) = self.stack.last().map(|f| f.kind) else {
            return self.root(el);
        };
        let role = match role(parent, el.name) {
            // Conditions are not evaluated, so a conditional element is refused
            // whole rather than guessed at.
            Role::Open(_) | Role::Break if el.attr(None, "condition").is_some() => {
                Role::Refused(CONDITIONAL)
            }
            role => role,
        };
        match role {
            Role::Silent => self.ignoring = 1,
            Role::Refused(reason) => {
                self.skip(line, reason);
                self.ignoring = 1;
            }
            Role::Break => self.line_break()?,
            Role::Open(kind) => self.open(kind, el, line)?,
        }
        Ok(())
    }

    /// The root: `tt` in a TTML or DFXP namespace. Its `ttp:` parameters are
    /// read here, and the paragraphs are timed against its time base.
    fn root(&mut self, el: &Element) -> Result<(), ReadError> {
        if !(is_tt(el.name) && el.name.local == "tt") {
            return Err(ReadError::NotTtml);
        }
        let attrs: Vec<(&str, &str)> = el
            .attrs()
            .filter(|(q, _)| matches!(q.ns, Some(TTP_NS | DFXP_TTP_NS)))
            .map(|(q, v)| (q.local, v.as_ref()))
            .collect();
        let params = Params::from_tt(&attrs).map_err(|Unsupported(r)| ReadError::Unsupported(r))?;
        self.params = Some(params);
        let (preserve, bad_space) = space(el, false);
        self.budget.charge()?;
        let mut frame = Frame::bare(Kind::Root, preserve);
        // 12.2: the document's time begins at zero.
        frame.begin = Some(0);
        frame.cursor = Some(0);
        frame.fault = bad_space;
        self.stack.push(frame);
        Ok(())
    }

    /// Opens a `body`, `div`, `p` or `span`. Its time is resolved here from
    /// its parent; its end is known only at its end tag (see [`Frame::end`]).
    fn open(&mut self, kind: Kind, el: &Element, line: u32) -> Result<(), ReadError> {
        let params = self.params.ok_or(ReadError::NotTtml)?;
        let Some(parent) = self.stack.last() else {
            return Err(ReadError::NotTtml);
        };
        let (base, parent_limit, parent_preserve) = (parent.base(), parent.limit, parent.preserve);
        let mut fault = parent.fault;
        let begin = stamp(el, "begin", &params)?;
        let end = stamp(el, "end", &params)?;
        let dur = stamp(el, "dur", &params)?;
        let (begin_at, explicit) = timing(base, [begin, end, dur], &mut fault);
        let seq = match el.attr(None, "timeContainer") {
            None | Some("par") => false,
            Some("seq") => true,
            Some(_) => {
                note(&mut fault, BAD_TIME_CONTAINER);
                false
            }
        };
        let (preserve, bad_space) = space(el, parent_preserve);
        if let Some(reason) = bad_space {
            note(&mut fault, reason);
        }
        let mut frame = Frame::bare(kind, preserve);
        frame.begin = begin_at;
        frame.cursor = begin_at;
        frame.seq = seq;
        frame.limit = min_end(parent_limit, explicit.flatten());
        frame.explicit = explicit;
        frame.fault = fault;
        // Charged before the frame is pushed: an element is an item.
        self.budget.charge()?;
        if kind == Kind::Paragraph {
            self.para = Some(Para {
                line,
                pieces: Vec::new(),
                visible: false,
                hidden: false,
                fault: None,
            });
        }
        self.stack.push(frame);
        Ok(())
    }

    /// A `br` in a paragraph or span: a line break in the parent's interval.
    /// Its own content is metadata and is read by the frame that holds it.
    fn line_break(&mut self) -> Result<(), ReadError> {
        let Some(top) = self.stack.last() else {
            return Ok(());
        };
        let (base, end) = top.leaf_time();
        let preserve = top.preserve;
        self.child_ended(end);
        if let (Some(begin), Some(end)) = (base, end)
            && end > begin
        {
            self.budget.charge()?;
            self.push_piece(Piece {
                begin,
                end,
                content: Content::Break,
            });
        }
        self.budget.charge()?;
        self.stack.push(Frame::bare(Kind::Break, preserve));
        Ok(())
    }

    fn text(&mut self, text: &str, line: u32) -> Result<(), ReadError> {
        if self.ignoring > 0 {
            return Ok(());
        }
        let Some(top) = self.stack.last() else {
            return Ok(());
        };
        let (kind, preserve) = (top.kind, top.preserve);
        let (base, end) = top.leaf_time();
        let visible = text.chars().any(|c| !c.is_whitespace());
        if !matches!(kind, Kind::Paragraph | Kind::Span) {
            if visible {
                self.skip(line, STRAY);
            }
            return Ok(());
        }
        if visible {
            self.child_ended(end);
        }
        match (base, end) {
            (Some(begin), Some(end)) if end > begin => {
                self.budget.charge()?;
                let content = Content::Text {
                    text: slice::normalize(text, preserve),
                    preserve,
                };
                self.push_piece(Piece {
                    begin,
                    end,
                    content,
                });
            }
            _ if visible => {
                if let Some(para) = self.para.as_mut() {
                    para.hidden = true;
                }
            }
            _ => {}
        }
        if visible && let Some(para) = self.para.as_mut() {
            para.visible = true;
        }
        Ok(())
    }

    fn end(&mut self) -> Result<(), ReadError> {
        if self.ignoring > 0 {
            self.ignoring -= 1;
            return Ok(());
        }
        let Some(frame) = self.stack.pop() else {
            return Ok(());
        };
        // A `br`'s line break was placed at its start, and the root has no parent.
        if matches!(frame.kind, Kind::Root | Kind::Break) {
            return Ok(());
        }
        let end = frame.end();
        if frame.kind == Kind::Paragraph {
            self.paragraph(&frame, end)?;
        }
        if let (Some(reason), Some(para)) = (frame.fault, self.para.as_mut()) {
            note(&mut para.fault, reason);
        }
        self.child_ended(end);
        Ok(())
    }

    /// Cuts a finished paragraph into cues, or skips it with a reason. A
    /// paragraph with no visible text has nothing to lose and is not counted.
    fn paragraph(&mut self, frame: &Frame, end: Option<u64>) -> Result<(), ReadError> {
        let Some(para) = self.para.take() else {
            return Ok(());
        };
        if !para.visible {
            return Ok(());
        }
        if let Some(reason) = frame.fault.or(para.fault) {
            self.skip(para.line, reason);
            return Ok(());
        }
        let (Some(begin), Some(end)) = (frame.begin, end) else {
            self.skip(para.line, NO_END);
            return Ok(());
        };
        if begin > MAX_MS || end > MAX_MS {
            self.skip(para.line, LATE);
            return Ok(());
        }
        if end <= begin {
            self.skip(para.line, NO_DISPLAY);
            return Ok(());
        }
        slice::cues(
            &para.pieces,
            begin,
            end,
            self.budget,
            &mut self.copied,
            &mut self.cues,
        )?;
        if para.hidden {
            self.skip(para.line, NO_DISPLAY);
        }
        Ok(())
    }

    /// Stores a piece of the open paragraph. The caller has charged it.
    fn push_piece(&mut self, piece: Piece) {
        if let Some(para) = self.para.as_mut() {
            para.pieces.push(piece);
        }
    }

    fn child_ended(&mut self, end: Option<u64>) {
        if let Some(top) = self.stack.last_mut() {
            top.child_ended(end);
        }
    }

    /// Counts one `skipped` item: the first one kept is the earliest by line.
    fn skip(&mut self, line: u32, reason: &'static str) {
        self.skipped.count += 1;
        let line = line as usize;
        if self.skipped.first.is_none_or(|(first, _)| line < first) {
            self.skipped.first = Some((line, reason));
        }
    }

    fn finish(self) -> Parsed {
        Parsed {
            cues: self.cues,
            losses: BTreeMap::new(),
            skipped: self.skipped,
        }
    }
}
