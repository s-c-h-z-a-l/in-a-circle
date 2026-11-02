//! XHTML chapter reader: a book chapter (foreign input) into the document
//! tree, through the strict `xml` reader with the HTML named characters.
//!
//! The subset (anything else keeps its text; text disappears only with a
//! reported [`Loss`]):
//! - Blocks: `p`, `h1`-`h6`, `ul`/`ol` (`start`, at most 999,999,999;
//!   `menu`, `dir` as `ul`), `li`, `blockquote`, `pre` (a line feed
//!   right after its start tag is dropped; markup inside keeps its text,
//!   `br` is a line feed, a block inside is a line of its own, a cell a
//!   space), `hr`,
//!   `table` with `thead`/`tbody`/`tfoot`/`tr`/`th`/`td`/`caption`.
//! - Inlines: `em`/`i`, `strong`/`b`, `del`/`s`/`strike`, `code`/`kbd`/
//!   `samp` (flat text), `a href` (a link inside a link is its text),
//!   `img` (`src`, `alt`, `title`), `br`, `ruby` (base text, then each `rt`
//!   in parentheses; an `rp` holding only brackets and white space, the
//!   parentheses the reader writes itself, is dropped, any other `rp`
//!   keeps its text).
//! - Transparent (the element goes, its content stays): `div`, `section`,
//!   `article`, `aside`, `figure`, `figcaption`, `header`, `footer`, `nav`,
//!   `dl`/`dt`/`dd` and the other HTML block containers, which also end a
//!   paragraph; `span` and every unknown element, MathML and the EPUB
//!   `epub:` elements included, which do not.
//! - Dropped with their content: `head`, `script`, `style`, `template`;
//!   inline SVG ([`Loss::Images`]); without a loss, MathML `annotation`
//!   and `annotation-xml` (another encoding of the same formula) and
//!   `epub:case` (an alternative to its `epub:switch`'s `epub:default`,
//!   which is read). Stylesheets and `style`
//!   attributes are never read: [`Loss::Styles`] is always reported.
//! - Elements in no namespace are read as XHTML (EPUB 2 chapters written
//!   without the namespace), names compared ASCII case-insensitively;
//!   elements of any other namespace are transparent.
//! - White space (space, TAB, LF, FF, CR) collapses as in HTML, except in
//!   `pre`: a run is one space, none at the start or end of a block or
//!   around a line break. A no-break space is text.
//! - Inline content at block level is wrapped in a paragraph; a block
//!   where only inline content fits (a paragraph in a cell or in
//!   emphasis) keeps its text on lines of its own (line breaks), a cell
//!   there is a space.
//! - Tables: the first row is the header when it is in `thead` or all its
//!   cells are `th`, else the header is empty; `tfoot` rows come last; a
//!   cell spanning columns or rows (`colspan` up to 1000, `rowspan` up to
//!   65534, as HTML clamps them) is kept once, the cells it covered are
//!   empty ([`Loss::MergedCells`]); short rows are padded; a caption is a
//!   paragraph before the table.
//!
//! Budgets: every element is a node, charged when it starts, and so is
//! every node it builds besides its own (a text run, an implicit
//! paragraph, list item, row or cell, a line break, an empty cell a span
//! adds); text is charged by the bytes kept, before it is copied. Both are
//! charged before the allocation they pay for. The open elements and the
//! nodes being built are the builder's own stacks, never recursion.

use std::collections::BTreeSet;

use xml::{Element, Event, QName, Reader};

use crate::budget::Meter;
use crate::md::MAX_LIST_NUMBER;
use crate::{Align, Block, Document, Inline, LimitError, Limits, ListItem, Loss};

const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";
const SVG_NS: &str = "http://www.w3.org/2000/svg";
const MATHML_NS: &str = "http://www.w3.org/1998/Math/MathML";
/// The EPUB structural namespace (`epub:switch`, `epub:case`).
const OPS_NS: &str = "http://www.idpf.org/2007/ops";

/// What a ruby parenthesis (`rp`) holds when it only shows the brackets
/// around `rt` to a reader without ruby support: the ASCII ones and the
/// CJK and fullwidth ones books use. The reader writes its own
/// parentheses around `rt`, so such an `rp` is dropped; any other text in
/// it is kept.
const RP_BRACKETS: &str = "()[]{}<>（）［］｛｝〈〉《》「」『』【】〔〕〖〗〘〙〚〛｟｠⦅⦆";

/// HTML clamps `colspan` to 1..=1000 and `rowspan` to 0..=65534 (WHATWG,
/// "processing model" of tables), 0 meaning "to the end of the row
/// group": the same values here, so a hostile span costs at most what a
/// browser would lay out, every covered cell charged as a node.
const MAX_COLSPAN: u32 = 1000;
const MAX_ROWSPAN: u32 = 65534;

/// The longest element name the reader knows (`blockquote`,
/// `figcaption`); a longer one is unknown without being compared.
const LONGEST_NAME: usize = 10;

/// Upper bound of the work counted by [`read_steps`] per input byte,
/// besides [`STEPS_PER_NODE`]. Text is scanned at most four times (white
/// space test, pending break test, collapsed length, copy); an element of
/// at least four bytes (`<a/>`) costs one step per event, frame opened,
/// closed or attached and per level of the trailing space trimmed (each
/// level built by an element of its own). 8 leaves room for these
/// constants; a quadratic path at the tests' sizes exceeds it by orders of
/// magnitude.
#[doc(hidden)]
pub const STEPS_PER_BYTE: u64 = 8;

/// Steps per node of the budget that do not come from a byte: each empty
/// cell a span or padding adds, and the row-span bookkeeping of a column,
/// cost a step and are charged as a node, so they are bounded by the
/// node budget.
#[doc(hidden)]
pub const STEPS_PER_NODE: u64 = 4;

/// Why a chapter could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XhtmlError {
    /// Not well-formed, too deep, an unsupported encoding or a DTD entity.
    Xml(xml::Error),
    /// A budget of [`Limits`] ran out (`Exceeded("nodes")`,
    /// `Exceeded("owned text")`, `Exceeded("input")`).
    Limit(LimitError),
}

impl From<LimitError> for XhtmlError {
    fn from(e: LimitError) -> Self {
        XhtmlError::Limit(e)
    }
}

impl std::fmt::Display for XhtmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            XhtmlError::Xml(e) => e.fmt(f),
            XhtmlError::Limit(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for XhtmlError {}

/// Reads one chapter's text (already decoded by `xml::decode`) under
/// `limits`: the text length is checked against `limits.input_bytes`
/// first. The losses always hold [`Loss::Styles`].
pub fn read_limited(text: &str, limits: Limits) -> Result<(Document, BTreeSet<Loss>), XhtmlError> {
    read_steps(text, limits).0
}

/// [`read_limited`] and the work it did (the linearity tests and the fuzz
/// targets bound it by [`STEPS_PER_BYTE`] and [`STEPS_PER_NODE`]). The
/// XML reader's own work is `xml::Reader::steps`, bounded by the `xml`
/// crate.
#[doc(hidden)]
pub fn read_steps(
    text: &str,
    limits: Limits,
) -> (Result<(Document, BTreeSet<Loss>), XhtmlError>, u64) {
    if text.len() > limits.input_bytes {
        return (Err(LimitError::Exceeded("input").into()), 0);
    }
    let mut b = Builder::new(limits);
    let mut r = Reader::new(
        text,
        xml::Options {
            max_depth: xml::MAX_DEPTH,
            html_entities: true,
        },
    );
    while !b.m.failed() {
        match r.next() {
            Ok(Some(ev)) => b.event(ev),
            Ok(None) => break,
            Err(e) => return (Err(XhtmlError::Xml(e)), b.steps),
        }
    }
    let res = b.finish();
    (res, b.steps)
}

/// Whether the document holds any text that is not white space: text,
/// code and code blocks count, an image's alt text does not.
pub fn has_text(doc: &Document) -> bool {
    enum Part<'a> {
        Blocks(&'a [Block]),
        Inlines(&'a [Inline]),
    }
    let visible = |s: &str| !s.trim().is_empty();
    let mut stack = vec![Part::Blocks(&doc.blocks)];
    while let Some(part) = stack.pop() {
        match part {
            Part::Blocks(blocks) => {
                for b in blocks {
                    match b {
                        Block::Paragraph(c) | Block::Heading { content: c, .. } => {
                            stack.push(Part::Inlines(c));
                        }
                        Block::CodeBlock { text, .. } if visible(text) => return true,
                        Block::BlockQuote(inner) => stack.push(Part::Blocks(inner)),
                        Block::List { items, .. } => {
                            stack.extend(items.iter().map(|i| Part::Blocks(&i.blocks)));
                        }
                        Block::Table { head, rows, .. } => {
                            stack.extend(
                                head.iter()
                                    .chain(rows.iter().flatten())
                                    .map(|c| Part::Inlines(c)),
                            );
                        }
                        _ => {}
                    }
                }
            }
            Part::Inlines(inlines) => {
                for i in inlines {
                    match i {
                        Inline::Text(t) | Inline::Code(t) if visible(t) => return true,
                        Inline::Emph(c)
                        | Inline::Strong(c)
                        | Inline::Strikethrough(c)
                        | Inline::Link { content: c, .. } => stack.push(Part::Inlines(c)),
                        _ => {}
                    }
                }
            }
        }
    }
    false
}

/// HTML's ASCII white space.
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0C' | b'\r')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Head,
    Body,
    Foot,
}

/// What an element is to the reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    /// Dropped with its content.
    Skip,
    /// Inline SVG: dropped with its content, an image loss.
    Svg,
    Para,
    Heading(u8),
    Quote,
    /// `true`: ordered.
    List(bool),
    Item,
    Pre,
    Hr,
    /// A block container that is not kept: ends a paragraph.
    Block,
    Table,
    Row,
    /// `true`: `th`.
    Cell(bool),
    Group(Section),
    Caption,
    /// Holds nothing (`col`, `colgroup`): neither opens nor ends anything.
    Quiet,
    Emph,
    Strong,
    Strike,
    Code,
    Link,
    Img,
    Br,
    Rt,
    /// Ruby parenthesis: its text is kept unless it is only the brackets
    /// (and white space) the reader writes around `rt` itself.
    Rp,
    /// Inline and not kept (`span`, unknown elements).
    Inline,
}

fn classify(name: QName<'_>) -> Tag {
    // A MathML annotation is another encoding of the formula beside it
    // (TeX, content MathML), and an `epub:case` an alternative to the
    // `epub:default` the book gives as its own fallback: dropped without
    // a loss, the formula's or the fallback's text stays. HTML has no
    // element of that name, so in XHTML or no namespace (MathML written
    // without its namespace) it is the MathML one too.
    let annotation = |local: &str| {
        local.eq_ignore_ascii_case("annotation") || local.eq_ignore_ascii_case("annotation-xml")
    };
    match name.ns {
        None | Some(XHTML_NS | MATHML_NS) if annotation(name.local) => return Tag::Skip,
        None | Some(XHTML_NS) => {}
        Some(SVG_NS) => return Tag::Svg,
        Some(OPS_NS) if name.local == "case" => return Tag::Skip,
        Some(_) => return Tag::Inline,
    }
    let l = name.local.as_bytes();
    if l.len() > LONGEST_NAME {
        return Tag::Inline;
    }
    let mut buf = [0u8; LONGEST_NAME];
    let low = &mut buf[..l.len()];
    low.copy_from_slice(l);
    low.make_ascii_lowercase();
    match &*low {
        b"head" | b"script" | b"style" | b"template" => Tag::Skip,
        b"svg" => Tag::Svg,
        b"p" => Tag::Para,
        b"h1" => Tag::Heading(1),
        b"h2" => Tag::Heading(2),
        b"h3" => Tag::Heading(3),
        b"h4" => Tag::Heading(4),
        b"h5" => Tag::Heading(5),
        b"h6" => Tag::Heading(6),
        b"blockquote" => Tag::Quote,
        b"ul" | b"menu" | b"dir" => Tag::List(false),
        b"ol" => Tag::List(true),
        b"li" => Tag::Item,
        b"pre" => Tag::Pre,
        b"hr" => Tag::Hr,
        b"table" => Tag::Table,
        b"tr" => Tag::Row,
        b"td" => Tag::Cell(false),
        b"th" => Tag::Cell(true),
        b"thead" => Tag::Group(Section::Head),
        b"tbody" => Tag::Group(Section::Body),
        b"tfoot" => Tag::Group(Section::Foot),
        b"caption" => Tag::Caption,
        b"col" | b"colgroup" => Tag::Quiet,
        b"em" | b"i" => Tag::Emph,
        b"strong" | b"b" => Tag::Strong,
        b"del" | b"s" | b"strike" => Tag::Strike,
        b"code" | b"kbd" | b"samp" => Tag::Code,
        b"a" => Tag::Link,
        b"img" => Tag::Img,
        b"br" => Tag::Br,
        b"rt" => Tag::Rt,
        b"rp" => Tag::Rp,
        b"html" | b"body" | b"div" | b"section" | b"article" | b"aside" | b"nav" | b"header"
        | b"footer" | b"main" | b"figure" | b"figcaption" | b"address" | b"center" | b"dl"
        | b"dt" | b"dd" | b"hgroup" | b"details" | b"summary" | b"fieldset" | b"legend"
        | b"form" | b"noscript" | b"search" | b"dialog" => Tag::Block,
        _ => Tag::Inline,
    }
}

/// `ol start`: a non-negative decimal integer, else HTML's default 1;
/// clamped to the Markdown writer's highest list number, so a hostile
/// start does not number every item the same (the writers saturate).
fn list_start(e: &Element<'_>) -> u64 {
    e.attr(None, "start")
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(1)
        .min(MAX_LIST_NUMBER)
}

/// `colspan` and `rowspan` as HTML clamps them; not a number is 1.
fn spans(e: &Element<'_>) -> (u32, u32) {
    let num = |name| {
        e.attr(None, name)
            .and_then(|s| s.trim().parse::<u32>().ok())
    };
    let col = num("colspan").unwrap_or(1).clamp(1, MAX_COLSPAN);
    let row = match num("rowspan") {
        Some(0) => MAX_ROWSPAN,
        Some(n) => n.min(MAX_ROWSPAN),
        None => 1,
    };
    (col, row)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Root,
    Quote,
    Item,
}

/// A frame holding inline content.
enum Run {
    Para,
    Heading(u8),
    Cell {
        th: bool,
        colspan: u32,
        rowspan: u32,
    },
    Caption,
    Emph,
    Strong,
    Strike,
    Link {
        dest: String,
        title: String,
    },
}

struct Cell {
    content: Vec<Inline>,
    th: bool,
    colspan: u32,
    rowspan: u32,
}

struct GridRow {
    cells: Vec<Vec<Inline>>,
    all_th: bool,
}

struct Grid {
    section: Section,
    head: Vec<GridRow>,
    body: Vec<GridRow>,
    foot: Vec<GridRow>,
    /// Per column, the rows (this one included) a cell above still covers.
    spans: Vec<u32>,
    /// Columns from here on are free in the next row.
    hi: usize,
    caption: Vec<Block>,
}

/// A node being built.
enum Frame {
    Blocks {
        kind: Container,
        blocks: Vec<Block>,
        implicit: bool,
    },
    List {
        start: Option<u64>,
        items: Vec<ListItem>,
        loose: bool,
    },
    /// Not boxed: a box cannot be allocated fallibly, and the stack holds
    /// at most one frame per open element (the XML depth limit) plus the
    /// implicit ones, so the larger frame costs a few kilobytes at most.
    Table(Grid),
    Row {
        cells: Vec<Cell>,
        implicit: bool,
    },
    Inlines {
        kind: Run,
        inlines: Vec<Inline>,
        implicit: bool,
    },
    /// `pre` (`raw`: white space kept) or inline code (collapsed); with
    /// `rp`, a ruby parenthesis (collapsed), `Some` holding the white
    /// space state before it.
    Flat {
        raw: bool,
        text: String,
        fresh: bool,
        rp: Option<bool>,
    },
}

/// What happens when an open element ends, after the frames it opened
/// close.
#[derive(Clone, Copy)]
enum End {
    Nothing,
    /// A block inside inline content: the next content goes on a new line.
    Break,
    Text(&'static str),
    /// Inside `pre`: a line feed, unless the text is empty or already
    /// ends with one (see [`Builder::pre_gap`]).
    Line,
    /// A table row group: back to the body.
    Group,
}

struct Open {
    /// The frame stack's height when the element started: its end closes
    /// every frame above.
    height: usize,
    end: End,
}

/// The surroundings a block element starts in.
enum Ctx {
    Block,
    List,
    Table,
    Row,
    Inline,
}

struct Builder {
    m: Meter,
    losses: BTreeSet<Loss>,
    stack: Vec<Frame>,
    open: Vec<Open>,
    /// Depth inside a dropped element (0: not in one).
    skip: u32,
    /// Links open (a link inside one is its text).
    links: u32,
    /// The last thing written to the current block's inline content was
    /// white space, a line break, or nothing: white space now writes
    /// nothing.
    last_space: bool,
    /// The current block has inline content.
    flow: bool,
    /// A block ended or started inside inline content: the next content
    /// starts a new line.
    pending_break: bool,
    steps: u64,
}

impl Builder {
    fn new(limits: Limits) -> Self {
        Builder {
            m: Meter::new(limits),
            losses: BTreeSet::from([Loss::Styles]),
            stack: vec![Frame::Blocks {
                kind: Container::Root,
                blocks: Vec::new(),
                implicit: false,
            }],
            open: Vec::new(),
            skip: 0,
            links: 0,
            last_space: true,
            flow: false,
            pending_break: false,
            steps: 0,
        }
    }

    fn finish(&mut self) -> Result<(Document, BTreeSet<Loss>), XhtmlError> {
        self.m.result()?;
        while self.stack.len() > 1 {
            self.close_top();
        }
        self.m.result()?;
        let blocks = match self.stack.pop() {
            Some(Frame::Blocks { blocks, .. }) => blocks,
            _ => Vec::new(),
        };
        Ok((Document { blocks }, std::mem::take(&mut self.losses)))
    }

    fn event(&mut self, ev: Event<'_>) {
        self.steps += 1;
        match ev {
            Event::Start(e) => {
                if self.m.node() {
                    self.start(&e);
                }
            }
            Event::End(_) => self.end(),
            Event::Text(t) => {
                if self.skip == 0 {
                    self.text(&t);
                }
            }
        }
    }

    fn start(&mut self, e: &Element<'_>) {
        if self.skip > 0 {
            self.skip += 1;
            return;
        }
        let tag = classify(e.name);
        match tag {
            Tag::Skip => {
                self.skip = 1;
                return;
            }
            Tag::Svg => {
                self.losses.insert(Loss::Images);
                self.skip = 1;
                return;
            }
            _ => {}
        }
        if let Some(Frame::Flat { raw, .. }) = self.stack.last() {
            let raw = *raw;
            self.flat_start(tag, e, raw);
            return;
        }
        match tag {
            Tag::Emph
            | Tag::Strong
            | Tag::Strike
            | Tag::Code
            | Tag::Link
            | Tag::Img
            | Tag::Br
            | Tag::Rt
            | Tag::Rp
            | Tag::Inline => self.inline_start(tag, e),
            Tag::Quiet => self.push_open(End::Nothing),
            _ => self.block_start(tag, e),
        }
    }

    fn end(&mut self) {
        if self.skip > 0 {
            self.skip -= 1;
            return;
        }
        let Some(o) = self.open.pop() else {
            return;
        };
        self.close_to(o.height);
        match o.end {
            End::Nothing => {}
            End::Break => self.pending_break = true,
            End::Text(s) => self.text(s),
            End::Line => self.pre_gap("\n"),
            End::Group => {
                if let Some(Frame::Table(g)) = self.stack.last_mut() {
                    g.section = Section::Body;
                    g.spans.clear();
                    g.hi = 0;
                }
            }
        }
    }

    fn push_open(&mut self, end: End) {
        let height = self.stack.len();
        self.m.push(&mut self.open, Open { height, end });
    }

    fn push_frame(&mut self, f: Frame) {
        self.steps += 1;
        self.m.push(&mut self.stack, f);
    }

    /// Opens a block's inline content; the white space state starts over.
    fn open_run(&mut self, kind: Run, implicit: bool) {
        self.last_space = true;
        self.flow = false;
        self.pending_break = false;
        self.push_frame(Frame::Inlines {
            kind,
            inlines: Vec::new(),
            implicit,
        });
    }

    /// Inside `pre` or inline code every element is its text: a break is a
    /// line feed (a space in code), a block starts and ends a line in `pre`
    /// (a cell is a space) and is a space in code, an image its alt text,
    /// a link its text.
    fn flat_start(&mut self, tag: Tag, e: &Element<'_>, raw: bool) {
        let end = match tag {
            Tag::Br => {
                self.text(if raw { "\n" } else { " " });
                End::Nothing
            }
            Tag::Img => {
                self.losses.insert(Loss::Images);
                self.text(e.attr(None, "alt").unwrap_or(""));
                End::Nothing
            }
            Tag::Link if e.attr(None, "href").is_some() => {
                self.losses.insert(Loss::Links);
                End::Nothing
            }
            Tag::Rt => {
                self.text("(");
                End::Text(")")
            }
            Tag::Rp => {
                self.rp_start();
                return;
            }
            Tag::Para
            | Tag::Heading(_)
            | Tag::Quote
            | Tag::List(_)
            | Tag::Item
            | Tag::Pre
            | Tag::Hr
            | Tag::Block
            | Tag::Table
            | Tag::Row
            | Tag::Cell(_)
            | Tag::Caption => {
                if !raw {
                    self.text(" ");
                    End::Text(" ")
                } else if matches!(tag, Tag::Cell(_)) {
                    // Cells of a row go on one line, a space apart.
                    self.pre_gap(" ");
                    End::Nothing
                } else {
                    self.pre_gap("\n");
                    End::Line
                }
            }
            _ => End::Nothing,
        };
        self.push_open(end);
    }

    /// In `pre`, writes `gap` (a line feed or a space) between a block
    /// and the text around it, unless the text is empty or already ends
    /// with a line feed (or, for a space, with white space): markup that
    /// lays a listing out in blocks keeps its words apart.
    fn pre_gap(&mut self, gap: &'static str) {
        let Some(Frame::Flat {
            raw: true, text, ..
        }) = self.stack.last()
        else {
            return;
        };
        let at_gap = match text.as_bytes().last() {
            None | Some(b'\n') => true,
            Some(b' ' | b'\t') => gap == " ",
            Some(_) => false,
        };
        if !at_gap {
            self.text(gap);
        }
    }

    fn inline_start(&mut self, tag: Tag, e: &Element<'_>) {
        self.ensure_inline();
        if self.m.failed() {
            return;
        }
        match tag {
            Tag::Emph | Tag::Strong | Tag::Strike => {
                self.push_open(End::Nothing);
                let kind = match tag {
                    Tag::Emph => Run::Emph,
                    Tag::Strong => Run::Strong,
                    _ => Run::Strike,
                };
                self.push_frame(Frame::Inlines {
                    kind,
                    inlines: Vec::new(),
                    implicit: false,
                });
            }
            Tag::Link => match e.attr(None, "href") {
                Some(href) if self.links == 0 => {
                    let dest = self.m.own(href);
                    let title = self.m.own(e.attr(None, "title").unwrap_or(""));
                    self.push_open(End::Nothing);
                    self.push_frame(Frame::Inlines {
                        kind: Run::Link { dest, title },
                        inlines: Vec::new(),
                        implicit: false,
                    });
                    self.links += 1;
                }
                _ => self.push_open(End::Nothing),
            },
            Tag::Code => {
                self.content_starts();
                self.push_open(End::Nothing);
                self.push_frame(Frame::Flat {
                    raw: false,
                    text: String::new(),
                    fresh: false,
                    rp: None,
                });
            }
            Tag::Img => {
                self.image(e);
                self.push_open(End::Nothing);
            }
            Tag::Br => {
                if self.flow {
                    self.line_break();
                }
                self.pending_break = false;
                self.push_open(End::Nothing);
            }
            Tag::Rt => {
                self.push_open(End::Text(")"));
                self.text("(");
            }
            Tag::Rp => self.rp_start(),
            _ => self.push_open(End::Nothing),
        }
    }

    /// `rp` collects its text flat (an element inside is its text, as in
    /// inline code); [`Builder::close_top`] then drops it when it holds
    /// only brackets and white space, else writes it where it stands.
    /// An `rp` inside one is its text: only the outermost collects and
    /// decides, so nesting does not copy the text again per level.
    fn rp_start(&mut self) {
        let space = self.last_space;
        self.push_open(End::Nothing);
        if let Some(Frame::Flat { rp: Some(_), .. }) = self.stack.last() {
            return;
        }
        self.push_frame(Frame::Flat {
            raw: false,
            text: String::new(),
            fresh: false,
            rp: Some(space),
        });
    }

    fn block_start(&mut self, tag: Tag, e: &Element<'_>) {
        self.close_implicit(tag);
        loop {
            self.steps += 1;
            if self.m.failed() {
                return;
            }
            let ctx = match self.stack.last() {
                Some(Frame::Blocks { .. }) | None => Ctx::Block,
                Some(Frame::List { .. }) => Ctx::List,
                Some(Frame::Table(_)) => Ctx::Table,
                Some(Frame::Row { .. }) => Ctx::Row,
                Some(Frame::Inlines { .. } | Frame::Flat { .. }) => Ctx::Inline,
            };
            match (ctx, tag) {
                (Ctx::Inline, Tag::Cell(_)) => {
                    self.text(" ");
                    self.push_open(End::Nothing);
                }
                (Ctx::Inline, _) => {
                    self.pending_break = true;
                    self.push_open(End::Break);
                }
                (Ctx::Table, Tag::Row) => {
                    self.push_open(End::Nothing);
                    self.push_frame(Frame::Row {
                        cells: Vec::new(),
                        implicit: false,
                    });
                }
                (Ctx::Table, Tag::Cell(_)) => {
                    if self.m.node() {
                        self.push_frame(Frame::Row {
                            cells: Vec::new(),
                            implicit: true,
                        });
                    }
                    continue;
                }
                (Ctx::Table, Tag::Group(s)) => {
                    if let Some(Frame::Table(g)) = self.stack.last_mut() {
                        g.section = s;
                        g.spans.clear();
                        g.hi = 0;
                    }
                    self.push_open(End::Group);
                }
                (Ctx::Table, Tag::Caption) => {
                    self.push_open(End::Nothing);
                    self.open_run(Run::Caption, false);
                }
                (Ctx::Row, Tag::Cell(th)) => {
                    let (colspan, rowspan) = spans(e);
                    self.push_open(End::Nothing);
                    self.open_run(
                        Run::Cell {
                            th,
                            colspan,
                            rowspan,
                        },
                        false,
                    );
                }
                (Ctx::Table | Ctx::Row, _) => {
                    self.ensure_inline();
                    continue;
                }
                (Ctx::List, Tag::Item) => {
                    self.push_open(End::Nothing);
                    self.push_frame(Frame::Blocks {
                        kind: Container::Item,
                        blocks: Vec::new(),
                        implicit: false,
                    });
                }
                (Ctx::List, _) => {
                    if self.m.node() {
                        self.push_frame(Frame::Blocks {
                            kind: Container::Item,
                            blocks: Vec::new(),
                            implicit: true,
                        });
                    }
                    continue;
                }
                (Ctx::Block, Tag::Para) => {
                    // An explicit paragraph in a list item makes the list
                    // loose (blank lines between items), as Markdown does.
                    if let [
                        ..,
                        Frame::List { loose, .. },
                        Frame::Blocks {
                            kind: Container::Item,
                            ..
                        },
                    ] = self.stack.as_mut_slice()
                    {
                        *loose = true;
                    }
                    self.push_open(End::Nothing);
                    self.open_run(Run::Para, false);
                }
                (Ctx::Block, Tag::Heading(level)) => {
                    self.push_open(End::Nothing);
                    self.open_run(Run::Heading(level), false);
                }
                (Ctx::Block, Tag::Quote) => {
                    self.push_open(End::Nothing);
                    self.push_frame(Frame::Blocks {
                        kind: Container::Quote,
                        blocks: Vec::new(),
                        implicit: false,
                    });
                }
                (Ctx::Block, Tag::List(ordered)) => {
                    let start = ordered.then(|| list_start(e));
                    self.push_open(End::Nothing);
                    self.push_frame(Frame::List {
                        start,
                        items: Vec::new(),
                        loose: false,
                    });
                }
                (Ctx::Block, Tag::Pre) => {
                    self.push_open(End::Nothing);
                    self.push_frame(Frame::Flat {
                        raw: true,
                        text: String::new(),
                        fresh: true,
                        rp: None,
                    });
                }
                (Ctx::Block, Tag::Hr) => {
                    self.add_block(Block::ThematicBreak);
                    self.push_open(End::Nothing);
                }
                (Ctx::Block, Tag::Table) => {
                    self.push_open(End::Nothing);
                    self.push_frame(Frame::Table(Grid {
                        section: Section::Body,
                        head: Vec::new(),
                        body: Vec::new(),
                        foot: Vec::new(),
                        spans: Vec::new(),
                        hi: 0,
                        caption: Vec::new(),
                    }));
                }
                (Ctx::Block, _) => self.push_open(End::Nothing),
            }
            return;
        }
    }

    /// Closes the implicit frames a block element `tag` ends: always an
    /// implicit paragraph; an implicit list item before `li`; an implicit
    /// cell before a cell, row, row group or caption, and an implicit row
    /// before the last three.
    fn close_implicit(&mut self, tag: Tag) {
        loop {
            self.steps += 1;
            let close = match self.stack.last() {
                Some(Frame::Inlines {
                    kind: Run::Para,
                    implicit: true,
                    ..
                }) => true,
                Some(Frame::Blocks {
                    kind: Container::Item,
                    implicit: true,
                    ..
                }) => tag == Tag::Item,
                Some(Frame::Inlines {
                    kind: Run::Cell { .. },
                    implicit: true,
                    ..
                }) => matches!(tag, Tag::Row | Tag::Cell(_) | Tag::Group(_) | Tag::Caption),
                Some(Frame::Row { implicit: true, .. }) => {
                    matches!(tag, Tag::Row | Tag::Group(_) | Tag::Caption)
                }
                _ => false,
            };
            if !close {
                return;
            }
            self.close_top();
        }
    }

    /// Opens the implicit frames inline content needs where it is: a
    /// paragraph in a block container, a list item (and a paragraph) in a
    /// list, a row and a cell in a table.
    fn ensure_inline(&mut self) {
        loop {
            self.steps += 1;
            if self.m.failed() {
                return;
            }
            match self.stack.last() {
                Some(Frame::Blocks { .. }) => {
                    if self.m.node() {
                        self.open_run(Run::Para, true);
                    }
                }
                Some(Frame::List { .. }) => {
                    if self.m.node() {
                        self.push_frame(Frame::Blocks {
                            kind: Container::Item,
                            blocks: Vec::new(),
                            implicit: true,
                        });
                    }
                }
                Some(Frame::Table(_)) => {
                    if self.m.node() {
                        self.push_frame(Frame::Row {
                            cells: Vec::new(),
                            implicit: true,
                        });
                    }
                }
                Some(Frame::Row { .. }) => {
                    if self.m.node() {
                        self.open_run(
                            Run::Cell {
                                th: false,
                                colspan: 1,
                                rowspan: 1,
                            },
                            true,
                        );
                    }
                }
                Some(Frame::Inlines { .. } | Frame::Flat { .. }) | None => return,
            }
        }
    }

    fn close_to(&mut self, height: usize) {
        while self.stack.len() > height.max(1) {
            self.close_top();
        }
    }

    /// Closes the top frame and attaches what it built to its parent.
    fn close_top(&mut self) {
        self.steps += 1;
        if self.stack.len() <= 1 {
            return;
        }
        let Some(f) = self.stack.pop() else {
            return;
        };
        match f {
            Frame::Blocks {
                kind: Container::Item,
                blocks,
                ..
            } => {
                let item = ListItem {
                    checked: None,
                    blocks,
                };
                match self.stack.last_mut() {
                    Some(Frame::List { items, .. }) => self.m.push(items, item),
                    _ => {
                        for b in item.blocks {
                            self.add_block(b);
                        }
                    }
                }
            }
            Frame::Blocks { blocks, .. } => {
                if !blocks.is_empty() {
                    self.add_block(Block::BlockQuote(blocks));
                }
            }
            Frame::List {
                start,
                items,
                loose,
            } => {
                if !items.is_empty() {
                    self.add_block(Block::List {
                        start,
                        tight: !loose,
                        items,
                    });
                }
            }
            Frame::Table(g) => self.finish_table(g),
            Frame::Row { cells, .. } => {
                if let Some(Frame::Table(g)) = self.stack.last_mut() {
                    place_row(g, cells, &mut self.m, &mut self.losses, &mut self.steps);
                }
            }
            Frame::Inlines {
                kind,
                mut inlines,
                implicit,
            } => match kind {
                Run::Para | Run::Heading(_) | Run::Caption | Run::Cell { .. } => {
                    trim_end(&mut inlines, true, &mut self.steps);
                    self.last_space = true;
                    self.flow = false;
                    self.pending_break = false;
                    match kind {
                        Run::Para if !inlines.is_empty() => {
                            self.add_block(Block::Paragraph(inlines));
                        }
                        Run::Heading(level) if !inlines.is_empty() => {
                            self.add_block(Block::Heading {
                                level,
                                content: inlines,
                            });
                        }
                        Run::Caption if !inlines.is_empty() => {
                            if let Some(Frame::Table(g)) = self.stack.last_mut() {
                                self.m.push(&mut g.caption, Block::Paragraph(inlines));
                            }
                        }
                        Run::Cell {
                            th,
                            colspan,
                            rowspan,
                        } if !(implicit && inlines.is_empty()) => {
                            if let Some(Frame::Row { cells, .. }) = self.stack.last_mut() {
                                let cell = Cell {
                                    content: inlines,
                                    th,
                                    colspan,
                                    rowspan,
                                };
                                self.m.push(cells, cell);
                            }
                        }
                        _ => {}
                    }
                }
                Run::Emph if !inlines.is_empty() => self.add_inline(Inline::Emph(inlines)),
                Run::Strong if !inlines.is_empty() => self.add_inline(Inline::Strong(inlines)),
                Run::Strike if !inlines.is_empty() => {
                    self.add_inline(Inline::Strikethrough(inlines));
                }
                Run::Link { dest, title } => {
                    self.links -= 1;
                    if !inlines.is_empty() {
                        self.add_inline(Inline::Link {
                            dest,
                            title,
                            content: inlines,
                            auto: false,
                        });
                    }
                }
                _ => {}
            },
            Frame::Flat {
                raw: true,
                mut text,
                ..
            } => {
                self.last_space = true;
                self.flow = false;
                self.pending_break = false;
                if !text.is_empty() {
                    // Ends with one line feed, like a Markdown code block.
                    if !text.ends_with('\n') && self.m.text(1) {
                        self.m.reserve_str(&mut text, 1);
                        text.push('\n');
                    }
                    self.add_block(Block::CodeBlock {
                        info: String::new(),
                        text,
                    });
                }
            }
            Frame::Flat {
                rp: Some(space),
                text,
                ..
            } => {
                // A hidden `rp` shows nothing, its white space included;
                // kept, its text is written again where the `rp` started
                // (charged a second time: the budget only errs early).
                self.last_space = space;
                if !text
                    .chars()
                    .all(|c| c.is_whitespace() || RP_BRACKETS.contains(c))
                {
                    self.text(&text);
                }
            }
            Frame::Flat { text, .. } => {
                if !text.is_empty() {
                    self.add_inline(Inline::Code(text));
                }
            }
        }
    }

    /// Adds a block to the innermost block container (the top frame,
    /// where blocks are opened).
    fn add_block(&mut self, b: Block) {
        for f in self.stack.iter_mut().rev() {
            self.steps += 1;
            if let Frame::Blocks { blocks, .. } = f {
                self.m.push(blocks, b);
                return;
            }
        }
    }

    /// Adds an inline to the innermost inline content (the top frame,
    /// where inlines are opened).
    fn add_inline(&mut self, i: Inline) {
        for f in self.stack.iter_mut().rev() {
            self.steps += 1;
            if let Frame::Inlines { inlines, .. } = f {
                self.m.push(inlines, i);
                self.flow = true;
                return;
            }
        }
    }

    fn finish_table(&mut self, g: Grid) {
        let Grid {
            head,
            body,
            foot,
            caption,
            ..
        } = g;
        for p in caption {
            self.add_block(p);
        }
        let has_head = !head.is_empty();
        let mut rows = head.into_iter().chain(body).chain(foot).peekable();
        let header = match rows.peek() {
            Some(r) if has_head || r.all_th => rows.next().map(|r| r.cells),
            _ => None,
        };
        // Every allocation here is fallible, like the rest of the builder:
        // the sizes are bounded by nodes already charged, but an allocator
        // that refuses ends the chapter with `Allocation`, not an abort.
        let mut kept: Vec<Vec<Vec<Inline>>> = Vec::new();
        if kept.try_reserve_exact(rows.size_hint().0).is_err() {
            self.m.fail_allocation();
            return;
        }
        for r in rows {
            self.m.push(&mut kept, r.cells);
        }
        let rows = kept;
        let width = rows
            .iter()
            .map(Vec::len)
            .chain(header.as_ref().map(Vec::len))
            .max()
            .unwrap_or(0);
        if width == 0 {
            return;
        }
        let mut head = header.unwrap_or_default();
        let mut rows = rows;
        for row in std::iter::once(&mut head).chain(rows.iter_mut()) {
            while row.len() < width {
                self.steps += 1;
                if !self.m.node() {
                    return;
                }
                self.m.push(row, Vec::new());
            }
        }
        let mut align = Vec::new();
        if align.try_reserve_exact(width).is_err() {
            self.m.fail_allocation();
            return;
        }
        align.resize(width, Align::None);
        self.add_block(Block::Table { align, head, rows });
    }

    /// Pending line break before new content (an image, inline code).
    fn content_starts(&mut self) {
        if self.pending_break {
            self.pending_break = false;
            if self.flow {
                self.line_break();
            }
        }
    }

    /// Ends the current line: the space before it goes, a hard break is
    /// added, white space after it writes nothing.
    fn line_break(&mut self) {
        if !self.m.node() {
            return;
        }
        if let Some(Frame::Inlines { inlines, .. }) = self.stack.last_mut() {
            trim_end(inlines, false, &mut self.steps);
            self.m.push(inlines, Inline::HardBreak);
        }
        self.last_space = true;
    }

    fn image(&mut self, e: &Element<'_>) {
        self.content_starts();
        let alt = e.attr(None, "alt").unwrap_or("");
        let alt = if alt.is_empty() || !self.m.node() {
            Vec::new()
        } else {
            vec![Inline::Text(self.m.own(alt))]
        };
        let dest = self.m.own(e.attr(None, "src").unwrap_or(""));
        let title = self.m.own(e.attr(None, "title").unwrap_or(""));
        if self.m.failed() {
            return;
        }
        self.add_inline(Inline::Image { dest, title, alt });
        self.last_space = false;
    }

    fn text(&mut self, s: &str) {
        let len = s.len() as u64;
        match self.stack.last_mut() {
            Some(Frame::Flat {
                raw: true,
                text,
                fresh,
                ..
            }) => {
                self.steps += len;
                let s = if std::mem::take(fresh) {
                    s.strip_prefix('\n').unwrap_or(s)
                } else {
                    s
                };
                if self.m.text(s.len()) {
                    self.m.reserve_str(text, s.len());
                    if !self.m.failed() {
                        text.push_str(s);
                    }
                }
            }
            Some(Frame::Flat { text, .. }) => {
                self.steps += 2 * len;
                let n = collapsed_len(s, self.last_space);
                if n > 0 && self.m.text(n) {
                    self.m.reserve_str(text, n);
                    if !self.m.failed() {
                        collapse_into(text, s, &mut self.last_space);
                    }
                }
            }
            Some(Frame::Inlines { .. }) => self.inline_text(s),
            _ => {
                self.steps += len;
                if s.bytes().all(is_ws) {
                    return;
                }
                self.ensure_inline();
                if !self.m.failed() {
                    self.text(s);
                }
            }
        }
    }

    fn inline_text(&mut self, s: &str) {
        let len = s.len() as u64;
        if self.pending_break {
            self.steps += len;
            if s.bytes().all(is_ws) {
                return;
            }
            self.content_starts();
        }
        self.steps += 2 * len;
        let n = collapsed_len(s, self.last_space);
        if n == 0 || !self.m.text(n) {
            return;
        }
        let Some(Frame::Inlines { inlines, .. }) = self.stack.last_mut() else {
            return;
        };
        if let Some(Inline::Text(t)) = inlines.last_mut() {
            self.m.reserve_str(t, n);
            if !self.m.failed() {
                collapse_into(t, s, &mut self.last_space);
            }
        } else if self.m.node() {
            let mut t = String::new();
            self.m.reserve_str(&mut t, n);
            if !self.m.failed() {
                collapse_into(&mut t, s, &mut self.last_space);
                self.m.push(inlines, Inline::Text(t));
            }
        }
        self.flow = true;
    }
}

/// Places a closed row's cells in the table's grid: columns a cell above
/// still covers get an empty cell, a cell spanning columns is followed by
/// empty ones (both [`Loss::MergedCells`]); every empty cell is a node.
fn place_row(
    g: &mut Grid,
    cells: Vec<Cell>,
    m: &mut Meter,
    losses: &mut BTreeSet<Loss>,
    steps: &mut u64,
) {
    let all_th = !cells.is_empty() && cells.iter().all(|c| c.th);
    let mut out: Vec<Vec<Inline>> = Vec::new();
    let mut col = 0usize;
    // An empty cell standing for a merged one.
    let mut merged = |out: &mut Vec<Vec<Inline>>, m: &mut Meter, steps: &mut u64| {
        *steps += 1;
        losses.insert(Loss::MergedCells);
        if m.node() {
            m.push(out, Vec::new());
        }
    };
    for c in cells {
        while col < g.spans.len() && g.spans[col] > 0 && !m.failed() {
            merged(&mut out, m, steps);
            col += 1;
        }
        m.push(&mut out, c.content);
        let end = col + c.colspan as usize;
        while g.spans.len() < end && !m.failed() {
            *steps += 1;
            m.push(&mut g.spans, 0);
        }
        if m.failed() {
            return;
        }
        for s in &mut g.spans[col..end] {
            *steps += 1;
            *s = c.rowspan;
        }
        for _ in 1..c.colspan {
            merged(&mut out, m, steps);
        }
        col = end;
        g.hi = g.hi.max(end);
    }
    while col < g.hi && !m.failed() {
        if g.spans[col] > 0 {
            merged(&mut out, m, steps);
        } else {
            *steps += 1;
            if m.node() {
                m.push(&mut out, Vec::new());
            }
        }
        col += 1;
    }
    // Next row: one row less to cover; columns from `hi` on are free.
    let mut hi = 0;
    for (i, s) in g.spans[..g.hi].iter_mut().enumerate() {
        *steps += 1;
        *s = s.saturating_sub(1);
        if *s > 0 {
            hi = i + 1;
        }
    }
    g.hi = hi;
    if out.is_empty() || m.failed() {
        return;
    }
    let row = GridRow { cells: out, all_th };
    let rows = match g.section {
        Section::Head => &mut g.head,
        Section::Body => &mut g.body,
        Section::Foot => &mut g.foot,
    };
    m.push(rows, row);
}

/// Drops the white space at the end of `v`, walking down the last inline
/// of each level (emphasis, links); with `breaks`, trailing line breaks
/// and emptied inlines go too.
fn trim_end(v: &mut Vec<Inline>, breaks: bool, steps: &mut u64) {
    let mut v = v;
    loop {
        *steps += 1;
        match v.last_mut() {
            Some(Inline::Text(t)) => {
                if t.ends_with(' ') {
                    t.pop();
                }
                if !t.is_empty() {
                    return;
                }
                v.pop();
                if !breaks {
                    return;
                }
            }
            Some(Inline::HardBreak) if breaks => {
                v.pop();
            }
            Some(
                Inline::Emph(c)
                | Inline::Strong(c)
                | Inline::Strikethrough(c)
                | Inline::Link { content: c, .. },
            ) if c.is_empty() => {
                v.pop();
            }
            _ => {
                let next = match v.last_mut() {
                    Some(
                        Inline::Emph(c)
                        | Inline::Strong(c)
                        | Inline::Strikethrough(c)
                        | Inline::Link { content: c, .. },
                    ) => c,
                    _ => return,
                };
                v = next;
            }
        }
    }
}

/// Bytes `s` collapses to after white space (`space`: the last thing
/// written was white space).
fn collapsed_len(s: &str, mut space: bool) -> usize {
    let mut n = 0;
    for &b in s.as_bytes() {
        if is_ws(b) {
            if !space {
                n += 1;
                space = true;
            }
        } else {
            n += 1;
            space = false;
        }
    }
    n
}

/// Appends `s` with every white space run as one space, none when
/// `space` (the last thing written was white space); `space` follows.
/// Room is reserved by the caller.
fn collapse_into(out: &mut String, s: &str, space: &mut bool) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if is_ws(b[i]) {
            if !*space {
                out.push(' ');
                *space = true;
            }
            i += 1;
        } else {
            // White space is ASCII: the run ends on a character boundary.
            let j = b[i..]
                .iter()
                .position(|&c| is_ws(c))
                .map_or(b.len(), |k| i + k);
            out.push_str(&s[i..j]);
            *space = false;
            i = j;
        }
    }
}
