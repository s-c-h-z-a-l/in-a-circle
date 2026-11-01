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
//!   modes. No `<img>` is written, so a page never loads anything from
//!   outside and the alt text is never lost.
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
fn plain(is: &[Inline], out: &mut String) {
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
fn xml_char(c: char) -> bool {
    let noncharacter = ('\u{FDD0}'..='\u{FDEF}').contains(&c) || (c as u32) & 0xFFFE == 0xFFFE;
    !noncharacter
        && matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
}

/// Writes one chapter. `anchors` must have been built from the chapters of
/// the whole document, this one included (found by its file name, so the
/// chapter file names must be unique; a chapter it does not know gets
/// headings without ids, and its links to other chapters still resolve). Every piece is charged to `limits.output_bytes`
/// before it is appended.
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
    let own = anchors.files.iter().position(|f| *f == ch.file);
    let mut w = Writer {
        sink: Sink::new(limits),
        out: String::new(),
        losses: BTreeSet::new(),
        anchors,
        file: &ch.file,
        ids: own.map_or(&[][..], |i| &anchors.headings[i]),
        next_heading: 0,
        standalone: matches!(mode, Mode::Standalone { .. }),
    };
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

struct Writer<'a> {
    sink: Sink,
    out: String,
    losses: BTreeSet<Loss>,
    anchors: &'a Anchors,
    file: &'a str,
    /// Ids of this chapter's headings, in document order.
    ids: &'a [String],
    next_heading: usize,
    standalone: bool,
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
    fn url(&mut self, s: &str) {
        let mut start = 0;
        for (i, c) in s.char_indices() {
            if c.is_ascii_alphanumeric() || (c.is_ascii() && "-_.+!*(),%#@?=;:/$~".contains(c)) {
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
            Inline::Image { alt, .. } => {
                self.losses.insert(Loss::Images);
                self.plain_text(alt, false);
            }
            Inline::SoftBreak => self.raw("\n"),
            Inline::HardBreak => self.raw("<br/>\n"),
            Inline::Html(_) => {
                self.losses.insert(Loss::Html);
            }
        }
    }

    fn link(&mut self, dest: &str, title: &str, content: &[Inline]) {
        let internal = dest.strip_prefix('#').and_then(|f| self.resolve(f));
        if internal.is_none() && !is_external(dest) {
            self.losses.insert(Loss::Links);
            self.inlines(content);
            return;
        }
        self.raw("<a href=\"");
        match internal {
            Some((file, id)) => {
                if file != self.file {
                    self.url(&file);
                }
                self.raw("#");
                // Percent-encoded: a URI is plain ASCII; the reader decodes
                // it back to the id.
                for b in id.bytes() {
                    if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                        self.sink.push(&mut self.out, b as char);
                    } else {
                        self.raw(&format!("%{b:02X}"));
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
    fn resolve(&self, fragment: &str) -> Option<(String, String)> {
        let id = slug(&percent_decode(fragment));
        let ci = *self.anchors.owner.get(&id)?;
        Some((self.anchors.files[ci].clone(), id))
    }
}

fn is_external(dest: &str) -> bool {
    ["http://", "https://", "mailto:"].iter().any(|p| {
        dest.get(..p.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(p))
    })
}

/// `%XX` sequences decoded; a bad escape stays as written, bytes that are
/// not UTF-8 become U+FFFD.
fn percent_decode(s: &str) -> String {
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
}
