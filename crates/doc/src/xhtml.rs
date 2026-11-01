//! The strict XHTML writer behind the user-facing book and web page
//! outputs. It is separate from [`crate::html`], which only reproduces the
//! reference renderers for the Markdown spec tests.
//!
//! Policy: the output is well-formed XML and valid (X)HTML.
//! - Characters XML 1.0 forbids (C0 controls except TAB, LF and CR, U+FFFE
//!   and U+FFFF) are left out, reported as [`Loss::ControlCharacters`].
//! - Raw HTML (block and inline) is dropped, reported as [`Loss::Html`].
//! - Only the five XML entities and numeric references are written.
//! - Headings carry GitHub-style ids (see [`slug`]); a `#x` link is
//!   resolved against them across the whole document and written to the
//!   target chapter file; an unresolved one keeps its text ([`Loss::Links`]).
//! - `http(s):` and `mailto:` links are kept; every other link keeps its
//!   text ([`Loss::Links`]).
//! - Images: every image becomes its alt text ([`Loss::Images`]), in both
//!   modes, so a page never loads anything from outside and the alt text is
//!   never lost. The one exception is a chapter of a book written by
//!   [`write_book_chapter`]: an image the book packages ([`Images`]) is an
//!   `<img>` of the packaged file, with its alt text.
//! - In such a book a link to another of its documents (`other.md`,
//!   `other.md#x`) is written to that document's chapter file and heading.
//! - Task list items are written as a text marker (`[x] ` / `[ ] `), table
//!   alignment as a class (`left`, `center`, `right`): no form element
//!   (many reading systems do not support them) and no `style` attribute
//!   (the stylesheet owns presentation, one rule per class).

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::budget::Sink;
use crate::{Align, Block, Inline, LimitError, Limits, ListItem, Loss};

/// What a chapter is written as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// The body content only (what goes between `<body>` and `</body>` of
    /// an EPUB chapter file; the packager writes the surrounding shell).
    Fragment,
    /// A whole self-contained page: doctype, `lang`, title, the embedded
    /// [`STYLE_STANDALONE`] stylesheet, no script, no `<img>`, nothing
    /// loaded from outside.
    Standalone { lang: String, title: String },
}

/// One chapter: its blocks and the name of the file it is written to (the
/// target of links that point into it).
#[derive(Debug, Clone)]
pub struct Chapter<'a> {
    pub blocks: &'a [Block],
    pub file: String,
}

/// Heading ids of a whole document: which chapter file holds an id, and
/// which id every heading of every chapter gets, in document order.
#[derive(Debug, Clone, Default)]
pub struct Anchors {
    /// id -> index into `files`.
    owner: HashMap<String, usize>,
    files: Vec<String>,
    /// Per chapter, the ids of its headings in document order.
    headings: Vec<Vec<String>>,
}

impl Anchors {
    /// Gives every heading of `chapters` its id. Collisions are numbered
    /// over the whole document in document order, as GitHub does: the
    /// second `intro` is `intro-1`, the third `intro-2`. Memory is bounded
    /// by the headings' text, which the reader's budgets already bound; a
    /// heading's text is held about four times here (plain text, base slug,
    /// owner key, used key), so the factor is 4 over the headings' bytes.
    /// Chapter file names must be unique ([`write_chapter`] finds its
    /// headings by file name).
    pub fn build(chapters: &[Chapter]) -> Anchors {
        let mut a = Anchors::default();
        let mut used: HashSet<String> = HashSet::new();
        // Next suffix to try per base slug, so n equal headings cost O(n)
        // rather than O(n^2) probes.
        let mut next: HashMap<String, usize> = HashMap::new();
        for (ci, ch) in chapters.iter().enumerate() {
            a.files.push(ch.file.clone());
            let mut ids = Vec::new();
            let mut texts = Vec::new();
            headings(ch.blocks, &mut texts);
            for text in texts {
                let base = slug(&text);
                let mut id = base.clone();
                if used.contains(&id) {
                    let n = next.entry(base.clone()).or_insert(0);
                    loop {
                        *n += 1;
                        id = format!("{base}-{n}");
                        if !used.contains(&id) {
                            break;
                        }
                    }
                }
                used.insert(id.clone());
                a.owner.insert(id.clone(), ci);
                ids.push(id);
            }
            a.headings.push(ids);
        }
        a
    }

    /// The ids of the top-level headings of chapter `chapter` (its index in
    /// the chapters given to [`Anchors::build`]), each with the heading's
    /// index in `blocks`, which must be that chapter's blocks. A book's
    /// table of contents links to these. Empty for a chapter it does not
    /// know.
    pub fn top_level_ids<'a>(&'a self, chapter: usize, blocks: &[Block]) -> Vec<(usize, &'a str)> {
        let Some(ids) = self.headings.get(chapter) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // Position in `ids`, which hold every heading depth first.
        let mut k = 0;
        for (i, b) in blocks.iter().enumerate() {
            if let Block::Heading { .. } = b {
                if let Some(id) = ids.get(k) {
                    out.push((i, id.as_str()));
                }
                k += 1;
            } else {
                k += count_headings(std::slice::from_ref(b));
            }
        }
        out
    }
}

/// How many headings [`headings`] finds in `bs` (the same walk).
fn count_headings(bs: &[Block]) -> usize {
    bs.iter()
        .map(|b| match b {
            Block::Heading { .. } => 1,
            Block::BlockQuote(c) => count_headings(c),
            Block::List { items, .. } => items.iter().map(|i| count_headings(&i.blocks)).sum(),
            _ => 0,
        })
        .sum()
}

/// Plain texts of the headings in `bs`, depth first in document order (the
/// order [`Writer::block`] meets them in).
fn headings(bs: &[Block], out: &mut Vec<String>) {
    for b in bs {
        match b {
            Block::Heading { content, .. } => {
                let mut s = String::new();
                plain(content, &mut s);
                out.push(s);
            }
            Block::BlockQuote(c) => headings(c, out),
            Block::List { items, .. } => {
                for item in items {
                    headings(&item.blocks, out);
                }
            }
            _ => {}
        }
    }
}

/// The text of `is` without any markup: what a heading id is made from.
pub fn plain(is: &[Inline], out: &mut String) {
    for i in is {
        match i {
            Inline::Text(t) | Inline::Code(t) => out.push_str(t),
            Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => plain(c, out),
            Inline::Link { content, .. } => plain(content, out),
            Inline::Image { alt, .. } => plain(alt, out),
            Inline::SoftBreak | Inline::HardBreak => out.push(' '),
            Inline::Html(_) => {}
        }
    }
}

/// XML 1.0 (fifth edition) `NameStartChar` above ASCII. An id is an XML
/// name (epubcheck checks it), so a character outside these ranges (for
/// example `²` or a circled digit, which Rust still calls numeric) cannot
/// stay in a slug.
fn name_start_range(c: char) -> bool {
    matches!(c as u32,
        0xC0..=0xD6 | 0xD8..=0xF6 | 0xF8..=0x2FF | 0x370..=0x37D | 0x37F..=0x1FFF
        | 0x200C..=0x200D | 0x2070..=0x218F | 0x2C00..=0x2FEF | 0x3001..=0xD7FF
        | 0xF900..=0xFDCF | 0xFDF0..=0xFFFD | 0x10000..=0xEFFFF)
}

/// The GitHub-style anchor of `text`: lowercase; Unicode letters and digits
/// stay; every whitespace character becomes `-`; `-` and `_` stay; other
/// punctuation is left out. An empty result, or one that does not start with
/// a letter or `_` (not an XML name), gets the prefix `h-`
/// (`2026 terv` -> `h-2026-terv`). Collisions are numbered by [`Anchors`].
/// A link target goes through the same function, so `#2026-terv` and
/// `#h-2026-terv` both find the heading.
pub fn slug(text: &str) -> String {
    let mut s = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_whitespace() {
            s.push('-');
        } else if c == '-'
            || c == '_'
            || c.is_ascii_alphanumeric()
            || (!c.is_ascii() && (c.is_alphabetic() || c.is_numeric()) && name_start_range(c))
        {
            s.push(c);
        }
    }
    let starts_ok = s
        .chars()
        .next()
        .is_some_and(|c| c == '_' || c.is_alphabetic());
    if starts_ok { s } else { format!("h-{s}") }
}

/// Stylesheet of [`Mode::Standalone`]: about 1 KB as asked of the page
/// (a comfortable measure of 40em centered, line height, table and code
/// frames, a dark scheme); the hard bound is 1536 B, checked by a test. No
/// font file, no `url(...)`, none of `<`, `>`, `&` (the page embeds it as
/// plain text).
pub const STYLE_STANDALONE: &str = "\
:root{color-scheme:light dark}
body{max-width:40em;margin:2em auto;padding:0 1em;line-height:1.6;font-family:system-ui,sans-serif}
pre,code{font-family:ui-monospace,monospace}
pre{padding:.8em;overflow-x:auto;border:1px solid #8886;border-radius:4px}
blockquote{margin:1em 0;padding-left:1em;border-left:3px solid #8886}
table{border-collapse:collapse;display:block;overflow-x:auto}
th,td{border:1px solid #8886;padding:.3em .6em}
.left{text-align:left}
.center{text-align:center}
.right{text-align:right}
img{max-width:100%}
@media (prefers-color-scheme:dark){body{background:#181a1b;color:#e6e6e6}a{color:#8ab4f8}}
";

/// Stylesheet of an EPUB chapter: only what a reader's default lacks (table
/// frames and alignment, wrapped code); typeface, size and measure stay the
/// reader's. Same character restrictions as [`STYLE_STANDALONE`].
pub const STYLE_EPUB: &str = "\
pre{white-space:pre-wrap}
code{font-family:monospace}
table{border-collapse:collapse}
th,td{border:1px solid;padding:.2em .5em}
.left{text-align:left}
.center{text-align:center}
.right{text-align:right}
img{max-width:100%}
";

/// Whether `c` may be written: XML 1.0 allows it and HTML5 does not call it
/// a noncharacter. The noncharacters are U+FDD0..=U+FDEF and the last two
/// code points of every plane (U+FFFE, U+FFFF, U+1FFFE, ...); XML allows
/// those of planes 1-16, but the standalone page must also be valid HTML.
pub fn xml_char(c: char) -> bool {
    let noncharacter = ('\u{FDD0}'..='\u{FDEF}').contains(&c) || (c as u32) & 0xFFFE == 0xFFFE;
    !noncharacter
        && matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
}

/// Writes one chapter. `anchors` must have been built from the chapters of
/// the whole document, this one included (found by its file name, so the
/// chapter file names must be unique; a chapter it does not know gets
/// headings without ids, and its links to other chapters still resolve). Every piece is charged to
/// `limits.output_bytes` before it is appended.
pub fn write_chapter(
    ch: &Chapter,
    anchors: &Anchors,
    mode: &Mode,
    limits: Limits,
) -> Result<(String, BTreeSet<Loss>), LimitError> {
    debug_assert!(
        anchors.files.iter().filter(|f| **f == ch.file).count() <= 1,
        "chapter file names must be unique"
    );
    let mut w = Writer::new(ch, anchors, None, limits);
    w.standalone = matches!(mode, Mode::Standalone { .. });
    if let Mode::Standalone { lang, title } = mode {
        w.raw("<!DOCTYPE html>\n<html xmlns=\"http://www.w3.org/1999/xhtml\" lang=\"");
        w.esc(lang, true);
        w.raw("\">\n<head>\n<meta charset=\"utf-8\"/>\n");
        w.raw("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"/>\n");
        w.raw("<title>");
        w.esc(title, false);
        w.raw("</title>\n<style>\n");
        w.raw(STYLE_STANDALONE);
        w.raw("</style>\n</head>\n<body>\n");
    }
    w.blocks(ch.blocks, false);
    w.cr();
    if w.standalone {
        w.raw("</body>\n</html>\n");
    }
    w.sink.result()?;
    Ok((w.out, w.losses))
}

/// The images a book packages, as a chapter of one of its documents sees
/// them.
pub trait Images {
    /// The path, relative to the chapter file, of the packaged image that
    /// `dest` (an image destination as the document writes it) refers to;
    /// `None` writes the image's alt text instead.
    fn src(&self, dest: &str) -> Option<String>;
}

/// What a chapter of a book of several documents may point to beyond its
/// own document.
pub struct Refs<'a> {
    /// Every document's heading ids, each built by [`Anchors::build`] from
    /// that document's chapters only, so a document's ids are the ones it
    /// would have on its own (GitHub numbers repeats per file). Chapter file
    /// names must be unique across the book.
    pub anchors: &'a [Anchors],
    /// Every document's key, [`normalize`]d and unique: its path relative
    /// to one folder that contains every document of the book (their
    /// deepest common folder), so `a/one.md` and `b/two.md` for documents
    /// in sibling folders and the file name for documents side by side. A
    /// link is resolved against the folder of its own document's key within
    /// that key space, so `../b/two.md` from `a/one.md` is `b/two.md`; a
    /// path that climbs out of the common folder (`../../x.md`) keeps a
    /// leading `..` and matches no key. An empty key is never a target.
    pub keys: &'a [String],
    /// The document the chapter belongs to: an index into `anchors` and
    /// `keys`.
    pub part: usize,
    /// The images the book packages for this document.
    pub images: &'a dyn Images,
}

/// Writes one chapter of document `refs.part` of a book, in
/// [`Mode::Fragment`]. As [`write_chapter`] with that document's anchors,
/// and besides: an image `refs.images` resolves is an `<img>` of the
/// packaged file (its alt text in `alt`), and a link to another document
/// of the book (`other.md`, `other.md#x`, relative to this document's key)
/// is written to that document's first chapter file or to the heading `x`
/// in it. A link to a document outside the book, or to a heading the
/// target document does not have, keeps its text ([`Loss::Links`]).
pub fn write_book_chapter(
    ch: &Chapter,
    refs: &Refs,
    limits: Limits,
) -> Result<(String, BTreeSet<Loss>), LimitError> {
    let mut w = Writer::new(ch, &refs.anchors[refs.part], Some(refs), limits);
    w.blocks(ch.blocks, false);
    w.cr();
    w.sink.result()?;
    Ok((w.out, w.losses))
}

/// Calls `f` with the destination of every image [`write_book_chapter`]
/// meets in `blocks`, in the order it meets them (repeats included): what
/// a book packer resolves before it writes the package document. An image
/// inside another image's alt text, in raw HTML, in a table without
/// columns or in a table cell past the header's width is not written as
/// an image and is not listed.
pub fn image_dests(blocks: &[Block], f: &mut dyn FnMut(&str)) {
    fn inlines(is: &[Inline], f: &mut dyn FnMut(&str)) {
        for i in is {
            match i {
                Inline::Image { dest, .. } => f(dest),
                Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => inlines(c, f),
                Inline::Link { content, .. } => inlines(content, f),
                Inline::Text(_)
                | Inline::Code(_)
                | Inline::SoftBreak
                | Inline::HardBreak
                | Inline::Html(_) => {}
            }
        }
    }
    for b in blocks {
        match b {
            Block::Paragraph(c) | Block::Heading { content: c, .. } => inlines(c, f),
            Block::BlockQuote(c) => image_dests(c, f),
            Block::List { items, .. } => {
                for item in items {
                    image_dests(&item.blocks, f);
                }
            }
            Block::Table { align, head, rows } => {
                for row in std::iter::once(head).chain(rows) {
                    for cell in row.iter().take(align.len()) {
                        inlines(cell, f);
                    }
                }
            }
            Block::CodeBlock { .. } | Block::ThematicBreak | Block::HtmlBlock(_) => {}
        }
    }
}

/// `path` with empty and `.` segments left out and each `..` taking away
/// the segment before it; a `..` with no segment before it stays, and so
/// does a leading `/`. Lexical only: nothing is looked up on disk.
pub fn normalize(path: &str) -> String {
    let mut segs: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." if segs.last().is_some_and(|l| *l != "..") => {
                segs.pop();
            }
            _ => segs.push(seg),
        }
    }
    let joined = segs.join("/");
    if path.starts_with('/') {
        format!("/{joined}")
    } else {
        joined
    }
}

struct Writer<'a> {
    sink: Sink,
    out: String,
    losses: BTreeSet<Loss>,
    anchors: &'a Anchors,
    /// The book this chapter is part of, for [`write_book_chapter`].
    refs: Option<&'a Refs<'a>>,
    file: &'a str,
    /// Ids of this chapter's headings, in document order.
    ids: &'a [String],
    next_heading: usize,
    standalone: bool,
}

impl<'a> Writer<'a> {
    fn new(
        ch: &'a Chapter,
        anchors: &'a Anchors,
        refs: Option<&'a Refs<'a>>,
        limits: Limits,
    ) -> Self {
        let own = anchors.files.iter().position(|f| *f == ch.file);
        Writer {
            sink: Sink::new(limits),
            out: String::new(),
            losses: BTreeSet::new(),
            anchors,
            refs,
            file: &ch.file,
            ids: own.map_or(&[][..], |i| &anchors.headings[i]),
            next_heading: 0,
            standalone: false,
        }
    }
}

impl Writer<'_> {
    fn raw(&mut self, s: &str) {
        self.sink.push_str(&mut self.out, s);
    }

    /// Ends the current line unless the output is at one.
    fn cr(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.sink.push(&mut self.out, '\n');
        }
    }

    /// Escaped `s`, with characters XML forbids left out. In an attribute
    /// the quotes and the whitespace XML would normalize are escaped too.
    fn esc(&mut self, s: &str, attr: bool) {
        let mut start = 0;
        for (i, c) in s.char_indices() {
            let rep = match c {
                '&' => "&amp;",
                '<' => "&lt;",
                '>' => "&gt;",
                '"' if attr => "&quot;",
                '\'' if attr => "&#39;",
                '\t' if attr => "&#9;",
                '\n' if attr => "&#10;",
                '\r' => "&#13;",
                c if !xml_char(c) => "",
                _ => continue,
            };
            self.sink.push_str(&mut self.out, &s[start..i]);
            if rep.is_empty() {
                self.losses.insert(Loss::ControlCharacters);
            } else {
                self.raw(rep);
            }
            start = i + c.len_utf8();
        }
        self.sink.push_str(&mut self.out, &s[start..]);
    }

    /// The plain text of `is`, escaped (an image's alt text).
    fn plain_text(&mut self, is: &[Inline], attr: bool) {
        for i in is {
            if self.sink.failed() {
                return;
            }
            match i {
                Inline::Text(t) | Inline::Code(t) => self.esc(t, attr),
                Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => {
                    self.plain_text(c, attr)
                }
                Inline::Link { content, .. } => self.plain_text(content, attr),
                Inline::Image { alt, .. } => self.plain_text(alt, attr),
                Inline::SoftBreak | Inline::HardBreak => self.raw(" "),
                Inline::Html(_) => {
                    self.losses.insert(Loss::Html);
                }
            }
        }
    }

    /// `s` as a URL in an attribute: bytes outside a conservative safe set
    /// are percent-encoded (controls included, so none needs removing).
    /// Valid per the WHATWG URL rules: a `%` not followed by two hex digits
    /// is written `%25`, only the first `#` starts the fragment (a later
    /// one is `%23`), and `[`/`]` stay literal in the authority (an IPv6
    /// host) and are encoded anywhere else. Known limit: a non-ASCII host
    /// is percent-encoded as UTF-8, which parsers decode to punycode but
    /// which is not a strictly valid host string.
    fn url(&mut self, s: &str) {
        let bytes = s.as_bytes();
        let authority = authority_range(s);
        let mut fragment = false;
        let mut start = 0;
        for (i, c) in s.char_indices() {
            let keep = match c {
                '%' => {
                    bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
                        && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
                }
                '#' => !std::mem::replace(&mut fragment, true),
                '[' | ']' => authority.contains(&i),
                c => c.is_ascii_alphanumeric() || (c.is_ascii() && "-_.+!*(),@?=;:/$~".contains(c)),
            };
            if keep {
                continue;
            }
            self.sink.push_str(&mut self.out, &s[start..i]);
            if c == '&' {
                self.raw("&amp;");
            } else {
                let mut buf = [0; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    self.raw(&format!("%{b:02X}"));
                }
            }
            start = i + c.len_utf8();
        }
        self.sink.push_str(&mut self.out, &s[start..]);
    }

    fn blocks(&mut self, bs: &[Block], tight: bool) {
        for b in bs {
            if self.sink.failed() {
                return;
            }
            self.block(b, tight);
        }
    }

    fn block(&mut self, b: &Block, tight: bool) {
        match b {
            Block::Paragraph(c) => {
                if tight {
                    self.inlines(c);
                } else {
                    self.cr();
                    self.raw("<p>");
                    self.inlines(c);
                    self.raw("</p>\n");
                }
            }
            Block::Heading { level, content } => {
                self.cr();
                let id = self.ids.get(self.next_heading);
                self.next_heading += 1;
                let tag = format!("<h{level}");
                self.raw(&tag);
                if let Some(id) = id {
                    self.raw(" id=\"");
                    self.esc(id, true);
                    self.raw("\"");
                }
                self.raw(">");
                self.inlines(content);
                self.raw(&format!("</h{level}>\n"));
            }
            Block::CodeBlock { info, text } => {
                self.cr();
                self.raw("<pre><code");
                let lang = info.split_whitespace().next().unwrap_or("");
                if !lang.is_empty() {
                    self.raw(" class=\"language-");
                    self.esc(lang, true);
                    self.raw("\"");
                }
                self.raw(">");
                self.esc(text, false);
                self.raw("</code></pre>\n");
            }
            Block::BlockQuote(c) => {
                self.cr();
                self.raw("<blockquote>\n");
                self.blocks(c, false);
                self.cr();
                self.raw("</blockquote>\n");
            }
            Block::List {
                start,
                tight,
                items,
            } => {
                self.cr();
                let tag = if start.is_some() { "ol" } else { "ul" };
                match start {
                    Some(n) if *n != 1 => self.raw(&format!("<ol start=\"{n}\">\n")),
                    _ => self.raw(&format!("<{tag}>\n")),
                }
                for item in items {
                    if self.sink.failed() {
                        return;
                    }
                    self.cr();
                    self.raw("<li>");
                    self.list_item(item, *tight);
                    self.raw("</li>\n");
                }
                self.cr();
                self.raw(&format!("</{tag}>\n"));
            }
            // A table with no column has no cell to write; `<tr>` needs one.
            Block::Table { align, .. } if align.is_empty() => {}
            Block::Table { align, head, rows } => {
                self.cr();
                self.raw("<table>\n<thead>\n");
                self.table_row("th", align, head);
                self.raw("</thead>\n");
                if !rows.is_empty() {
                    self.raw("<tbody>\n");
                    for row in rows {
                        if self.sink.failed() {
                            return;
                        }
                        self.table_row("td", align, row);
                    }
                    self.raw("</tbody>\n");
                }
                self.raw("</table>\n");
            }
            Block::ThematicBreak => {
                self.cr();
                self.raw("<hr/>\n");
            }
            Block::HtmlBlock(_) => {
                self.losses.insert(Loss::Html);
            }
        }
    }

    /// One `<tr>`; the reader makes every row as wide as `align`, a shorter
    /// one is padded with empty cells and a longer one cut regardless.
    fn table_row(&mut self, tag: &str, align: &[Align], cells: &[Vec<Inline>]) {
        self.raw("<tr>\n");
        for (i, a) in align.iter().enumerate() {
            self.raw(&format!("<{tag}"));
            match a {
                Align::None => {}
                Align::Left => self.raw(" class=\"left\""),
                Align::Center => self.raw(" class=\"center\""),
                Align::Right => self.raw(" class=\"right\""),
            }
            self.raw(">");
            if let Some(cell) = cells.get(i) {
                self.inlines(cell);
            }
            self.raw(&format!("</{tag}>\n"));
        }
        self.raw("</tr>\n");
    }

    /// The blocks of a list item; a task item's marker goes at the start of
    /// its first paragraph (inside it), or before the blocks otherwise.
    fn list_item(&mut self, item: &ListItem, tight: bool) {
        let Some(done) = item.checked else {
            self.blocks(&item.blocks, tight);
            return;
        };
        let marker = if done { "[x] " } else { "[ ] " };
        match item.blocks.split_first() {
            Some((Block::Paragraph(c), rest)) => {
                if tight {
                    self.raw(marker);
                    self.inlines(c);
                } else {
                    self.cr();
                    self.raw("<p>");
                    self.raw(marker);
                    self.inlines(c);
                    self.raw("</p>\n");
                }
                self.blocks(rest, tight);
            }
            _ => {
                self.raw(marker);
                self.blocks(&item.blocks, tight);
            }
        }
    }

    fn inlines(&mut self, is: &[Inline]) {
        for i in is {
            if self.sink.failed() {
                return;
            }
            self.inline(i);
        }
    }

    fn wrap(&mut self, tag: &str, c: &[Inline]) {
        self.raw(&format!("<{tag}>"));
        self.inlines(c);
        self.raw(&format!("</{tag}>"));
    }

    fn inline(&mut self, i: &Inline) {
        match i {
            Inline::Text(t) => self.esc(t, false),
            Inline::Emph(c) => self.wrap("em", c),
            Inline::Strong(c) => self.wrap("strong", c),
            Inline::Strikethrough(c) => self.wrap("del", c),
            Inline::Code(t) => {
                self.raw("<code>");
                self.esc(t, false);
                self.raw("</code>");
            }
            Inline::Link {
                dest,
                title,
                content,
                ..
            } => self.link(dest, title, content),
            Inline::Image { dest, title, alt } => {
                match self.refs.and_then(|r| r.images.src(dest)) {
                    Some(src) => {
                        self.raw("<img src=\"");
                        self.url(&src);
                        self.raw("\" alt=\"");
                        self.plain_text(alt, true);
                        self.raw("\"");
                        if !title.is_empty() {
                            self.raw(" title=\"");
                            self.esc(title, true);
                            self.raw("\"");
                        }
                        self.raw("/>");
                    }
                    None => {
                        self.losses.insert(Loss::Images);
                        self.plain_text(alt, false);
                    }
                }
            }
            Inline::SoftBreak => self.raw("\n"),
            Inline::HardBreak => self.raw("<br/>\n"),
            Inline::Html(_) => {
                self.losses.insert(Loss::Html);
            }
        }
    }

    fn link(&mut self, dest: &str, title: &str, content: &[Inline]) {
        let internal = match dest.strip_prefix('#') {
            Some(f) => self
                .resolve(self.anchors, f)
                .map(|(file, id)| (file, Some(id))),
            None if is_external(dest) => None,
            None => self.resolve_document(dest),
        };
        if internal.is_none() && !is_external(dest) {
            self.losses.insert(Loss::Links);
            self.inlines(content);
            return;
        }
        self.raw("<a href=\"");
        match internal {
            Some((file, id)) => {
                if file != self.file || id.is_none() {
                    self.url(&file);
                }
                if let Some(id) = id {
                    self.raw("#");
                    // Percent-encoded: a URI is plain ASCII; the reader
                    // decodes it back to the id.
                    for b in id.bytes() {
                        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                            self.sink.push(&mut self.out, b as char);
                        } else {
                            self.raw(&format!("%{b:02X}"));
                        }
                    }
                }
            }
            None => self.url(dest),
        }
        self.raw("\"");
        if !title.is_empty() {
            self.raw(" title=\"");
            self.esc(title, true);
            self.raw("\"");
        }
        self.raw(">");
        self.inlines(content);
        self.raw("</a>");
    }

    /// The (file, id) a `#fragment` link points to: the fragment is
    /// percent-decoded and sent through [`slug`] (so `#Intro`, `#intro` and
    /// `#2026-terv` find `intro` and `h-2026-terv`), then looked up.
    fn resolve(&self, anchors: &Anchors, fragment: &str) -> Option<(String, String)> {
        let id = slug(&percent_decode(fragment));
        let ci = *anchors.owner.get(&id)?;
        Some((anchors.files[ci].clone(), id))
    }

    /// The (file, id) a link to another document of the book points to:
    /// the path before the first `#`, percent-decoded, joined to the folder
    /// of this document's key and normalized, must be a document's key
    /// (every key is relative to the same common folder, see
    /// [`Refs::keys`]); no fragment (or an empty one) is that document's
    /// first chapter file, else the fragment must find a heading of that
    /// document. Outside a book, an absolute path or no match: `None`.
    fn resolve_document(&self, dest: &str) -> Option<(String, Option<String>)> {
        let refs = self.refs?;
        let (path, fragment) = match dest.split_once('#') {
            Some((p, f)) => (p, Some(f).filter(|f| !f.is_empty())),
            None => (dest, None),
        };
        let path = percent_decode(path);
        if path.is_empty() || path.starts_with('/') {
            return None;
        }
        let own = &refs.keys[refs.part];
        let target = match own.rfind('/') {
            Some(i) => normalize(&format!("{}/{path}", &own[..i])),
            None => normalize(&path),
        };
        if target.is_empty() {
            // `./` or `x/..`: a folder, never a document (an empty key
            // names none).
            return None;
        }
        let part = refs.keys.iter().position(|k| *k == target)?;
        let anchors = &refs.anchors[part];
        match fragment {
            None => Some((anchors.files.first()?.clone(), None)),
            Some(f) => self.resolve(anchors, f).map(|(file, id)| (file, Some(id))),
        }
    }
}

fn is_external(dest: &str) -> bool {
    ["http://", "https://", "mailto:"].iter().any(|p| {
        dest.get(..p.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(p))
    })
}

/// The byte range of the authority of `s` (`scheme://authority/...`), empty
/// when `s` has no `://` before its first `/`, `?` or `#`.
fn authority_range(s: &str) -> std::ops::Range<usize> {
    let Some(p) = s.find("://") else { return 0..0 };
    if s[..p].contains(['/', '?', '#']) {
        return 0..0;
    }
    let from = p + 3;
    let to = s[from..]
        .find(['/', '?', '#'])
        .map_or(s.len(), |e| from + e);
    from..to
}

/// `%XX` sequences decoded; a bad escape stays as written, bytes that are
/// not UTF-8 become U+FFFD.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Dialect, Document, md};

    fn doc(src: &str) -> Document {
        md::read_with(src.as_bytes(), Dialect::Gfm).expect("fixture reads")
    }

    fn chapter<'a>(blocks: &'a [Block], file: &str) -> Chapter<'a> {
        Chapter {
            blocks,
            file: file.to_string(),
        }
    }

    fn standalone() -> Mode {
        Mode::Standalone {
            lang: "hu".into(),
            title: "T & \"c\"".into(),
        }
    }

    /// One document as one chapter.
    fn render(src: &str, mode: &Mode) -> (String, BTreeSet<Loss>) {
        let d = doc(src);
        let ch = chapter(&d.blocks, "ch0001.xhtml");
        let a = Anchors::build(std::slice::from_ref(&ch));
        let (out, losses) = write_chapter(&ch, &a, mode, Limits::UNLIMITED).unwrap();
        well_formed(&out).unwrap_or_else(|e| panic!("{e}\n{out}"));
        (out, losses)
    }

    /// A small XML well-formedness checker: tags balance, every attribute
    /// is quoted and escaped, only the five XML entities and numeric
    /// references appear, no character XML forbids, no unclosed void tag.
    fn well_formed(s: &str) -> Result<(), String> {
        let mut stack: Vec<String> = Vec::new();
        let mut i = 0;
        while i < s.len() {
            let rest = &s[i..];
            let c = rest.chars().next().unwrap();
            if !xml_char(c) {
                return Err(format!("forbidden character {c:?}"));
            }
            if rest.starts_with("<!DOCTYPE html>") {
                i += "<!DOCTYPE html>".len();
            } else if c == '<' {
                let end = rest.find('>').ok_or("unterminated tag")?;
                let tag = &rest[1..end];
                if tag.contains('<') {
                    return Err(format!("'<' inside tag <{tag}>"));
                }
                if let Some(name) = tag.strip_prefix('/') {
                    if stack.pop().as_deref() != Some(name) {
                        return Err(format!("unbalanced </{name}>"));
                    }
                } else {
                    let selfclosed = tag.ends_with('/');
                    let tag = tag.trim_end_matches('/');
                    let name_end = tag.find(char::is_whitespace).unwrap_or(tag.len());
                    let name = &tag[..name_end];
                    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
                        return Err(format!("bad tag name in <{tag}>"));
                    }
                    attributes(&tag[name_end..])?;
                    if !selfclosed {
                        stack.push(name.to_string());
                    }
                }
                i += end + 1;
            } else if c == '&' {
                let end = rest.find(';').ok_or("unterminated entity")?;
                let e = &rest[1..end];
                let numeric = e
                    .strip_prefix('#')
                    .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()));
                if !(numeric || ["amp", "lt", "gt", "quot"].contains(&e)) {
                    return Err(format!("entity &{e};"));
                }
                i += end + 1;
            } else {
                i += c.len_utf8();
            }
        }
        if stack.is_empty() {
            Ok(())
        } else {
            Err(format!("unclosed {stack:?}"))
        }
    }

    fn attributes(mut s: &str) -> Result<(), String> {
        let mut first = true;
        while !s.is_empty() {
            let t = s.trim_start();
            if t.len() == s.len() && !first {
                return Err("attributes not separated by whitespace".into());
            }
            first = false;
            s = t;
            if s.is_empty() {
                break;
            }
            let eq = s
                .find("=\"")
                .ok_or_else(|| format!("unquoted attribute in {s:?}"))?;
            if eq == 0
                || !s[..eq]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                return Err(format!("bad attribute name in {s:?}"));
            }
            let v = &s[eq + 2..];
            let close = v.find('"').ok_or("unterminated attribute value")?;
            let val = &v[..close];
            if val.contains(['<', '>']) || val.chars().any(|c| matches!(c, '\t' | '\n' | '\r')) {
                return Err(format!("unescaped attribute value {val:?}"));
            }
            let mut amp = val;
            while let Some(p) = amp.find('&') {
                let e = amp[p..]
                    .find(';')
                    .ok_or("unterminated entity in attribute")?;
                let name = &amp[p + 1..p + e];
                if !(name.starts_with('#') || ["amp", "lt", "gt", "quot"].contains(&name)) {
                    return Err(format!("entity &{name}; in attribute"));
                }
                amp = &amp[p + e..];
            }
            s = &v[close + 1..];
        }
        Ok(())
    }

    #[test]
    fn checker_rejects_what_it_should() {
        for bad in [
            "<br>",
            "<p>a</div>",
            "<p>&nbsp;</p>",
            "<p a=b></p>",
            "<p a=\"<\"></p>",
            "<p>\u{1}</p>",
            "<p a=\"1\"b=\"2\"></p>",
            "<p>a < b</p>",
        ] {
            assert!(well_formed(bad).is_err(), "{bad:?} accepted");
        }
        assert!(well_formed("<p a=\"1\" b=\"&amp;\">x &lt; <br/>&#13;</p>").is_ok());
    }

    #[test]
    fn slug_keeps_unicode_and_prefixes_digits() {
        assert_eq!(slug("Áttekintés"), "áttekintés");
        assert_eq!(slug("2026 terv"), "h-2026-terv");
        assert_eq!(slug("Hello, World! (v1.2)"), "hello-world-v12");
        assert_eq!(slug("a  b_c-d"), "a--b_c-d");
        assert_eq!(slug(""), "h-");
        assert_eq!(slug("!!!"), "h-");
        assert_eq!(slug("-x"), "h--x");
        // Numeric to Rust but not an XML name character.
        assert_eq!(slug("x\u{b2}\u{2460}"), "x");
        // The heading id and a link to it, in the Hungarian case.
        let (out, losses) = render(
            "# Áttekintés\n\n# 2026 terv\n\n[lásd](#áttekintés) [x](#2026-terv) [y](#h-2026-terv)\n",
            &Mode::Fragment,
        );
        assert!(out.contains("<h1 id=\"áttekintés\">"), "{out}");
        assert!(out.contains("<h1 id=\"h-2026-terv\">"), "{out}");
        assert!(out.contains("href=\"#%C3%A1ttekint%C3%A9s\""), "{out}");
        assert_eq!(out.matches("href=\"#h-2026-terv\"").count(), 2, "{out}");
        assert!(losses.is_empty());
    }

    #[test]
    fn slug_collisions_number_in_order() {
        let d1 = doc("# A\n\n# A\n\n# A-1\n");
        let d2 = doc("# A\n\n> # A\n");
        let chapters = [
            chapter(&d1.blocks, "c1.xhtml"),
            chapter(&d2.blocks, "c2.xhtml"),
        ];
        let a = Anchors::build(&chapters);
        assert_eq!(a.headings[0], ["a", "a-1", "a-1-1"]);
        assert_eq!(a.headings[1], ["a-2", "a-3"]);
        assert_eq!(a.owner["a-2"], 1);
        assert_eq!(a.owner["a"], 0);
        let (out, _) = write_chapter(&chapters[1], &a, &Mode::Fragment, Limits::UNLIMITED).unwrap();
        assert!(
            out.contains("<h1 id=\"a-2\">") && out.contains("<h1 id=\"a-3\">"),
            "{out}"
        );
    }

    #[test]
    fn links_across_chapters_name_the_file() {
        let d1 = doc("[to two](#second) [self](#first)\n\n# First\n");
        let d2 = doc("# Second\n");
        let chapters = [
            chapter(&d1.blocks, "c1.xhtml"),
            chapter(&d2.blocks, "c2.xhtml"),
        ];
        let a = Anchors::build(&chapters);
        let (out, losses) =
            write_chapter(&chapters[0], &a, &Mode::Fragment, Limits::UNLIMITED).unwrap();
        assert!(
            out.contains("<a href=\"c2.xhtml#second\">to two</a>"),
            "{out}"
        );
        assert!(out.contains("<a href=\"#first\">self</a>"), "{out}");
        assert!(losses.is_empty());
        well_formed(&out).unwrap();
    }

    #[test]
    fn top_level_ids_skip_nested_headings() {
        // The nested headings (in a quote and a list) take ids first, so the
        // top-level ones must be found past them, as the writer numbers them.
        let d1 = doc("# Intro\n");
        let d2 = doc("# Intro\n\n> ## Intro\n\n- ### Intro\n\n## Intro\n\ntext\n");
        let chapters = [
            chapter(&d1.blocks, "c1.xhtml"),
            chapter(&d2.blocks, "c2.xhtml"),
        ];
        let a = Anchors::build(&chapters);
        assert_eq!(a.top_level_ids(0, &d1.blocks), [(0, "intro")]);
        assert_eq!(
            a.top_level_ids(1, &d2.blocks),
            [(0, "intro-1"), (3, "intro-4")]
        );
        let (out, _) = write_chapter(&chapters[1], &a, &Mode::Fragment, Limits::UNLIMITED).unwrap();
        assert!(out.contains("<h2 id=\"intro-4\">"), "{out}");
        // A chapter the anchors do not know has no ids.
        assert!(a.top_level_ids(2, &d1.blocks).is_empty());
    }

    #[test]
    fn forbidden_xml_chars_removed() {
        let d = Document {
            blocks: vec![
                Block::Heading {
                    level: 2,
                    content: vec![Inline::Text("h\u{1}i".into())],
                },
                Block::Paragraph(vec![
                    Inline::Text("a\u{1}b\u{FFFE}c\u{FFFF}d\u{b}e\tf\u{FDD0}\u{FDEF}\u{1FFFE}\u{10FFFF}\u{FDCF}\u{FDF0}\u{1FFFD}g".into()),
                    Inline::Code("x\u{0}y".into()),
                    Inline::Link {
                        dest: "https://x.y/\u{1}".into(),
                        title: "t\u{2}t".into(),
                        content: vec![Inline::Text("l".into())],
                        auto: false,
                    },
                ]),
                Block::CodeBlock {
                    info: "r\u{3}s".into(),
                    text: "z\u{4}".into(),
                },
            ],
        };
        let ch = chapter(&d.blocks, "c.xhtml");
        let a = Anchors::build(std::slice::from_ref(&ch));
        let (out, losses) = write_chapter(&ch, &a, &Mode::Fragment, Limits::UNLIMITED).unwrap();
        assert_eq!(losses, BTreeSet::from([Loss::ControlCharacters]));
        assert!(out.contains("abcde\tf\u{FDCF}\u{FDF0}\u{1FFFD}g"), "{out}");
        assert!(
            out.contains("<code>xy</code>") && out.contains("title=\"tt\""),
            "{out}"
        );
        assert!(
            out.contains("language-rs") && out.contains(">z</code>"),
            "{out}"
        );
        assert!(out.contains("href=\"https://x.y/%01\""), "{out}");
        assert!(out.contains("id=\"hi\""), "{out}");
        well_formed(&out).unwrap_or_else(|e| panic!("{e}\n{out}"));
    }

    #[test]
    fn table_without_columns_is_skipped() {
        let blocks = vec![
            Block::Table {
                align: vec![],
                head: vec![],
                rows: vec![vec![]],
            },
            Block::Paragraph(vec![Inline::Text("x".into())]),
        ];
        let ch = chapter(&blocks, "c.xhtml");
        let a = Anchors::build(std::slice::from_ref(&ch));
        let (out, _) = write_chapter(&ch, &a, &Mode::Fragment, Limits::UNLIMITED).unwrap();
        assert_eq!(out, "<p>x</p>\n");
    }

    #[test]
    fn raw_html_dropped() {
        let (out, losses) = render(
            "<script>alert(1)</script>\n\nx <b>y</b> <script>z</script>\n\n<div>\nq\n</div>\n",
            &Mode::Fragment,
        );
        assert!(
            !out.contains("<script") && !out.contains("<b>") && !out.contains("<div"),
            "{out}"
        );
        assert!(out.contains("<p>x y z</p>"), "{out}");
        assert_eq!(losses, BTreeSet::from([Loss::Html]));
        // Each kind reports on its own.
        let (out, losses) = render("<div>\nq\n</div>\n", &Mode::Fragment);
        assert_eq!(out, "");
        assert_eq!(losses, BTreeSet::from([Loss::Html]));
        let (out, losses) = render("a <b>c</b>\n", &Mode::Fragment);
        assert_eq!(out, "<p>a c</p>\n");
        assert_eq!(losses, BTreeSet::from([Loss::Html]));
    }

    #[test]
    fn internal_link_to_missing_target_is_text() {
        let (out, losses) = render(
            "[a](#nope) [b](other.md) [c](javascript:alert(1)) [d](#) [e](ftp://x.y)\n",
            &Mode::Fragment,
        );
        assert!(!out.contains("<a"), "{out}");
        assert!(out.contains("<p>a b c d e</p>"), "{out}");
        assert_eq!(losses, BTreeSet::from([Loss::Links]));
    }

    #[test]
    fn external_links_kept() {
        let (out, losses) = render(
            "[a](https://x.y/?q=1&r=2 \"T \\\"q\\\"\") [b](http://x.y) [c](mailto:me@x.y) [d](HTTPS://X.Y) www.example.org\n",
            &Mode::Fragment,
        );
        assert!(
            out.contains("<a href=\"https://x.y/?q=1&amp;r=2\" title=\"T &quot;q&quot;\">a</a>"),
            "{out}"
        );
        assert!(out.contains("<a href=\"http://x.y\">b</a>"), "{out}");
        assert!(out.contains("<a href=\"mailto:me@x.y\">c</a>"), "{out}");
        assert!(out.contains("<a href=\"HTTPS://X.Y\">d</a>"), "{out}");
        assert!(
            out.contains("<a href=\"http://www.example.org\">www.example.org</a>"),
            "{out}"
        );
        assert!(losses.is_empty());
    }

    /// Regression (found by the fuzzer): a non-ASCII URL was cut inside a
    /// character.
    #[test]
    fn non_ascii_url_is_percent_encoded() {
        let (out, _) = render(
            "[a](https://x.y/\u{fffd}é?q=ü) <https://x.y/😀>\n",
            &Mode::Fragment,
        );
        assert!(
            out.contains("href=\"https://x.y/%EF%BF%BD%C3%A9?q=%C3%BC\""),
            "{out}"
        );
        assert!(out.contains("https://x.y/%F0%9F%98%80"), "{out}");
    }

    /// Regression: a stray `%` and a second `#` made the href invalid
    /// (the Nu validator rejects both).
    #[test]
    fn url_is_valid_for_a_stray_percent_and_a_second_hash() {
        let (out, _) = render(
            "[a](https://a/50%off) [b](https://a/x#y#z) [c](https://a/%41%zz%4)\n",
            &Mode::Fragment,
        );
        assert!(out.contains("href=\"https://a/50%25off\""), "{out}");
        assert!(out.contains("href=\"https://a/x#y%23z\""), "{out}");
        assert!(out.contains("href=\"https://a/%41%25zz%254\""), "{out}");
    }

    /// The brackets of an IPv6 host stay; elsewhere they are encoded.
    #[test]
    fn url_keeps_brackets_in_the_authority_only() {
        let (out, _) = render(
            "<http://[::1]:8080/> [a](http://[::1]/p[1]?q=[2])\n",
            &Mode::Fragment,
        );
        assert!(out.contains("href=\"http://[::1]:8080/\""), "{out}");
        assert!(
            out.contains("href=\"http://[::1]/p%5B1%5D?q=%5B2%5D\""),
            "{out}"
        );
    }

    /// A later `://` (here in a query) does not make the text before it an
    /// authority: the brackets after the `?` are encoded.
    #[test]
    fn url_authority_needs_the_scheme_before_any_separator() {
        let (out, _) = render("[a](mailto:a@b?u=http://[::1])\n", &Mode::Fragment);
        assert!(
            out.contains("href=\"mailto:a@b?u=http://%5B::1%5D\""),
            "{out}"
        );
    }

    #[test]
    fn local_image_is_alt_text() {
        for mode in [Mode::Fragment, standalone()] {
            let (out, losses) = render("![an *alt* & more](pic.png) ![](../x.png)\n", &mode);
            assert!(!out.contains("<img"), "{out}");
            assert!(out.contains("an alt &amp; more "), "{out}");
            assert_eq!(losses, BTreeSet::from([Loss::Images]));
        }
        // A remote image is alt text too: nothing is loaded from outside.
        for mode in [Mode::Fragment, standalone()] {
            let (out, losses) = render("![r](https://x.y/a.png)\n", &mode);
            assert!(!out.contains("<img") && out.contains("<p>r</p>"), "{out}");
            assert_eq!(losses, BTreeSet::from([Loss::Images]));
        }
    }

    #[test]
    fn alt_always_written() {
        for mode in [Mode::Fragment, standalone()] {
            let (out, losses) = render(
                "![p \"q\" & r](https://x.y/b.png \"ti'tle\") ![](https://x.y/a.png)\n",
                &mode,
            );
            assert!(
                !out.contains("<img") && !out.contains("src=") && !out.contains("title="),
                "{out}"
            );
            assert!(out.contains("<p>p \"q\" &amp; r </p>"), "{out}");
            assert_eq!(losses, BTreeSet::from([Loss::Images]));
        }
    }

    #[test]
    fn gfm_nodes_written() {
        let (out, losses) = render(
            "| a | b | c |\n|:--|:-:|--:|\n| 1 | 2 | 3 |\n\n~~gone~~\n\n- [x] done\n- [ ] todo\n- plain\n",
            &Mode::Fragment,
        );
        assert!(
            out.contains("<table>\n<thead>\n<tr>\n<th class=\"left\">a</th>"),
            "{out}"
        );
        assert!(
            out.contains("<th class=\"center\">b</th>")
                && out.contains("<td class=\"right\">3</td>"),
            "{out}"
        );
        assert!(out.contains("<tbody>") && out.contains("</table>"), "{out}");
        assert!(out.contains("<del>gone</del>"), "{out}");
        assert!(
            out.contains("<li>[x] done</li>") && out.contains("<li>[ ] todo</li>"),
            "{out}"
        );
        assert!(!out.contains("style=") && !out.contains("<input"), "{out}");
        assert!(losses.is_empty());
    }

    #[test]
    fn standalone_page_shape() {
        let (out, losses) = render("# T\n\ntext\n", &standalone());
        assert!(out.starts_with("<!DOCTYPE html>\n<html "), "{out}");
        assert!(out.contains(" lang=\"hu\">"), "{out}");
        assert!(out.contains("<meta charset=\"utf-8\"/>"), "{out}");
        assert!(out.contains("<title>T &amp; \"c\"</title>"), "{out}");
        assert!(
            out.contains(&format!("<style>\n{STYLE_STANDALONE}</style>")),
            "{out}"
        );
        assert!(
            !out.contains("<script") && !out.contains("<link") && !out.contains("src="),
            "{out}"
        );
        // A remote image does not put anything outside into the page.
        let (out, _) = render("![x](https://t.example/p.png)\n", &standalone());
        assert!(!out.contains("src=") && !out.contains("t.example"), "{out}");
        assert!(out.ends_with("</body>\n</html>\n"), "{out}");
        assert!(losses.is_empty());
        // A fragment is the body content only.
        let (frag, _) = render("# T\n\ntext\n", &Mode::Fragment);
        assert_eq!(frag, "<h1 id=\"t\">T</h1>\n<p>text</p>\n");
    }

    #[test]
    fn standalone_loads_nothing_from_outside() {
        let (out, losses) = render(
            "![r](https://t.example/p.png) ![l](pic.png) ![](http://t.example/q.png \"t\")\n\n# H\n",
            &standalone(),
        );
        for bad in ["<img", "<link", "<script", "@import", "url("] {
            assert!(!out.contains(bad), "{bad}: {out}");
        }
        assert!(!out.contains("t.example"), "{out}");
        assert_eq!(losses, BTreeSet::from([Loss::Images]));
    }

    #[test]
    fn stylesheets_stay_small_and_inert() {
        assert!(STYLE_STANDALONE.len() <= 1536, "{}", STYLE_STANDALONE.len());
        assert!(STYLE_EPUB.len() <= STYLE_STANDALONE.len());
        for s in [STYLE_STANDALONE, STYLE_EPUB] {
            assert!(!s.contains(['<', '>', '&']) && !s.contains("url(") && !s.contains("@import"));
        }
        assert!(STYLE_STANDALONE.contains("prefers-color-scheme:dark"));
    }

    #[test]
    fn output_is_well_formed_xml() {
        let corpus = [
            "# H *e* `c` [l](https://x.y) ![i](p.png)\n\nPara with <i>raw</i> & ampersand < less > more \"q\" 'a'.  \nHard\nsoft\n\n> quote\n> # in quote\n\n1. one\n   - nested\n2. two\n\n3. three\n\n5. five\n\n```rust ignore\nlet a = \"<&>\";\n```\n\n---\n\n    indented\n\n<div>html</div>\n",
            "| a | b |\n|---|:-:|\n| `x` | [y](#a) |\n|  |  |\n\n- [x] a\n\n  para\n- [ ] \n- [ ] > q\n",
            "",
            "\u{1}\n\n[\u{2}](https://a.b)\n",
        ];
        for src in corpus {
            for mode in [Mode::Fragment, standalone()] {
                render(src, &mode);
            }
        }
    }

    #[test]
    fn charges_output_budget() {
        let src = "# Title\n\nSome *text* with a [link](https://x.y) and `code`.\n\n- a\n- b\n";
        let d = doc(src);
        let ch = chapter(&d.blocks, "c.xhtml");
        let a = Anchors::build(std::slice::from_ref(&ch));
        for mode in [Mode::Fragment, standalone()] {
            let (full, _) = write_chapter(&ch, &a, &mode, Limits::UNLIMITED).unwrap();
            let at = |n: usize| Limits {
                output_bytes: n,
                ..Limits::UNLIMITED
            };
            let (same, _) = write_chapter(&ch, &a, &mode, at(full.len())).unwrap();
            assert_eq!(same, full);
            assert_eq!(
                write_chapter(&ch, &a, &mode, at(full.len() - 1)).unwrap_err(),
                LimitError::Exceeded("output")
            );
            assert_eq!(
                write_chapter(&ch, &a, &mode, at(0)).unwrap_err(),
                LimitError::Exceeded("output")
            );
        }
    }

    #[test]
    fn unknown_chapter_has_no_ids_but_still_writes() {
        let d = doc("# H\n\n[x](#h)\n");
        let ch = chapter(&d.blocks, "c.xhtml");
        let (out, losses) =
            write_chapter(&ch, &Anchors::default(), &Mode::Fragment, Limits::UNLIMITED).unwrap();
        assert!(out.contains("<h1>H</h1>"), "{out}");
        assert!(!out.contains("<a"), "{out}");
        assert_eq!(losses, BTreeSet::from([Loss::Links]));
    }

    /// Text and attribute escaping, arm by arm: `>` and a carriage return
    /// are escaped everywhere; quotes, tab and newline only in an
    /// attribute (in text they stay as written).
    #[test]
    fn every_escape_arm() {
        let d = Document {
            blocks: vec![Block::Paragraph(vec![
                Inline::Text("a>b<c&d\"e'f\tg\nh\ri".into()),
                Inline::Link {
                    dest: "https://x.y/".into(),
                    title: "a>b<c&d\"e'f\tg\nh\ri".into(),
                    content: vec![Inline::Text("l".into())],
                    auto: false,
                },
            ])],
        };
        let ch = chapter(&d.blocks, "c.xhtml");
        let a = Anchors::build(std::slice::from_ref(&ch));
        let (out, _) = write_chapter(&ch, &a, &Mode::Fragment, Limits::UNLIMITED).unwrap();
        assert_eq!(
            out,
            "<p>a&gt;b&lt;c&amp;d\"e'f\tg\nh&#13;i\
<a href=\"https://x.y/\" title=\"a&gt;b&lt;c&amp;d&quot;e&#39;f&#9;g&#10;h&#13;i\">l</a></p>\n"
        );
    }

    /// An ordered list says where it starts only when that is not 1.
    #[test]
    fn list_start_is_written_when_not_one() {
        let (out, _) = render("1. a\n2. b\n", &Mode::Fragment);
        assert_eq!(out, "<ol>\n<li>a</li>\n<li>b</li>\n</ol>\n");
        let (out, _) = render("3. a\n4. b\n", &Mode::Fragment);
        assert_eq!(out, "<ol start=\"3\">\n<li>a</li>\n<li>b</li>\n</ol>\n");
        let (out, _) = render("0. a\n", &Mode::Fragment);
        assert_eq!(out, "<ol start=\"0\">\n<li>a</li>\n</ol>\n");
        let (out, _) = render("- a\n", &Mode::Fragment);
        assert_eq!(out, "<ul>\n<li>a</li>\n</ul>\n");
    }

    /// The count of headings in a block is what the heading ids are
    /// numbered by: a block without a heading takes none, a quote with two
    /// takes two.
    #[test]
    fn top_level_ids_count_the_headings_before_them() {
        let d = doc("text\n\n# Top\n");
        let ch = chapter(&d.blocks, "c.xhtml");
        let a = Anchors::build(std::slice::from_ref(&ch));
        assert_eq!(a.top_level_ids(0, &d.blocks), [(1, "top")]);
        let d = doc("> # Q\n>\n> # Q\n\ntext\n\n# Top\n");
        let ch = chapter(&d.blocks, "c.xhtml");
        let a = Anchors::build(std::slice::from_ref(&ch));
        assert_eq!(a.top_level_ids(0, &d.blocks), [(2, "top")]);
    }

    #[test]
    fn percent_decoding_edges() {
        assert_eq!(percent_decode(""), "");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("%41"), "A");
        assert_eq!(percent_decode("ab%41cd"), "abAcd");
        assert_eq!(percent_decode("abcd%41"), "abcdA");
        assert_eq!(percent_decode("%41%42%43"), "ABC");
        assert_eq!(percent_decode("%c3%A1"), "\u{e1}");
        assert_eq!(percent_decode("%2f%3a"), "/:");
        assert_eq!(percent_decode("%7e%20"), "~ ");
        // The two digits weigh 16 and 1.
        assert_eq!(percent_decode("%10"), "\u{10}");
        assert_eq!(percent_decode("%01"), "\u{1}");
        // A bad or cut escape stays as written.
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("%4"), "%4");
        assert_eq!(percent_decode("ab%4"), "ab%4");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%4z"), "%4z");
        assert_eq!(percent_decode("%z4"), "%z4");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%%41"), "%A");
        // Bytes that are not UTF-8 become U+FFFD.
        assert_eq!(percent_decode("%ff"), "\u{fffd}");
        assert_eq!(percent_decode("%C3"), "\u{fffd}");
    }

    /// A link to an escaped heading id finds the heading.
    #[test]
    fn percent_encoded_link_finds_its_heading() {
        let (out, losses) = render(
            "# \u{c1}ttekint\u{e9}s\n\n[x](#%C3%A1ttekint%C3%A9s) [y](#%c3%81ttekint%c3%a9s)\n",
            &Mode::Fragment,
        );
        assert_eq!(
            out.matches("href=\"#%C3%A1ttekint%C3%A9s\"").count(),
            2,
            "{out}"
        );
        assert!(losses.is_empty());
    }

    /// Packaged images: destination -> path in the package.
    struct Packaged(Vec<(&'static str, &'static str)>);

    impl Images for Packaged {
        fn src(&self, dest: &str) -> Option<String> {
            self.0
                .iter()
                .find(|(d, _)| *d == dest)
                .map(|(_, s)| s.to_string())
        }
    }

    /// A book of `parts` ((key, Markdown) each, one chapter per part,
    /// `ch0001.xhtml` onwards), every chapter written with `images`.
    fn render_book(parts: &[(&str, &str)], images: &dyn Images) -> Vec<(String, BTreeSet<Loss>)> {
        let docs: Vec<Document> = parts.iter().map(|(_, src)| doc(src)).collect();
        let chapters: Vec<Chapter> = docs
            .iter()
            .enumerate()
            .map(|(i, d)| chapter(&d.blocks, &format!("ch{:04}.xhtml", i + 1)))
            .collect();
        let anchors: Vec<Anchors> = chapters
            .iter()
            .map(|c| Anchors::build(std::slice::from_ref(c)))
            .collect();
        let keys: Vec<String> = parts.iter().map(|(k, _)| normalize(k)).collect();
        chapters
            .iter()
            .enumerate()
            .map(|(part, ch)| {
                let refs = Refs {
                    anchors: &anchors,
                    keys: &keys,
                    part,
                    images,
                };
                let (out, losses) = write_book_chapter(ch, &refs, Limits::UNLIMITED).unwrap();
                well_formed(&out).unwrap_or_else(|e| panic!("{e}\n{out}"));
                (out, losses)
            })
            .collect()
    }

    #[test]
    fn packaged_image_is_an_img() {
        let images = Packaged(vec![("a b.png", "img/0001.png"), ("x.jpg", "img/0002.jpg")]);
        let out = render_book(
            &[(
                "a.md",
                "![an *alt* & \"q\"](<a b.png> \"T & 'u'\") ![](x.jpg) ![miss](b.png)\n",
            )],
            &images,
        );
        let (out, losses) = &out[0];
        assert!(
            out.contains("<img src=\"img/0001.png\" alt=\"an alt &amp; &quot;q&quot;\" title=\"T &amp; &#39;u&#39;\"/>"),
            "{out}"
        );
        assert!(
            out.contains("<img src=\"img/0002.jpg\" alt=\"\"/>"),
            "{out}"
        );
        // The one the book does not package is its alt text.
        assert!(out.ends_with(" miss</p>\n"), "{out}");
        assert_eq!(*losses, BTreeSet::from([Loss::Images]));

        let (out, losses) = &render_book(&[("a.md", "![p](x.jpg)\n")], &images)[0];
        assert_eq!(out, "<p><img src=\"img/0002.jpg\" alt=\"p\"/></p>\n");
        assert!(losses.is_empty(), "{losses:?}");
        // The plain chapter writer never writes one, in either mode.
        for mode in [Mode::Fragment, standalone()] {
            let (out, _) = render("![p](x.jpg)\n", &mode);
            assert!(!out.contains("<img"), "{out}");
        }
    }

    #[test]
    fn links_between_documents() {
        let parts = [
            (
                "a.md",
                "# Intro\n\n[1](sub/b.md#Part%20Two) [2](sub/b.md) [3](./sub/../c.md#top) \
                 [4](missing.md) [5](sub/b.md#nope) [6](/c.md) [7](a.md#intro) [8](#intro) \
                 [9](sub/b.md#) [10](sub/b.md#intro)\n",
            ),
            (
                "sub/b.md",
                "# Intro\n\n# Part Two\n\n[back](../a.md#intro) [own](#intro) [c](../c.md)\n",
            ),
            ("c.md", "# Top\n"),
        ];
        let out = render_book(&parts, &Packaged(vec![]));
        let (a, la) = &out[0];
        for want in [
            "<a href=\"ch0002.xhtml#part-two\">1</a>",
            "<a href=\"ch0002.xhtml\">2</a>",
            "<a href=\"ch0003.xhtml#top\">3</a>",
            " 4 ",
            " 5 ",
            " 6 ",
            "<a href=\"#intro\">7</a>",
            "<a href=\"#intro\">8</a>",
            "<a href=\"ch0002.xhtml\">9</a>",
            // A document's ids are its own: b's first heading is `intro`
            // too, not `intro-1`.
            "<a href=\"ch0002.xhtml#intro\">10</a>",
        ] {
            assert!(a.contains(want), "{want}\n{a}");
        }
        assert_eq!(*la, BTreeSet::from([Loss::Links]));
        let (b, lb) = &out[1];
        assert!(b.contains("<h1 id=\"intro\">Intro</h1>"), "{b}");
        assert!(b.contains("<a href=\"ch0001.xhtml#intro\">back</a>"), "{b}");
        assert!(b.contains("<a href=\"#intro\">own</a>"), "{b}");
        assert!(b.contains("<a href=\"ch0003.xhtml\">c</a>"), "{b}");
        assert!(lb.is_empty(), "{lb:?}");

        // A percent-encoded path finds its document.
        let out = render_book(
            &[("a.md", "[n](my%20notes.md#x)\n"), ("my notes.md", "# X\n")],
            &Packaged(vec![]),
        );
        assert!(
            out[0].0.contains("<a href=\"ch0002.xhtml#x\">n</a>"),
            "{}",
            out[0].0
        );
        // Outside a book a link to another file keeps its text.
        let (out, losses) = render("# X\n\n[n](a.md#x)\n", &Mode::Fragment);
        assert!(!out.contains("<a "), "{out}");
        assert_eq!(losses, BTreeSet::from([Loss::Links]));
    }

    /// `image_dests` lists exactly the images the writer asks the book
    /// for, in its order: every place an image can be written, and the
    /// places where it is not.
    #[test]
    fn image_dests_match_the_writer() {
        struct Asked(std::cell::RefCell<Vec<String>>);
        impl Images for Asked {
            fn src(&self, dest: &str) -> Option<String> {
                self.0.borrow_mut().push(dest.to_string());
                Some("img/0001.png".to_string())
            }
        }
        let src = "\
# ![h](h.png)

p ![1](1.png) *![2](2.png)* **![3](3.png)** ~~![4](4.png)~~ [![5](5.png)](https://x.y/) ![![no](no.png)](6.png)

> ![7](7.png)
>
> - ![8](8.png)
>   1. ![9](9.png)

| ![10](10.png) | b |
|---|---|
| ![11](11.png) | ![12](12.png) | ![past](past.png) |

```
![code](code.png)
```

<p>![html](html.png)</p>

![13](13.png) <img src=\"inline.png\">
";
        let d = doc(src);
        let mut listed = Vec::new();
        image_dests(&d.blocks, &mut |s| listed.push(s.to_string()));
        let asked = Asked(Default::default());
        let ch = chapter(&d.blocks, "ch0001.xhtml");
        let anchors = [Anchors::build(std::slice::from_ref(&ch))];
        let keys = ["a.md".to_string()];
        let refs = Refs {
            anchors: &anchors,
            keys: &keys,
            part: 0,
            images: &asked,
        };
        let (out, _) = write_book_chapter(&ch, &refs, Limits::UNLIMITED).unwrap();
        well_formed(&out).unwrap_or_else(|e| panic!("{e}\n{out}"));
        let want: Vec<String> = [
            "h", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13",
        ]
        .iter()
        .map(|n| format!("{n}.png"))
        .collect();
        assert_eq!(listed, want);
        assert_eq!(*asked.0.borrow(), want);

        // A tree the reader would not build: a row wider than the header,
        // and a table without columns, are cut as the writer cuts them.
        let img = |n: &str| {
            vec![Inline::Image {
                dest: format!("{n}.png"),
                title: String::new(),
                alt: Vec::new(),
            }]
        };
        let blocks = [
            Block::Table {
                align: vec![Align::None],
                head: vec![img("a"), img("wide")],
                rows: vec![vec![img("b"), img("wider")]],
            },
            Block::Table {
                align: Vec::new(),
                head: vec![img("none")],
                rows: Vec::new(),
            },
        ];
        let mut listed = Vec::new();
        image_dests(&blocks, &mut |s| listed.push(s.to_string()));
        let asked = Asked(Default::default());
        let ch = chapter(&blocks, "ch0001.xhtml");
        let refs = Refs {
            images: &asked,
            ..refs
        };
        write_book_chapter(&ch, &refs, Limits::UNLIMITED).unwrap();
        assert_eq!(listed, ["a.png", "b.png"]);
        assert_eq!(*asked.0.borrow(), listed);
    }

    #[test]
    fn normalize_is_lexical() {
        for (p, want) in [
            ("a.md", "a.md"),
            ("./a.md", "a.md"),
            ("x/../a.md", "a.md"),
            ("x//y/./a.md", "x/y/a.md"),
            ("../a.md", "../a.md"),
            ("../../x/../a.md", "../../a.md"),
            ("x/../../a.md", "../a.md"),
            ("/x/../a.md", "/a.md"),
            ("", ""),
        ] {
            assert_eq!(normalize(p), want, "{p}");
        }
    }
}
