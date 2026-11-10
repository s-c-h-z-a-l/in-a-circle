//! The TTML reader (TTML2, https://www.w3.org/TR/ttml2/; the DFXP namespace of
//! TTML 1 is read too, as many files use it). One pass over the `xml` crate's events. The
//! reader keeps one paragraph's pieces at a time: a paragraph is cut into cues
//! only when it ends, because its timed spans decide where the cuts are.
//! `time.rs` reads the time expressions, `slice.rs` cuts a paragraph, and
//! `style.rs` holds the styles and regions of the head.
//!
//! Timing (12.2, 12.4, I.2.2). An element's `begin`, `end` and `dur` are
//! relative to its reference begin: the begin of its container for `par` (the
//! default), the active end of its previous sibling for `seq` (the first child
//! takes the container's begin). `end` and `dur` together take the earlier of
//! the two. Without an explicit end, a container ends with its children: at the
//! latest child end (`seq`: at the end of the last child). Text and `br` are
//! anonymous spans (12.4): zero long in `seq`, and in `par` as long as their
//! container, which is indefinite unless it has an end. Such a piece does not
//! decide its container's end: the container ends with its timed children, so
//! white space or a `br` after the last timed line does not make a paragraph
//! without an end time (the endsync `all` of 12.2.4 would, read literally).
//! Every end is clamped to the earliest explicit end of the element and its
//! ancestors. A paragraph that is still indefinite has no end time and is skipped.
//!
//! Styles (10.4). A style is referenced by a `style` attribute (a list of ids,
//! merged in order, the later one winning), and its own declarations come last.
//! The inheritable values (italic, bold, underline, line-through, overline) pass
//! from an element to its content (10.4.2.1), and a paragraph takes those its
//! region gives where no ancestor does (10.4.2.2). A paragraph's region is the
//! `region` of the paragraph or of its nearest ancestor; `top` (see [`top_third`])
//! is decided by that region's placement.
//!
//! Accounting. Nothing is dropped without a count. Each construct the reader
//! meets, and where its content goes:
//!
//! | Construct | Goes to |
//! |---|---|
//! | XML that is not well-formed, or nested past `MAX_DEPTH` | `Malformed` (the whole file) |
//! | a DOCTYPE with entity declarations; a declared encoding other than UTF-8 or UTF-16 | `Unsupported` (the whole file) |
//! | a root that is not `tt` in the TTML or DFXP namespace | `NotTtml` (the whole file) |
//! | `ttp:timeBase="clock"`, a `wallclock(...)` time, a drop-frame `ttp:dropMode` | `Unsupported` (the whole file) |
//! | `ttp:*` on the root (TTML or DFXP parameter namespace) | `Params`; an unconverted time base or drop mode, or a bad value: `Unsupported` |
//! | other `ttp:*`, `ttm:*` attributes, `xml:lang`, `xml:id` on elements other than styles and regions | no content: nothing |
//! | `metadata` and the elements of the metadata namespaces, anywhere | silent: metadata is not content (14) |
//! | `head`: `styling` and `layout` are read (below); any other child (foreign, or a tt element not in the vocabulary) | `Elements` on every cue of the document (see the rule for document-level counts below) |
//! | `styling`: each `style` | an entry of the style table. Its `xml:id` names it; without one it can never be referenced and has no effect |
//! | `styling`: `initial` (10.1.1) | the initial value of each property it names, for every paragraph: a paragraph takes it where no ancestor, region or style reference gives one (10.4.2), so it is the lowest layer. Carried: `tts:fontStyle`, `tts:fontWeight`, `tts:textDecoration`, and `tts:display`, `tts:visibility`, `tts:opacity` (the same rules as on the element). The last `initial` in document order applies. Any other property: `Styling` or `Position` on every cue. A conditional `initial` is not evaluated, so it is not applied: `Styling` on every cue |
//! | `styling`: any other child | `Elements` on every cue of the document |
//! | `layout`: each `region` | an entry of the region table, named by its `xml:id` |
//! | a duplicate `xml:id` among styles, or among regions | the first definition stands; the later one is dropped. Each reference to the name counts the dropped one's declarations (below). The `xml:id` error is not fatal (XML ID, section 6) |
//! | a dropped duplicate's declarations, referenced | `Styling` for each carried or unknown declaration, `Position` for a placement: on each cue of a paragraph or element that references the name |
//! | `style` inside `region` | a nested declaration of the region (10.4.1.4): an entry merged after the referenced styles, before the region's own attributes |
//! | a `style` attribute (`body`, `div`, `p`, `span`, `br`, `region`, `style`): each id | merged in order. An id not defined, a loop, or a chain past [`style::MAX_CHAIN`] sets `Styling` on the element |
//! | `tts:fontStyle`, `tts:fontWeight`, `tts:textDecoration` (underline, line-through, overline) on those elements | carried: italic, bold and the decorations of the text, inherited as 10.2 says |
//! | `tts:fontStyle`, `tts:fontWeight`, `tts:textDecoration` with a value outside the grammar | `Styling` |
//! | other `tts:*` declarations, and `tts:*` names the reader does not know | `Styling` on the element (every piece under it, once per cue) |
//! | `tts:display` `none`, `tts:visibility` `hidden`, `tts:opacity` `0`, on `body`, `div`, `p`, `region` or `span` (10.2.14, 10.2.49, 10.2.30) | the content is not shown: `none` and `0` hide the element and everything in it, `hidden` is inherited and a descendant with `visible` is shown. A hidden paragraph is `skipped`: `hidden by the document`; a paragraph with its own hidden text, or a hidden span in a shown paragraph, counts once (the outermost element holding the hidden text). Opacity counts the whole element; the other values are initial values and count nothing |
//! | `tts:opacity` with a value between 0 and 1, `tts:display="inlineBlock"`, a `tts:display`, `tts:visibility` or `tts:opacity` on `br` or `tt` (not applicable there) | `Styling`; the text is kept |
//! | `tts:origin`, `tts:extent`, `tts:displayAlign` on a region of a paragraph | the placement of `top`: a `top` paragraph keeps them; otherwise `Position` |
//! | `tts:origin`, `tts:extent`, `tts:displayAlign` on any other element | `Position` |
//! | `tts:padding`, `tts:position`, `tts:textAlign`, `tts:bpd`, `tts:ipd`, `tts:overflow`, `tts:zIndex`, `tts:disparity` | `Position` |
//! | `tts:extent` on `tt` (10.2.16) | the root container's height in px (`top` of a px placement); `auto`, `contain`, and two px lengths are read; anything else `Position` |
//! | a region's placement that cannot be computed (`em`, `c`, `rw`, `rh`, a sign, a `px` without a root height) | the paragraph is not `top`; its placement counts as `Position` |
//! | `region` on `body`, `div` or `p` naming no region | `Position` on the paragraphs it reaches |
//! | `region` on a `span` | `Position` on the span's text (the span is not placed in its region) |
//! | `body`, `div` | containers: timing. A bad time or `timeContainer` makes the paragraphs under it `skipped` |
//! | `p` | cues (`slice.rs`), or `skipped` with its line and reason |
//! | `span` | a container. A `span` with its own timing slices its paragraph |
//! | `br` | a line-break piece over the container's interval, open-ended when the container has no end (12.4): it does not decide the container's end. Metadata in it is ignored without a count; other content is counted as below |
//! | `ruby`, `rb`, `rbc`, `rt`, `rtc`, `rp` in a paragraph or span | a span whose text stays; `Styling` on each cue of it |
//! | text in `p` or `span`, visible | a piece. Visible text with no display time (zero long, or outside its container) makes its paragraph `skipped` once: `text with no display time` |
//! | text in `p` or `span`, white space only | spacing only: a piece over its container's interval; with no end of its own it lasts as long as the paragraph (8.2.10, 12.4), and the space is trimmed where a line starts or ends. Not counted |
//! | visible text directly in `tt`, `body`, `div`, `br`, `head`, `styling`, `layout`, `style` or `region` | `skipped`: `text outside any cue` |
//! | `begin`, `end`, `dur` with a bad value, or `end` before `begin` | a fault on the element: its paragraphs are `skipped` with the reason |
//! | `xml:space` | white-space mode (8.2.10); another value is a fault |
//! | `condition` on `body`, `div`, `p`, `span`, `br` or `ruby` | the element and its content: `skipped`, `conditional element`. The condition is not evaluated, and the text may or may not be shown (8.2.1), so it is not an ignored element |
//! | `set`, `animate`, `animation` on another target, in a paragraph or span | `Styling` on each cue of that paragraph |
//! | `set`, `animate`, `animation` on another target, in `body`, `div` or the head | `Styling` on every cue of the document (an upper bound: the reader does not track which paragraphs they reach) |
//! | `set`, `animate`, `animation` with a `tts:display`, `tts:visibility` or `tts:opacity` target (13.1.3, 13.1.1), in a paragraph or span, or in `body`, `div`, `region` (the paragraphs under it, or placed in it) | each paragraph it reaches is `skipped`: `visibility changes over time`. The timeline is not played, so the paragraph is neither shown nor hidden |
//! | a foreign-namespace element (its whole subtree is pruned, 8.1) in a paragraph or span | `Elements` on each cue of that paragraph |
//! | a foreign or unknown tt element in `body` or `div` | `Elements` on every cue of the document (an upper bound, as above) |
//! | an `image` element (tt namespace) in a paragraph or span | the paragraph is one image unit, counted once in `Elements`; its text is converted |
//! | an `image` element in `body` or `div` | an image unit: `Elements` once |
//! | `smpte:backgroundImage` on a `p` | the paragraph is one image unit (as above); its text is converted |
//! | `smpte:backgroundImage` on a `div` or `body` | the `div` or `body` is one image unit; the text under it is converted |
//! | `smpte:backgroundImage` on a `span` or `br` | its paragraph is one image unit (as above) |
//! | a `span`, `p`, `div` or ruby element in a place that holds no text (`body`, `div`, the head, or inside a `p` or `span` where a paragraph cannot nest) | `skipped`: `unsupported element`, the element and its content |
//! | the document has image units and no text cue | `ReadError::ImageBased` (the whole file) |
//! | the document has no cue, no image unit and nothing skipped | `ReadError::NoCues` (the whole file) |
//! | the document has no cue, and skipped items | `ReadError::AllSkipped`: the count and the first skipped item (the whole file) |
//! | a `p` with no end time: its implicit duration is indefinite (12.4) and the document has no end | `skipped`: `no end time` |
//! | element frames, pieces, copies of pieces in slices, lines and cues, and the style and region entries, their references, and the duplicates | the item budget: past it `TooComplex` |
//! | text copied into cues | `TextTooLarge` past `OUTPUT_BYTES` |
//!
//! Document-level counts. A kind counted on every cue of the document (a
//! `Styling` or `Position` from `initial` (a property the reader does not carry) or from an ignored element outside a paragraph, an
//! `Elements` likewise) is the cue count: it replaces the per-cue counts the
//! paragraphs made, so a document never counts a cue twice. Image units add to
//! `Elements` on top of that, and only when a text cue remains.
//!
//! A cue's losses are its pieces' flags (`Flags`), counted once per cue. A piece
//! takes the flags of every element it is in, and a `br` those of its own
//! element too. Lines and runs follow the `Cue` model: a `br` or a line feed in
//! `preserve` mode ends a line, lines are trimmed, a line with no text is not
//! kept. A run ends where its inline style changes.

mod slice;
mod style;
mod time;

use std::collections::BTreeSet;

use xml::{Element, ErrorKind, Event, Options, QName, Reader};

use crate::srt::STRAY;
use crate::{Budget, Loss, MAX_MS, Parsed, ReadError, Skipped};
use slice::{Content, Output, Piece, RunStyle};
use style::{
    Css, Flags, Len, Opacity, Sheet, animates_visibility, attrs_css, hiding_declared, own_flags,
    root_extent, root_extent_is_usable, top_third,
};
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
/// The SMPTE-TT namespace: its `backgroundImage` attribute makes the element
/// that carries it an image unit (see the accounting table).
const SMPTE_NS: &str = "http://www.smpte-ra.org/schemas/2052-1/2010/smpte-tt";
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
/// Text the document hides: `display="none"` on it or an ancestor (10.2.14),
/// `visibility="hidden"` (10.2.49, inherited), or `opacity="0"` (10.2.30). It is
/// not shown, so it is counted, not converted. The hidden paragraph (or each
/// hidden span of a shown one) is one item.
const HIDDEN: &str = "hidden by the document";
/// A paragraph whose visibility a `set` or `animate` changes over time. The
/// reader does not play that timeline, so the paragraph is refused: showing it
/// or hiding it would be wrong for part of its time.
const VARIES: &str = "visibility changes over time";

/// Reads a whole TTML document. The `xml` crate decodes it (UTF-8, or UTF-16
/// with a byte order mark) and checks its declared encoding. Cues come out in
/// document order; the caller sorts them.
pub(crate) fn parse(bytes: &[u8], budget: &mut Budget) -> Result<Parsed, ReadError> {
    let input = xml::decode(bytes).map_err(xml_error)?;
    let scopes = animated_scopes(&input, budget)?;
    let mut reader = Reader::new(
        &input,
        Options {
            max_depth: xml::MAX_DEPTH,
            html_entities: false,
        },
    );
    let mut doc = Doc::new(budget, scopes);
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
    doc.finish()
}

/// The ordinals of the elements that a `set` or `animate` changing visibility
/// is a child of, when that animation is not inside a paragraph. A paragraph
/// inside such an element is refused, including the ones that come before the
/// animation in the document, so the scopes are found before the reading
/// starts. Elements are numbered by their start tags in document order, the
/// root being 0, and the reading numbers them the same way (`Doc::start`). A
/// paragraph's own animations are seen when they are read, so they are not
/// listed. This pass is linear in the document.
fn animated_scopes(input: &str, budget: &mut Budget) -> Result<BTreeSet<u32>, ReadError> {
    let mut reader = Reader::new(
        input,
        Options {
            max_depth: xml::MAX_DEPTH,
            html_entities: false,
        },
    );
    // One entry per open element: its ordinal, and whether it is in a paragraph.
    let mut open: Vec<(u32, bool)> = Vec::new();
    let mut scopes = BTreeSet::new();
    let mut next: u32 = 0;
    loop {
        match reader.next() {
            Ok(Some(Event::Start(el))) => {
                let ord = next;
                next += 1;
                let parent = open.last().copied();
                let in_para = parent.is_some_and(|(_, para)| para);
                let animation =
                    is_tt(el.name) && matches!(el.name.local, "set" | "animate" | "animation");
                if !in_para && animation && animates_visibility(&el) {
                    let target = parent.map_or(0, |(o, _)| o);
                    if !scopes.contains(&target) {
                        budget.charge()?;
                        scopes.insert(target);
                    }
                }
                let para = in_para || (is_tt(el.name) && el.name.local == "p");
                open.push((ord, para));
            }
            Ok(Some(Event::End(_))) => {
                open.pop();
            }
            Ok(Some(Event::Text(_))) => {}
            // The reading reports the error; this pass only finds the scopes.
            Ok(None) | Err(_) => break,
        }
    }
    Ok(scopes)
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
    /// `head`, `styling`, `layout`: containers of the head, with no timing.
    Head,
    Styling,
    Layout,
    Body,
    Div,
    Paragraph,
    Span,
    Break,
    /// A `style` or a `region`: an entry of the sheet (`Frame::entry`).
    Style,
    Region,
}

/// Where the content of an element is flowed (11.3.1): into a region of the
/// sheet, into a region named but not defined (`Missing`), or nowhere in
/// particular (`None`: the default placement).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    None,
    Region(usize),
    Missing,
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
    /// The inheritable values in force here (10.4.2.1): own, else the ancestor's.
    css: Css,
    /// The declarations that affect this element's content, its own and its
    /// ancestors' (see [`Flags`]).
    flags: Flags,
    /// The region the content is flowed into.
    region: Place,
    /// For a `style` or `region`: its entry in the sheet.
    entry: Option<usize>,
    /// Hidden by `display="none"` or `opacity="0"` on this element or an
    /// ancestor, or a region of it (see [`is_hidden`]). Not inherited as a value:
    /// the effect reaches the content, so it is carried down.
    concealed: bool,
    /// Visible text was hidden directly in this element (see `Doc::end`).
    hidden_text: bool,
    /// A `set` or `animate` changing visibility applies to this element's
    /// content: an ancestor of it has one, or its region does (see
    /// [`animated_scopes`]).
    animated: bool,
    /// The start tag's line, for a hidden span's item.
    line: u32,
}

/// Whether a frame's text is hidden by the document: its element, or an
/// ancestor, has `display="none"` or `opacity="0"` (`concealed`), or its
/// visibility is `hidden` (inherited, and a descendant with `visible` is shown,
/// 10.2.49).
fn is_hidden(frame: &Frame) -> bool {
    frame.concealed || frame.css.hidden == Some(true)
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
            css: Css::default(),
            flags: Flags::default(),
            region: Place::None,
            entry: None,
            concealed: false,
            hidden_text: false,
            animated: false,
            line: 0,
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

    /// The flags of a text piece in this element: the flags of its ancestors
    /// and this element, and the decorations that this element's text takes
    /// from the values in force (a line-through or an overline of any ancestor).
    fn text_flags(&self) -> Flags {
        Flags {
            styling: self.flags.styling || decorated(&self.css),
            position: self.flags.position,
            elements: false,
        }
    }

    /// This element's end, clamped by its ancestors. The implicit end of a
    /// container is its children's (see the module doc).
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
    /// The region's placement puts the paragraph at the top (see [`top_third`]).
    top: bool,
    /// Losses of the paragraph's own ignored elements (`set`, foreign content):
    /// every cue the paragraph is cut into carries them.
    flags: Flags,
    /// The paragraph holds an image (an image element, or a `backgroundImage`
    /// on it or on a span or `br` in it): one `Elements` unit. Its text is
    /// converted all the same.
    unit: bool,
    /// A `set` or `animate` changing visibility applies to the paragraph (see
    /// [`Frame::animated`]): it is refused whole (see `paragraph`).
    animated: bool,
    /// The start lines of the outermost hidden spans of a shown paragraph: one
    /// `skipped` item each, counted when the paragraph is kept (see `paragraph`).
    hidden_spans: Vec<u32>,
}

/// What an element is, given the element it is in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Its content is metadata: ignored, not counted.
    Silent,
    /// Not converted, and its text would be shown: the element and its content
    /// are `skipped` with this reason.
    Refused(&'static str),
    /// Not converted, and its content is not shown: counted as `ignore` says.
    Ignored(Ignore),
    /// `body`, `div`, `p`, `span`: a timed element.
    Open(Kind),
    /// `ruby` and its parts: a span whose text is kept, its ruby drawn as text.
    Ruby,
    Break,
    /// `head`, `styling`, `layout`: a container with no timing.
    Container(Kind),
    /// `style` (in `styling`, or nested in a `region`) or `region`: an entry.
    Define(Kind),
    /// `initial` (10.1.1): the initial value of the properties it names, which
    /// every paragraph takes where nothing else gives one (see `Doc::initial`).
    Initial,
}

/// What an ignored element counts as (the accounting table in the module doc).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ignore {
    /// `set`, `animate`, `animation`: a change of style the reader does not apply.
    Styling,
    /// Foreign and unsupported elements: pruned with their content.
    Elements,
    /// An image: its paragraph is an image unit, or outside a paragraph the
    /// element is one. The text is converted all the same.
    Image,
}

/// The element's role under a parent of kind `parent`.
fn role(parent: Kind, name: QName) -> Role {
    if is_metadata(name) {
        return Role::Silent;
    }
    // A foreign element, or one with no namespace, is not TTML: pruned whole.
    if !is_tt(name) {
        return Role::Ignored(Ignore::Elements);
    }
    match (parent, name.local) {
        (Kind::Root, "head") => Role::Container(Kind::Head),
        (Kind::Root, "body") => Role::Open(Kind::Body),
        (Kind::Head, "styling") => Role::Container(Kind::Styling),
        (Kind::Head, "layout") => Role::Container(Kind::Layout),
        (Kind::Styling | Kind::Region, "style") => Role::Define(Kind::Style),
        (Kind::Styling, "initial") => Role::Initial,
        (Kind::Layout, "region") => Role::Define(Kind::Region),
        (Kind::Body | Kind::Div, "div") => Role::Open(Kind::Div),
        (Kind::Body | Kind::Div, "p") => Role::Open(Kind::Paragraph),
        (Kind::Paragraph | Kind::Span, "span") => Role::Open(Kind::Span),
        (Kind::Paragraph | Kind::Span, "br") => Role::Break,
        (Kind::Paragraph | Kind::Span, "ruby" | "rb" | "rbc" | "rt" | "rtc" | "rp") => Role::Ruby,
        (_, "set" | "animate" | "animation") => Role::Ignored(Ignore::Styling),
        (_, "image") => Role::Ignored(Ignore::Image),
        // An element that carries text, in a place where it cannot be shown.
        (_, "p" | "div" | "span" | "ruby" | "rb" | "rbc" | "rt" | "rtc" | "rp") => {
            Role::Refused(UNSUPPORTED_ELEMENT)
        }
        _ => Role::Ignored(Ignore::Elements),
    }
}

/// The head's elements and the sheet's (`styling`, `layout`, `style`, `region`,
/// `initial`). After the body they are refused: the sheet is resolved then.
fn is_structure(name: QName) -> bool {
    is_tt(name)
        && matches!(
            name.local,
            "head" | "styling" | "layout" | "style" | "region" | "initial"
        )
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

/// Whether a computed set has a line-through or an overline: the decorations
/// the reader does not draw. They are inherited one by one (10.2.43.1), so they
/// are read from the computed values, not OR-ed down the element chain.
fn decorated(css: &Css) -> bool {
    css.line_through == Some(true) || css.overline == Some(true)
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
    skipped: Skipped,
    /// The cues, their losses and the text copied into them (see `slice.rs`).
    out: Output,
    /// The styles and regions of the head, resolved when the body begins.
    sheet: Sheet,
    /// The initial values (10.1.1), merged from the `initial` elements in
    /// document order: the last one given for a property applies. Each
    /// paragraph takes them where no ancestor, no region and no reference
    /// gives a value (10.4.2), so they are the lowest layer of its style.
    initial: Css,
    /// The body has begun: the sheet is resolved, and no head element is read.
    frozen: bool,
    /// The root container's height, when `tt` gives it in px (10.2.16).
    root_height: Option<Len>,
    /// The losses that hold for every cue: an `initial` element, and an ignored
    /// element outside any paragraph (see `finish`).
    doc_flags: Flags,
    /// Image units outside a paragraph: an image element, or a `div` or `body`
    /// with a background image. Each is one `Elements` count; the paragraph
    /// units are counted at their end. A document with image units and no
    /// text cue is image subtitles, refused as such.
    images: usize,
    /// The containers a visibility-changing animation applies to (see
    /// [`animated_scopes`]), by ordinal.
    animated_scopes: BTreeSet<u32>,
    /// The ordinal the next start tag takes, and the one of the current element.
    next_ord: u32,
    ord: u32,
}

impl<'b> Doc<'b> {
    fn new(budget: &'b mut Budget, animated_scopes: BTreeSet<u32>) -> Doc<'b> {
        Doc {
            budget,
            animated_scopes,
            next_ord: 0,
            ord: 0,
            params: None,
            stack: Vec::new(),
            para: None,
            ignoring: 0,
            skipped: Skipped::default(),
            out: Output::default(),
            sheet: Sheet::default(),
            initial: Css::default(),
            frozen: false,
            root_height: None,
            doc_flags: Flags::default(),
            images: 0,
        }
    }

    fn start(&mut self, el: &Element, line: u32) -> Result<(), ReadError> {
        // Every start tag takes an ordinal, read or not: `animated_scopes` numbers them the same way.
        self.ord = self.next_ord;
        self.next_ord += 1;
        if self.ignoring > 0 {
            self.ignoring += 1;
            return Ok(());
        }
        let Some(parent) = self.stack.last().map(|f| f.kind) else {
            return self.root(el);
        };
        // After the body the sheet is resolved: a head element there is ignored.
        let role = if self.frozen && is_structure(el.name) {
            Role::Ignored(Ignore::Elements)
        } else {
            role(parent, el.name)
        };
        let role = match role {
            // Conditions are not evaluated, so a conditional element is refused
            // whole rather than guessed at: its text may or may not be shown.
            Role::Open(_) | Role::Break | Role::Ruby if el.attr(None, "condition").is_some() => {
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
            // A set or animate that changes visibility is not counted as styling:
            // the paragraphs it reaches are refused instead (`paragraph`).
            Role::Ignored(Ignore::Styling)
                if animates_visibility(el)
                    && matches!(
                        parent,
                        Kind::Root
                            | Kind::Body
                            | Kind::Div
                            | Kind::Region
                            | Kind::Paragraph
                            | Kind::Span
                    ) =>
            {
                if let Some(para) = self.para.as_mut() {
                    para.animated = true;
                }
                self.ignoring = 1;
            }
            Role::Ignored(what) => {
                self.ignore(what);
                self.ignoring = 1;
            }
            Role::Break => self.line_break(el)?,
            Role::Ruby => {
                self.open(Kind::Span, el, line)?;
                // The ruby text stays; the ruby declarations are not drawn.
                if let Some(top) = self.stack.last_mut() {
                    top.flags.styling = true;
                }
            }
            Role::Open(kind) => self.open(kind, el, line)?,
            Role::Container(kind) => self.container(kind)?,
            Role::Define(kind) => self.define(kind, el)?,
            Role::Initial => {
                // A conditional `initial` is not evaluated (8.2.1), so it is not
                // applied: counted as styling, as the reader cannot tell what it sets.
                if el.attr(None, "condition").is_some() {
                    self.doc_flags.styling = true;
                } else {
                    let decl = attrs_css(el);
                    self.initial.merge(&decl);
                    let flags = own_flags(&decl);
                    self.doc_flags.styling |= flags.styling;
                    self.doc_flags.position |= flags.position;
                }
                self.ignoring = 1;
            }
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
        // 10.2.16: `tts:extent` on `tt` is the root container's size. Only two px
        // lengths give a height; `auto` and `contain` are read as the default.
        let extent = style::tts_value(el, "extent").map(str::trim);
        self.root_height = extent.and_then(root_extent);
        let decl = attrs_css(el);
        let mut frame = Frame::bare(Kind::Root, preserve);
        // 12.2: the document's time begins at zero.
        frame.begin = Some(0);
        frame.cursor = Some(0);
        frame.fault = bad_space;
        frame.css = decl.over(&Css::default());
        // The root takes no visibility (10.2.49 applies to body, div, p, region and span).
        frame.css.hidden = None;
        frame.animated = self.animated_scopes.contains(&self.ord);
        // `tts:extent` is consumed above, so it is not a placement of the content.
        frame.flags = Flags {
            styling: decl.styling || hiding_declared(&decl),
            position: decl.position
                || decl.origin.is_some()
                || decl.align.is_some()
                || extent.is_some_and(|v| !root_extent_is_usable(v)),
            elements: false,
        };
        self.stack.push(frame);
        Ok(())
    }

    /// Opens a `head`, `styling` or `layout`: a container with no timing.
    fn container(&mut self, kind: Kind) -> Result<(), ReadError> {
        self.budget.charge()?;
        let preserve = self.stack.last().is_some_and(|f| f.preserve);
        self.stack.push(Frame::bare(kind, preserve));
        Ok(())
    }

    /// Opens a `style` or a `region`: an entry of the sheet. Its `style`
    /// attribute references are recorded in order; its own declarations come
    /// last. A `style` in a region is a nested declaration of it (10.4.1.4): an
    /// unnamed entry the region references after the attribute's styles.
    fn define(&mut self, kind: Kind, el: &Element) -> Result<(), ReadError> {
        let own = attrs_css(el);
        let id = el.attr(Some(xml::XML_NS), "id");
        let parent = self.stack.last().map(|f| (f.kind, f.entry));
        let entry = match (kind, parent) {
            (Kind::Region, _) => self.sheet.define_region(self.budget, id, own)?,
            (_, Some((Kind::Region, Some(region)))) => {
                let nested = self.sheet.define_style(self.budget, None, own)?;
                self.sheet.push_ref(self.budget, region, nested)?;
                nested
            }
            _ => self.sheet.define_style(self.budget, id, own)?,
        };
        if let Some(refs) = el.attr(None, "style") {
            for name in refs.split_ascii_whitespace() {
                let target = self.sheet.style_ref(self.budget, name)?;
                self.sheet.push_ref(self.budget, entry, target)?;
            }
        }
        if kind == Kind::Region && self.animated_scopes.contains(&self.ord) {
            self.sheet.mark_animated(entry);
        }
        self.budget.charge()?;
        let preserve = self.stack.last().is_some_and(|f| f.preserve);
        let mut frame = Frame::bare(kind, preserve);
        frame.entry = Some(entry);
        self.stack.push(frame);
        Ok(())
    }

    /// Opens a `body`, `div`, `p` or `span`. Its time is resolved here from
    /// its parent; its end is known only at its end tag (see [`Frame::end`]).
    /// The first `body` resolves the sheet, so the head is read before any
    /// style is used.
    fn open(&mut self, kind: Kind, el: &Element, line: u32) -> Result<(), ReadError> {
        if kind == Kind::Body && !self.frozen {
            self.sheet.freeze();
            self.frozen = true;
        }
        let params = self.params.ok_or(ReadError::NotTtml)?;
        let Some(parent) = self.stack.last() else {
            return Err(ReadError::NotTtml);
        };
        let (base, parent_limit, parent_preserve) = (parent.base(), parent.limit, parent.preserve);
        let (mut fault, parent_css, parent_flags, parent_region) =
            (parent.fault, parent.css, parent.flags, parent.region);
        let (parent_concealed, parent_animated) = (parent.concealed, parent.animated);
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
        let decl = self.sheet.declared(el);
        let mut flags = parent_flags.join(own_flags(&decl));
        let mut css = decl.over(&parent_css);
        // display and opacity reach the content (10.2.14, 10.2.30); visibility is in `css`.
        // display and opacity are not inherited, so each element takes its own
        // value, else the initial one (10.2.14, 10.2.30, 10.1.1).
        let initial = self.initial;
        let mut concealed = parent_concealed
            || decl.display_none.or(initial.display_none) == Some(true)
            || decl.opacity.or(initial.opacity) == Some(Opacity::Zero);
        let mut animated = parent_animated || self.animated_scopes.contains(&self.ord);
        // A `region` names the region of the element and its content; a span's
        // is not used (its text is not placed in it), which is a `Position` loss.
        let region = match (kind, el.attr(None, "region")) {
            (Kind::Span, Some(_)) => {
                flags.position = true;
                parent_region
            }
            (_, Some(name)) => match self.sheet.region_named(name) {
                Some(entry) => Place::Region(entry),
                None => Place::Missing,
            },
            (_, None) => parent_region,
        };
        let mut top = false;
        if kind == Kind::Paragraph {
            match region {
                Place::None => {}
                // A region that is named and not defined: the placement is lost.
                Place::Missing => flags.position = true,
                Place::Region(entry) => {
                    let placed = *self.sheet.css(entry);
                    // 10.4.2.2: the region gives the values no ancestor does.
                    css = css.over(&placed);
                    // A region's own display and opacity hide its paragraphs (10.2.14, 10.2.30).
                    concealed |= placed.display_none.or(initial.display_none) == Some(true)
                        || placed.opacity.or(initial.opacity) == Some(Opacity::Zero);
                    animated |= self.sheet.animated(entry);
                    flags = flags.join(self.sheet.flags(entry));
                    top = top_third(&placed, self.root_height) == Some(true);
                    if !top
                        && (placed.origin.is_some()
                            || placed.extent.is_some()
                            || placed.align.is_some())
                    {
                        flags.position = true;
                    }
                }
            }
        }
        // 10.1.1: the initial values are the lowest layer of a paragraph's style:
        // a region's value (10.4.2.2) and every ancestor's already beat them.
        if kind == Kind::Paragraph {
            css = css.over(&initial);
        }
        // An image background (SMPTE-TT) is an image unit: a `div` or `body`
        // counts here, a `p` as its paragraph, a `span` through its paragraph.
        // The text under it is converted; only the image is not.
        let own_image = el.attr(Some(SMPTE_NS), "backgroundImage").is_some();
        if own_image && matches!(kind, Kind::Body | Kind::Div) {
            self.images += 1;
        }
        if own_image && kind == Kind::Span {
            self.image_unit();
        }
        let mut frame = Frame::bare(kind, preserve);
        frame.begin = begin_at;
        frame.cursor = begin_at;
        frame.seq = seq;
        frame.limit = min_end(parent_limit, explicit.flatten());
        frame.explicit = explicit;
        frame.fault = fault;
        frame.css = css;
        frame.flags = flags;
        frame.region = region;
        frame.concealed = concealed;
        frame.animated = animated;
        frame.line = line;
        // Charged before the frame is pushed: an element is an item.
        self.budget.charge()?;
        if kind == Kind::Paragraph {
            self.para = Some(Para {
                line,
                pieces: Vec::new(),
                visible: false,
                hidden: false,
                fault: None,
                top,
                flags: Flags::default(),
                unit: own_image,
                animated,
                hidden_spans: Vec::new(),
            });
        }
        self.stack.push(frame);
        Ok(())
    }

    /// A `br` in a paragraph or span: a line break in the parent's interval.
    /// Its own content is metadata and is read by the frame that holds it.
    /// A `br` is an anonymous span (12.4), so its end is the container's, not
    /// its own: it does not decide the container's end (`child_ended` is not
    /// called). In `seq` it is zero long and the cursor does not move.
    fn line_break(&mut self, el: &Element) -> Result<(), ReadError> {
        if el.attr(Some(SMPTE_NS), "backgroundImage").is_some() {
            self.image_unit();
        }
        let Some(top) = self.stack.last() else {
            return Ok(());
        };
        let (base, end) = top.leaf_time();
        let preserve = top.preserve;
        let decl = self.sheet.declared(el);
        // A `br` takes no display, visibility or opacity (10.2.14, 10.2.30, 10.2.49): counted as styling.
        let own = Flags {
            styling: decorated(&decl) || hiding_declared(&decl),
            ..Flags::default()
        };
        let flags = top.text_flags().join(own_flags(&decl)).join(own);
        // Zero long in `seq` (no piece); in `par` the end is open, and the
        // paragraph's end resolves it (see `slice::cues`).
        if let Some(begin) = base
            && end.is_none_or(|end| end > begin)
        {
            self.budget.charge()?;
            self.push_piece(Piece {
                begin,
                end,
                content: Content::Break,
                style: RunStyle::default(),
                flags,
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
        let (flags, style) = (top.text_flags(), RunStyle::of(&top.css));
        let hidden = is_hidden(top);
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
        if hidden {
            // Not shown: nothing is kept, and the element is counted when it ends.
            if visible && let Some(top) = self.stack.last_mut() {
                top.hidden_text = true;
            }
        } else {
            self.keep_text(base, end, text, preserve, style, flags)?;
        }
        if visible && let Some(para) = self.para.as_mut() {
            para.visible = true;
        }
        Ok(())
    }

    /// Keeps a piece of text in the open paragraph; visible text with no display
    /// time is counted (`NO_DISPLAY`, see `paragraph`). Text with no end of its
    /// own (white space between timed spans, say) lasts as long as the
    /// paragraph: it is kept with an open end, and `slice::cues` ends it.
    fn keep_text(
        &mut self,
        base: Option<u64>,
        end: Option<u64>,
        text: &str,
        preserve: bool,
        style: RunStyle,
        flags: Flags,
    ) -> Result<(), ReadError> {
        let visible = text.chars().any(|c| !c.is_whitespace());
        match (base, end) {
            (Some(begin), end) if end.is_none_or(|end| end > begin) => {
                self.budget.charge()?;
                let content = Content::Text {
                    text: slice::normalize(text, preserve),
                    preserve,
                };
                self.push_piece(Piece {
                    begin,
                    end,
                    content,
                    style,
                    flags,
                });
            }
            _ if visible => {
                if let Some(para) = self.para.as_mut() {
                    para.hidden = true;
                }
            }
            _ => {}
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
        // A `br`'s line break was placed at its start. The root, the head's
        // containers and the sheet's entries have no timing of their own.
        if matches!(
            frame.kind,
            Kind::Root
                | Kind::Break
                | Kind::Head
                | Kind::Styling
                | Kind::Layout
                | Kind::Style
                | Kind::Region
        ) {
            return Ok(());
        }
        // A span that hid its own text is one item, unless the element it is in
        // is hidden too: then the outermost hidden element is the one counted.
        if frame.kind == Kind::Span && frame.hidden_text {
            match self.stack.last_mut() {
                Some(parent) if is_hidden(parent) => parent.hidden_text = true,
                _ => self.hidden_span(frame.line)?,
            }
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

    /// Records a hidden span of the open paragraph, by its start line. It is
    /// counted if the paragraph is kept (see `paragraph`).
    fn hidden_span(&mut self, line: u32) -> Result<(), ReadError> {
        if let Some(para) = self.para.as_mut() {
            self.budget.charge()?;
            para.hidden_spans.push(line);
        }
        Ok(())
    }

    /// Cuts a finished paragraph into cues, or skips it with a reason. A
    /// paragraph with no visible text has nothing to lose and is not counted.
    /// Refusals come first: a paragraph refused for its visibility or its hidden
    /// text counts once, and its hidden spans are not counted separately.
    fn paragraph(&mut self, frame: &Frame, end: Option<u64>) -> Result<(), ReadError> {
        let Some(para) = self.para.take() else {
            return Ok(());
        };
        if para.unit {
            self.images += 1;
        }
        if !para.visible {
            return Ok(());
        }
        if para.animated {
            self.skip(para.line, VARIES);
            return Ok(());
        }
        // display and opacity hide the whole paragraph. Its visibility does not:
        // a span may set `visible` again, so the paragraph's own text is counted below.
        if frame.concealed {
            self.skip(para.line, HIDDEN);
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
        if frame.hidden_text {
            self.skip(para.line, HIDDEN);
        }
        for line in para.hidden_spans {
            self.skip(line, HIDDEN);
        }
        slice::cues(
            &para.pieces,
            (begin, end),
            para.top,
            para.flags,
            self.budget,
            &mut self.out,
        )?;
        if para.hidden {
            self.skip(para.line, NO_DISPLAY);
        }
        Ok(())
    }

    /// The open paragraph holds an image: its unit is counted once, at its end.
    fn image_unit(&mut self) {
        if let Some(para) = self.para.as_mut() {
            para.unit = true;
        }
    }

    /// Counts an ignored element where it sits: in an open paragraph, on each
    /// of its cues (an image: the paragraph's unit); elsewhere on every cue of
    /// the document, or as an image unit.
    fn ignore(&mut self, what: Ignore) {
        match (self.para.as_mut(), what) {
            (Some(para), Ignore::Styling) => para.flags.styling = true,
            (Some(para), Ignore::Elements) => para.flags.elements = true,
            (Some(para), Ignore::Image) => para.unit = true,
            (None, Ignore::Styling) => self.doc_flags.styling = true,
            (None, Ignore::Elements) => self.doc_flags.elements = true,
            (None, Ignore::Image) => self.images += 1,
        }
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

    /// The document's result. Document-level losses hold for every cue, so
    /// each is counted as the cue count, which replaces the per-cue counts of
    /// the paragraphs. Image units are added to `Elements`. Without a cue, a
    /// document with image units is image subtitles, refused as such.
    fn finish(self) -> Result<Parsed, ReadError> {
        let mut out = self.out;
        let cues = out.cues.len();
        if cues == 0 && self.images > 0 {
            return Err(ReadError::ImageBased);
        }
        for (set, loss) in [
            (self.doc_flags.styling, Loss::Styling),
            (self.doc_flags.position, Loss::Position),
            (self.doc_flags.elements, Loss::Elements),
        ] {
            if set && cues > 0 {
                out.losses.insert(loss, cues);
            }
        }
        if cues > 0 && self.images > 0 {
            *out.losses.entry(Loss::Elements).or_insert(0) += self.images;
        }
        Ok(Parsed {
            cues: out.cues,
            losses: out.losses,
            skipped: self.skipped,
        })
    }
}
