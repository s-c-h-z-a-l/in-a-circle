//! Markdown (CommonMark) reader and writer.
//!
//! Writer: text and line breaks of the tree render exactly as written, so
//! plain text converts to Markdown without changing what the Markdown shows.
//!
//! Reader: CommonMark 0.31.2 in two phases. `block` builds the block tree
//! and collects link reference definitions (`Refs`); then the raw text of
//! every paragraph and heading goes through `inlines` (the `inline`
//! module) with all definitions known.

mod block;
mod entities;
mod inline;
mod punct;
mod scan;

use crate::budget::{Meter, Sink};
use crate::{Block, Document, Inline, LimitError, Limits, Loss, ReadError};
use std::collections::{BTreeSet, HashMap};

/// Reads CommonMark 0.31.2. NUL characters become U+FFFD; invalid UTF-8
/// is an error. Container nesting deeper than 256 block quotes/list items
/// is not opened: the extra markers stay text. Inline nesting (emphasis,
/// links, images) deeper than 256 is flattened: the wrappers are dropped,
/// their content is kept.
///
/// Two phases, as in the spec's parsing strategy: `block` builds the block
/// structure and collects link reference definitions, then every
/// paragraph's and heading's raw text goes through `inlines`.
///
/// Unbudgeted compatibility entry point; the shipped conversion uses
/// [`read_limited`].
pub fn read(bytes: &[u8]) -> Result<Document, ReadError> {
    read_limited(bytes, Limits::UNLIMITED)
}

/// [`read`] under `limits`: the input size is checked first; the NUL
/// replacement copy, every block and inline node, every owned string and
/// every copy of a reference's destination and title is charged before it
/// is made, so the parse stops at the budget (`ReadError::Limit`).
pub fn read_limited(bytes: &[u8], limits: Limits) -> Result<Document, ReadError> {
    if bytes.len() > limits.input_bytes {
        return Err(LimitError::Exceeded("input").into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ReadError::NotText)?;
    let mut m = Meter::new(limits);
    let nuls = bytes.iter().filter(|&&b| b == 0).count();
    let doc = if nuls > 0 {
        // Each NUL (1 byte) becomes U+FFFD (3 bytes): the copy is charged
        // as owned text and reserved fallibly before it is made.
        let mut copy = String::new();
        if m.text(bytes.len() + 2 * nuls) {
            m.reserve_str(&mut copy, bytes.len() + 2 * nuls);
        }
        m.result()?;
        for c in text.chars() {
            copy.push(if c == '\0' { '\u{fffd}' } else { c });
        }
        block::parse(&copy, &mut m)?
    } else {
        block::parse(text, &mut m)?
    };
    Ok(doc)
}

/// A link reference definition; `dest` and `title` are unescaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkDef {
    pub dest: String,
    pub title: String,
}

/// Link reference definitions by [`normalize_label`]ed label; the first
/// definition of a label wins.
pub(crate) type Refs = HashMap<String, LinkDef>;

/// Inline seam between the two phases. `raw` is a paragraph's or heading's
/// content: lines joined with `\n`, each line's leading spaces/tabs and the
/// block's final whitespace removed; interior trailing spaces are kept (they
/// decide hard breaks). `refs` holds every definition of the document.
fn inlines(raw: &str, refs: &Refs, m: &mut Meter) -> Vec<Inline> {
    inline::parse(raw, refs, m)
}

/// Matching form of a link label: Unicode case fold (approximated as
/// lowercase-then-uppercase, so `ẞ`, `ß` and `SS` match), internal runs of
/// whitespace collapsed to one space, ends trimmed.
pub(crate) fn normalize_label(label: &str) -> String {
    let folded = label.to_lowercase().to_uppercase();
    let mut out = String::with_capacity(folded.len());
    for word in folded.split_ascii_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Resolves backslash escapes (`\\` before ASCII punctuation) and character
/// references, as needed for link destinations, titles and code info
/// strings.
fn unescape(s: &str) -> String {
    decode(s, true)
}

/// The one place where backslash escapes (if `backslashes`) and character
/// references are decoded; text inlines use its [`char_ref`] directly.
fn decode(s: &str, backslashes: bool) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let (mut i, mut start) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\\' if backslashes && b.get(i + 1).is_some_and(u8::is_ascii_punctuation) => {
                out.push_str(&s[start..i]);
                out.push(char::from(b[i + 1]));
                i += 2;
                start = i;
            }
            b'&' => {
                out.push_str(&s[start..i]);
                start = i;
                i = match char_ref(b, i, &mut out) {
                    Some(end) => {
                        start = end;
                        end
                    }
                    None => i + 1,
                };
            }
            _ => i += 1,
        }
    }
    out.push_str(&s[start..]);
    out
}

/// The exact byte length `decode(s, backslashes)` produces, computed without
/// building the result (one reference's text at a time in a scratch buffer),
/// so a budget is charged the real size, not an estimate: `&nGt;` is 5
/// bytes and decodes to 6.
fn decoded_len(s: &str, backslashes: bool) -> usize {
    let b = s.as_bytes();
    let mut scratch = String::new();
    let (mut i, mut total) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\\' if backslashes && b.get(i + 1).is_some_and(u8::is_ascii_punctuation) => {
                total += 1;
                i += 2;
            }
            b'&' => {
                scratch.clear();
                match char_ref(b, i, &mut scratch) {
                    Some(end) => {
                        total += scratch.len();
                        i = end;
                    }
                    None => {
                        total += 1;
                        i += 1;
                    }
                }
            }
            _ => {
                total += 1;
                i += 1;
            }
        }
    }
    total
}

/// Longest HTML5 entity name is 31 characters; this bounds the scan.
const MAX_ENTITY_NAME: usize = 32;

/// Character reference at `s[i] == b'&'`: an HTML5 named entity (`&amp;`,
/// only with `;`), decimal (`&#` 1-7 digits `;`) or hexadecimal (`&#x` 1-6
/// digits `;`), as the spec defines them. Pushes the decoded text and
/// returns the end, or returns `None` (pushing nothing) if there is no
/// valid reference here. A numeric reference to 0, a surrogate or past
/// U+10FFFF becomes U+FFFD.
fn char_ref(s: &[u8], i: usize, out: &mut String) -> Option<usize> {
    let mut j = i + 1;
    if s.get(j) == Some(&b'#') {
        j += 1;
        let (radix, max) = match s.get(j) {
            Some(b'x' | b'X') => {
                j += 1;
                (16, 6)
            }
            _ => (10, 7),
        };
        let n = s[j..]
            .iter()
            .take(max + 1)
            .take_while(|&&c| char::from(c).is_digit(radix))
            .count();
        if n == 0 || n > max || s.get(j + n) != Some(&b';') {
            return None;
        }
        // At most 7 decimal or 6 hex digits: no overflow.
        let v = s[j..j + n].iter().fold(0u32, |v, &c| {
            v * radix + char::from(c).to_digit(radix).unwrap_or(0)
        });
        out.push(
            char::from_u32(v)
                .filter(|&c| c != '\0')
                .unwrap_or('\u{fffd}'),
        );
        return Some(j + n + 1);
    }
    let n = s[j..]
        .iter()
        .take(MAX_ENTITY_NAME + 1)
        .take_while(|c| c.is_ascii_alphanumeric())
        .count();
    if n == 0 || n > MAX_ENTITY_NAME || s.get(j + n) != Some(&b';') {
        return None;
    }
    let name = scan::utf8(&s[j..j + n]);
    let k = entities::ENTITIES
        .binary_search_by(|(e, _)| e.cmp(&name))
        .ok()?;
    out.push_str(entities::ENTITIES[k].1);
    Some(j + n + 1)
}

/// Writes CommonMark. Text is escaped so that it can never be read as
/// structure: the characters `\ ` * _ [ ] < > & | ! #` always get a
/// backslash, line-start `- + = ~` and the `.`/`)` of a line-start `N.`/`N)`
/// likewise, and any whitespace character at the start or end of a line becomes a
/// numeric entity (`&#32;`, `&#9;`, `&#xA0;`, ...).
/// Runs of two or more interior spaces/tabs are kept but reported as
/// `Loss::Whitespace` (a renderer may collapse them).
///
/// Blocks are separated by one blank line (tight list items by a newline
/// where that cannot change the meaning); non-empty output ends with exactly
/// one `\n`, empty output is `""`. Adjacent lists alternate `-`/`*` and
/// `.`/`)` so they stay separate lists. Recurses over nested blocks; the
/// readers bound nesting depth.
/// Unbudgeted compatibility entry point; see [`write_limited`].
pub fn write(doc: &Document) -> (String, BTreeSet<Loss>) {
    write_limited(doc, Limits::UNLIMITED).expect("allocation failed while writing Markdown")
}

/// [`write`] under `limits.output_bytes`: every piece (a nested block's
/// text included) is counted before it is appended, so the writer stops at
/// the budget instead of producing the whole output first.
pub fn write_limited(
    doc: &Document,
    limits: Limits,
) -> Result<(String, BTreeSet<Loss>), LimitError> {
    let mut losses = BTreeSet::new();
    let mut sink = Sink::new(limits);
    let mut out = blocks_to_string(&doc.blocks, true, &mut losses, &mut sink);
    if !out.is_empty() {
        sink.push(&mut out, '\n');
    }
    sink.result()?;
    Ok((out, losses))
}

fn blocks_to_string(
    blocks: &[Block],
    loose: bool,
    losses: &mut BTreeSet<Loss>,
    sink: &mut Sink,
) -> String {
    let mut out = String::new();
    let mut prev: Option<&Block> = None;
    // (ordered, alternate delimiter) of the directly preceding list.
    let mut prev_list: Option<(bool, bool)> = None;
    for b in blocks {
        if sink.failed() {
            break;
        }
        let alt = match (b, prev_list) {
            (Block::List { start, .. }, Some((ordered, alt))) if ordered == start.is_some() => !alt,
            _ => false,
        };
        let s = block_to_string(b, alt, losses, sink);
        if s.is_empty() {
            continue;
        }
        if let Some(p) = prev {
            let sep = if loose || needs_blank(p, b) {
                "\n\n"
            } else {
                "\n"
            };
            sink.push_str(&mut out, sep);
        }
        sink.push_str(&mut out, &s);
        prev_list = match b {
            Block::List { start, .. } => Some((start.is_some(), alt)),
            _ => None,
        };
        prev = Some(b);
    }
    out
}

/// Whether a single newline between two blocks (tight list item) could
/// change the meaning: lazy continuation, setext underline, HTML block
/// continuation, or a list that cannot interrupt a paragraph.
fn needs_blank(prev: &Block, next: &Block) -> bool {
    match prev {
        Block::List { .. } | Block::BlockQuote(_) | Block::HtmlBlock(_) => true,
        Block::Paragraph(_) => match next {
            Block::Paragraph(_) | Block::HtmlBlock(_) | Block::ThematicBreak => true,
            Block::List { start, items, .. } => {
                !(matches!(start, None | Some(1)) && items.first().is_some_and(|i| !i.is_empty()))
            }
            _ => false,
        },
        _ => matches!(next, Block::HtmlBlock(_)),
    }
}

fn block_to_string(b: &Block, alt: bool, losses: &mut BTreeSet<Loss>, sink: &mut Sink) -> String {
    match b {
        Block::Paragraph(c) => {
            let mut w = InlineWriter::new(true, false, losses, sink);
            w.inlines(c, true);
            w.out
        }
        Block::Heading { level, content } => {
            let mut w = InlineWriter::new(false, true, losses, sink);
            w.inlines(content, true);
            let text = w.out;
            let mut out = String::new();
            for _ in 0..usize::from((*level).clamp(1, 6)) {
                sink.push(&mut out, '#');
            }
            if !text.is_empty() {
                sink.push(&mut out, ' ');
                sink.push_str(&mut out, &text);
            }
            out
        }
        Block::CodeBlock { info, text } => code_block(info, text, sink),
        Block::BlockQuote(inner) => {
            let body = blocks_to_string(inner, true, losses, sink);
            if body.is_empty() {
                return body;
            }
            let mut out = String::new();
            for (i, line) in body.split('\n').enumerate() {
                if i > 0 {
                    sink.push(&mut out, '\n');
                }
                sink.push_str(&mut out, if line.is_empty() { ">" } else { "> " });
                sink.push_str(&mut out, line);
            }
            out
        }
        Block::List {
            start,
            tight,
            items,
        } => {
            let mut out = String::new();
            for (i, item) in items.iter().enumerate() {
                if sink.failed() {
                    break;
                }
                if i > 0 {
                    sink.push_str(&mut out, if *tight { "\n" } else { "\n\n" });
                }
                let marker = match start {
                    None if alt => "* ".to_string(),
                    None => "- ".to_string(),
                    Some(n) => format!(
                        "{}{} ",
                        n.saturating_add(i as u64).min(999_999_999),
                        if alt { ')' } else { '.' }
                    ),
                };
                let mut body = blocks_to_string(item, !*tight, losses, sink);
                // "- ---" would be a thematic break, not an item.
                if marker == "- " && (body == "---" || body.starts_with("---\n")) {
                    body.replace_range(..3, "***");
                }
                for (j, line) in body.split('\n').enumerate() {
                    if j == 0 {
                        sink.push_str(&mut out, &marker);
                    } else {
                        sink.push(&mut out, '\n');
                        if !line.is_empty() {
                            for _ in 0..marker.len() {
                                sink.push(&mut out, ' ');
                            }
                        }
                    }
                    sink.push_str(&mut out, line);
                }
                // An empty item writes its marker and takes it back; at the
                // very edge of the output budget the marker (1-2 bytes) can
                // count against it first, so a document exactly that full
                // may be refused one byte early. Accepted: refusing is the
                // safe side, and the budget is a limit, not a promise.
                if body.is_empty() {
                    out.truncate(out.trim_end().len());
                }
            }
            out
        }
        Block::ThematicBreak => "---".to_string(),
        Block::HtmlBlock(h) => h.trim_end_matches('\n').to_string(),
    }
}

/// Fenced code block. The fence is longer than any run of the fence
/// character in the text; an info string containing a backtick forces a
/// tilde fence (a backtick fence's info string may not contain backticks).
fn code_block(info: &str, text: &str, sink: &mut Sink) -> String {
    let info = info.replace(['\n', '\r'], " ");
    let info = info.trim();
    let ch = if info.contains('`') { '~' } else { '`' };
    let (mut longest, mut run) = (0usize, 0usize);
    for c in text.chars() {
        if c == ch {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    // The fence is at most one longer than a run in `text`: no more than
    // the text itself, which the sink counts as it is appended.
    let mut out = String::new();
    let fence_len = (longest + 1).max(3);
    let body = if text.is_empty() {
        0
    } else {
        text.strip_suffix('\n').unwrap_or(text).len() + 1
    };
    if !sink.room(&mut out, 2 * fence_len + info.len() + 1 + body) {
        return out;
    }
    for _ in 0..fence_len {
        sink.push(&mut out, ch);
    }
    sink.push_str(&mut out, info);
    sink.push(&mut out, '\n');
    if !text.is_empty() {
        sink.push_str(&mut out, text.strip_suffix('\n').unwrap_or(text));
        sink.push(&mut out, '\n');
    }
    for _ in 0..fence_len {
        sink.push(&mut out, ch);
    }
    out
}

struct InlineWriter<'a> {
    out: String,
    losses: &'a mut BTreeSet<Loss>,
    sink: &'a mut Sink,
    /// Nothing but whitespace emitted on the current line so far.
    line_start: bool,
    /// Escape block markers at line start (not needed inside headings).
    markers: bool,
    /// Line breaks become spaces (headings cannot contain them).
    single_line: bool,
}

impl<'a> InlineWriter<'a> {
    fn new(
        markers: bool,
        single_line: bool,
        losses: &'a mut BTreeSet<Loss>,
        sink: &'a mut Sink,
    ) -> Self {
        InlineWriter {
            out: String::new(),
            losses,
            sink,
            line_start: true,
            markers,
            single_line,
        }
    }

    /// `end_of_line`: the end of `items` is the end of a rendered line.
    fn inlines(&mut self, items: &[Inline], end_of_line: bool) {
        for (idx, item) in items.iter().enumerate() {
            if self.sink.failed() {
                return;
            }
            let next = items.get(idx + 1);
            match item {
                Inline::Text(t) => {
                    let eol = match next {
                        None => end_of_line,
                        Some(n) => matches!(n, Inline::SoftBreak),
                    };
                    self.text(t, eol);
                }
                Inline::Emph(c) => self.wrapped("*", c),
                Inline::Strong(c) => self.wrapped("**", c),
                Inline::Code(t) => self.code_span(t),
                Inline::Link {
                    dest,
                    title,
                    content,
                } => {
                    self.line_start = false;
                    self.sink.push(&mut self.out, '[');
                    self.inlines(content, false);
                    self.sink.push(&mut self.out, ']');
                    self.target(dest, title);
                }
                Inline::Image { dest, title, alt } => {
                    self.line_start = false;
                    self.sink.push_str(&mut self.out, "![");
                    self.inlines(alt, false);
                    self.sink.push(&mut self.out, ']');
                    self.target(dest, title);
                }
                Inline::SoftBreak => self.line_break("\n"),
                Inline::HardBreak => {
                    if next.is_some() || !end_of_line {
                        self.line_break("\\\n");
                    }
                }
                Inline::Html(h) => {
                    self.line_start = false;
                    self.sink.push_str(&mut self.out, h);
                }
            }
        }
    }

    fn line_break(&mut self, br: &str) {
        if self.single_line {
            self.sink.push(&mut self.out, ' ');
        } else {
            self.sink.push_str(&mut self.out, br);
            self.line_start = true;
        }
    }

    fn wrapped(&mut self, delim: &str, c: &[Inline]) {
        self.line_start = false;
        self.sink.push_str(&mut self.out, delim);
        self.inlines(c, false);
        self.sink.push_str(&mut self.out, delim);
    }

    fn code_span(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        self.line_start = false;
        let mut runs = BTreeSet::new();
        let mut run = 0usize;
        for c in t.chars() {
            if c == '`' {
                run += 1;
            } else if run > 0 {
                runs.insert(run);
                run = 0;
            }
        }
        if run > 0 {
            runs.insert(run);
        }
        let n = (1..).find(|n| !runs.contains(n)).unwrap_or(1);
        let ticks: String = std::iter::repeat_n('`', n).collect();
        let pad = t.starts_with('`')
            || t.ends_with('`')
            || (t.starts_with(' ') && t.ends_with(' ') && t.chars().any(|c| c != ' '));
        self.sink.push_str(&mut self.out, &ticks);
        if pad {
            self.sink.push(&mut self.out, ' ');
        }
        self.sink.push_str(&mut self.out, t);
        if pad {
            self.sink.push(&mut self.out, ' ');
        }
        self.sink.push_str(&mut self.out, &ticks);
    }

    /// `(dest "title")`.
    fn target(&mut self, dest: &str, title: &str) {
        self.sink.push(&mut self.out, '(');
        let plain = !dest.is_empty()
            && !dest.starts_with('<')
            && !dest.chars().any(|c| c.is_whitespace() || c.is_control());
        if plain {
            for c in dest.chars() {
                if matches!(c, '\\' | '(' | ')' | '&') {
                    self.sink.push(&mut self.out, '\\');
                }
                self.sink.push(&mut self.out, c);
            }
        } else {
            self.sink.push(&mut self.out, '<');
            for c in dest.chars() {
                if matches!(c, '\\' | '<' | '>' | '&') {
                    self.sink.push(&mut self.out, '\\');
                }
                self.sink
                    .push(&mut self.out, if c == '\n' || c == '\r' { ' ' } else { c });
            }
            self.sink.push(&mut self.out, '>');
        }
        if !title.is_empty() {
            self.sink.push_str(&mut self.out, " \"");
            for c in title.chars() {
                if matches!(c, '\\' | '"' | '&') {
                    self.sink.push(&mut self.out, '\\');
                }
                self.sink.push(&mut self.out, c);
            }
            self.sink.push(&mut self.out, '"');
        }
        self.sink.push(&mut self.out, ')');
    }

    fn text(&mut self, t: &str, end_of_line: bool) {
        let mut rest = t;
        while let Some(c) = rest.chars().next() {
            if self.sink.failed() {
                return;
            }
            match c {
                c if c.is_whitespace() && c != '\n' => {
                    let n = rest
                        .find(|c: char| !c.is_whitespace() || c == '\n')
                        .unwrap_or(rest.len());
                    let (run, after) = rest.split_at(n);
                    let trailing = (after.is_empty() && end_of_line) || after.starts_with('\n');
                    if self.line_start || trailing {
                        // Rendering would strip it; an entity keeps it.
                        for ch in run.chars() {
                            match ch {
                                ' ' => self.sink.push_str(&mut self.out, "&#32;"),
                                '\t' => self.sink.push_str(&mut self.out, "&#9;"),
                                _ => self
                                    .sink
                                    .push_str(&mut self.out, &format!("&#x{:X};", ch as u32)),
                            }
                        }
                    } else {
                        if run
                            .as_bytes()
                            .windows(2)
                            .any(|w| w.iter().all(|&b| b == b' ' || b == b'\t'))
                        {
                            self.losses.insert(Loss::Whitespace);
                        }
                        self.sink.push_str(&mut self.out, run);
                    }
                    rest = after;
                }
                '\n' => {
                    self.line_break("\n");
                    rest = &rest[1..];
                }
                d if d.is_ascii_digit() && self.line_start && self.markers => {
                    let n = rest
                        .find(|c: char| !c.is_ascii_digit())
                        .unwrap_or(rest.len());
                    let (digits, after) = rest.split_at(n);
                    self.sink.push_str(&mut self.out, digits);
                    rest = after;
                    if n <= 9
                        && let Some(p) = rest.chars().next().filter(|p| matches!(p, '.' | ')'))
                    {
                        self.sink.push(&mut self.out, '\\');
                        self.sink.push(&mut self.out, p);
                        rest = &rest[1..];
                    }
                    self.line_start = false;
                }
                _ => {
                    let escape = matches!(
                        c,
                        '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '&' | '|' | '!' | '#'
                    ) || (self.line_start
                        && self.markers
                        && matches!(c, '-' | '+' | '=' | '~'));
                    if escape {
                        self.sink.push(&mut self.out, '\\');
                    }
                    self.sink.push(&mut self.out, c);
                    self.line_start = false;
                    rest = &rest[c.len_utf8()..];
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Inline {
        Inline::Text(s.to_string())
    }
    fn para(s: &str) -> Block {
        Block::Paragraph(vec![t(s)])
    }
    fn doc(blocks: Vec<Block>) -> Document {
        Document { blocks }
    }
    /// `decoded_len` is the exact length `unescape`/`decode` produce:
    /// plain bytes 1, an escaped punctuation 1 (only when escapes count), a
    /// known reference its decoded bytes, an unknown `&...` 1 per byte.
    #[test]
    fn decoded_len_matches_the_decoded_text() {
        for (s, with_escapes, want) in [
            ("", true, 0),
            ("abc", true, 3),
            ("\\*", true, 1),
            ("\\*", false, 2),
            ("\\a", true, 2),
            ("a\\", true, 2),
            ("&amp;", false, 1),
            ("&nGt;", false, 6),
            ("&nope;", false, 6),
            ("a&b", false, 3),
            ("&", false, 1),
            ("x&amp;y&lt;", false, 4),
            ("\\&amp;", true, 5),
        ] {
            assert_eq!(decoded_len(s, with_escapes), want, "{s:?} {with_escapes}");
        }
    }

    #[test]
    fn read_rejects_invalid_utf8() {
        assert_eq!(read(b"a\xffb"), Err(ReadError::NotText));
        assert_eq!(read(b"\xe2\x82"), Err(ReadError::NotText));
    }

    #[test]
    fn read_replaces_nul() {
        assert_eq!(read(b"a\0b").unwrap(), doc(vec![para("a\u{fffd}b")]));
        let code = Block::CodeBlock {
            info: String::new(),
            text: "\u{fffd}\n".into(),
        };
        assert_eq!(read(b"    \0").unwrap(), doc(vec![code]));
    }

    #[test]
    fn read_empty() {
        assert_eq!(read(b"").unwrap(), doc(vec![]));
        assert_eq!(read(b" \n\t\n").unwrap(), doc(vec![]));
    }

    #[test]
    fn label_normalization() {
        assert_eq!(
            normalize_label("  Foo \t\n BAR "),
            normalize_label("foo bar")
        );
        assert_eq!(normalize_label("ẞ"), normalize_label("ss"));
        assert_eq!(normalize_label(" \n "), "");
        assert_ne!(normalize_label("a\\b"), normalize_label("ab"));
    }

    #[test]
    fn unescape_punctuation_only() {
        assert_eq!(unescape(r"\*a\b\\\"), r"*a\b\\");
        assert_eq!(unescape("\\é"), "\\é");
    }

    #[test]
    fn unescape_char_refs() {
        assert_eq!(unescape("a&amp;b&copy;&ngE;"), "a&b©≧\u{338}");
        assert_eq!(unescape("&#35;&#x22;&#X41;&#1114111;"), "#\"A\u{10ffff}");
        assert_eq!(
            unescape("&#0;&#xD800;&#x110000;&#9999999;"),
            "\u{fffd}".repeat(4)
        );
        // Too many digits, no `;`, unknown or empty names stay literal.
        let literal = "&#12345678; &#x1234567; &#; &#x; &amp &nosuch; &; & &#a;";
        assert_eq!(unescape(literal), literal);
        assert_eq!(unescape(r"\&amp;"), "&amp;");
        assert_eq!(unescape("&&amp;"), "&&");
        let long = format!("&{};", "a".repeat(40));
        assert_eq!(unescape(&long), long);
        assert_eq!(decode(r"\*&lt;", false), r"\*<");
    }

    #[test]
    fn entity_table_is_sorted_and_semicolon_only() {
        let t = entities::ENTITIES;
        assert!(t.len() > 2000);
        assert!(t.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(t.iter().all(|(n, v)| {
            n.bytes().all(|c| c.is_ascii_alphanumeric())
                && n.len() <= MAX_ENTITY_NAME
                && !v.is_empty()
        }));
    }

    fn md(s: &str) -> String {
        write(&doc(vec![para(s)])).0
    }
    fn list(start: Option<u64>, tight: bool, items: Vec<Vec<Block>>) -> Block {
        Block::List {
            start,
            tight,
            items,
        }
    }

    #[test]
    fn escapes_special_chars() {
        assert_eq!(
            md("*a* _b_ #c [d](e) <f> &amp;"),
            "\\*a\\* \\_b\\_ \\#c \\[d\\](e) \\<f\\> \\&amp;\n"
        );
        assert_eq!(md("\\ ` | ! >"), "\\\\ \\` \\| \\! \\>\n");
    }

    #[test]
    fn line_start_list_markers_escaped() {
        assert_eq!(md("- x"), "\\- x\n");
        assert_eq!(md("+ x"), "\\+ x\n");
        assert_eq!(md("1. x"), "1\\. x\n");
        assert_eq!(md("23) x"), "23\\) x\n");
        assert_eq!(md("=== x"), "\\=== x\n");
        assert_eq!(md("~~~"), "\\~~~\n");
        // Mid-line markers are plain text.
        assert_eq!(md("a - b + c = d ~ 1. e"), "a - b + c = d ~ 1. e\n");
        // Ten digits cannot start a list.
        assert_eq!(md("1234567890. x"), "1234567890. x\n");
        assert_eq!(md("12 x"), "12 x\n");
    }

    #[test]
    fn markers_escaped_after_every_line_break() {
        let d = doc(vec![Block::Paragraph(vec![
            t("a"),
            Inline::HardBreak,
            t("- b"),
            Inline::SoftBreak,
            t("2. c"),
        ])]);
        assert_eq!(write(&d).0, "a\\\n\\- b\n2\\. c\n");
    }

    #[test]
    fn leading_space_entity() {
        let (s, l) = write(&doc(vec![para("  x")]));
        assert_eq!(s, "&#32;&#32;x\n");
        assert!(l.is_empty());
        assert_eq!(md("\t- x"), "&#9;\\- x\n");
        let d = doc(vec![Block::Paragraph(vec![
            t("a"),
            Inline::HardBreak,
            t(" b"),
        ])]);
        assert_eq!(write(&d).0, "a\\\n&#32;b\n");
    }

    #[test]
    fn unicode_whitespace_at_line_edges_is_entity_encoded() {
        assert_eq!(md("\u{a0}a"), "&#xA0;a\n");
        assert_eq!(md("a\u{2003}"), "a&#x2003;\n");
        assert_eq!(md("\u{3000}\u{3000}x"), "&#x3000;&#x3000;x\n");
        assert_eq!(md("a \u{a0} b\n\u{85}c"), "a \u{a0} b\n&#x85;c\n");
        // Interior stays raw and unreported.
        let (s, l) = write(&doc(vec![para("a\u{a0}\u{a0}b")]));
        assert_eq!(s, "a\u{a0}\u{a0}b\n");
        assert!(l.is_empty());
    }

    #[test]
    fn trailing_space_entity() {
        assert_eq!(md("a  "), "a&#32;&#32;\n");
        let d = doc(vec![Block::Paragraph(vec![
            t("a "),
            Inline::SoftBreak,
            t("b"),
        ])]);
        assert_eq!(write(&d).0, "a&#32;\nb\n");
        // A space before a hard break survives the backslash.
        let d = doc(vec![Block::Paragraph(vec![
            t("a "),
            Inline::HardBreak,
            t("b"),
        ])]);
        assert_eq!(write(&d).0, "a \\\nb\n");
        assert_eq!(md("a b"), "a b\n");
    }

    #[test]
    fn hardbreak_backslash() {
        let d = doc(vec![Block::Paragraph(vec![
            t("a"),
            Inline::HardBreak,
            t("b"),
        ])]);
        let (s, l) = write(&d);
        assert_eq!(s, "a\\\nb\n");
        assert!(l.is_empty());
        // A trailing break has nothing to break; no literal backslash.
        let d = doc(vec![Block::Paragraph(vec![t("a"), Inline::HardBreak])]);
        assert_eq!(write(&d).0, "a\n");
    }

    #[test]
    fn inner_spaces_reported() {
        let (s, l) = write(&doc(vec![para("a  b")]));
        assert_eq!(s, "a  b\n");
        assert_eq!(l, BTreeSet::from([Loss::Whitespace]));
        let (_, l) = write(&doc(vec![para("a\t b")]));
        assert_eq!(l, BTreeSet::from([Loss::Whitespace]));
        assert!(write(&doc(vec![para("a b\tc")])).1.is_empty());
    }

    #[test]
    fn newline_in_text_is_a_line_break() {
        assert_eq!(md("a\n- b"), "a\n\\- b\n");
    }

    #[test]
    fn fence_longer_than_content() {
        let code = |text: &str| Block::CodeBlock {
            info: "rs".into(),
            text: text.into(),
        };
        assert_eq!(
            write(&doc(vec![code("a ``` b\n")])).0,
            "````rs\na ``` b\n````\n"
        );
        assert_eq!(write(&doc(vec![code("x\n")])).0, "```rs\nx\n```\n");
        assert_eq!(write(&doc(vec![code("x")])).0, "```rs\nx\n```\n");
        assert_eq!(write(&doc(vec![code("")])).0, "```rs\n```\n");
        assert_eq!(write(&doc(vec![code("\n")])).0, "```rs\n\n```\n");
        // The longest run counts, not the first.
        assert_eq!(
            write(&doc(vec![code("`\n`````\n")])).0,
            "``````rs\n`\n`````\n``````\n"
        );
        let tilde = Block::CodeBlock {
            info: "a`b".into(),
            text: "~~~\n".into(),
        };
        assert_eq!(write(&doc(vec![tilde])).0, "~~~~a`b\n~~~\n~~~~\n");
    }

    #[test]
    fn blocks_are_separated_and_end_with_one_newline() {
        assert_eq!(write(&doc(vec![])).0, "");
        assert_eq!(write(&doc(vec![para("a"), para("b")])).0, "a\n\nb\n");
        let d = doc(vec![
            para("a"),
            Block::Paragraph(vec![]),
            Block::BlockQuote(vec![]),
            list(None, true, vec![]),
            para("b"),
        ]);
        assert_eq!(write(&d).0, "a\n\nb\n");
    }

    #[test]
    fn heading_rule_html() {
        let h = |level, c: Vec<Inline>| Block::Heading { level, content: c };
        let d = doc(vec![
            h(2, vec![t("A # b"), Inline::SoftBreak, t("- c ")]),
            h(9, vec![]),
            Block::ThematicBreak,
            Block::HtmlBlock("<div>\n*x*\n</div>\n".into()),
            para("p"),
        ]);
        assert_eq!(
            write(&d).0,
            "## A \\# b - c&#32;\n\n######\n\n---\n\n<div>\n*x*\n</div>\n\np\n"
        );
    }

    #[test]
    fn quote_and_lists() {
        let d = doc(vec![
            Block::BlockQuote(vec![para("q1"), para("q2")]),
            list(
                None,
                true,
                vec![vec![para("a"), list(Some(1), true, vec![vec![para("b")]])]],
            ),
            list(Some(3), false, vec![vec![para("x")], vec![para("y")]]),
        ]);
        assert_eq!(
            write(&d).0,
            "> q1\n>\n> q2\n\n- a\n  1. b\n\n3. x\n\n4. y\n"
        );
    }

    #[test]
    fn adjacent_lists_stay_separate() {
        let l = |s| list(s, true, vec![vec![para("a")]]);
        let d = doc(vec![l(None), l(None), l(None), l(Some(1)), l(Some(1))]);
        assert_eq!(write(&d).0, "- a\n\n* a\n\n- a\n\n1. a\n\n1) a\n");
    }

    #[test]
    fn tight_item_separators_keep_meaning() {
        let item = |blocks| list(None, true, vec![blocks]);
        // Two paragraphs would merge on a single newline.
        assert_eq!(
            write(&doc(vec![item(vec![para("a"), para("b")])])).0,
            "- a\n\n  b\n"
        );
        // Ordered list not starting at 1 cannot interrupt a paragraph.
        let sub = list(Some(2), true, vec![vec![para("b")]]);
        assert_eq!(
            write(&doc(vec![item(vec![para("a"), sub])])).0,
            "- a\n\n  2. b\n"
        );
        // A rule after a paragraph would be a setext underline.
        assert_eq!(
            write(&doc(vec![item(vec![para("a"), Block::ThematicBreak])])).0,
            "- a\n\n  ---\n"
        );
        // A rule as first item content must not become a thematic break.
        assert_eq!(
            write(&doc(vec![item(vec![Block::ThematicBreak])])).0,
            "- ***\n"
        );
        // Text after a list is not a lazy continuation.
        assert_eq!(
            write(&doc(vec![item(vec![
                list(None, true, vec![vec![para("b")]]),
                para("c")
            ])]))
            .0,
            "- - b\n\n  c\n"
        );
        // Empty item.
        assert_eq!(write(&doc(vec![item(vec![])])).0, "-\n");
    }

    #[test]
    fn code_in_quote_and_list() {
        let code = Block::CodeBlock {
            info: String::new(),
            text: "x\n\ny\n".into(),
        };
        let d = doc(vec![
            Block::BlockQuote(vec![code.clone()]),
            list(Some(1), true, vec![vec![code]]),
        ]);
        assert_eq!(
            write(&d).0,
            "> ```\n> x\n>\n> y\n> ```\n\n1. ```\n   x\n\n   y\n   ```\n"
        );
    }

    #[test]
    fn inline_formatting() {
        let d = doc(vec![Block::Paragraph(vec![
            Inline::Emph(vec![t("a")]),
            t(" "),
            Inline::Strong(vec![t("b")]),
            t(" "),
            Inline::Code("c".into()),
            t(" "),
            Inline::Code("a`b".into()),
            t(" "),
            Inline::Code("`".into()),
            t(" "),
            Inline::Code(" x ".into()),
            Inline::Code(String::new()),
            Inline::Html("<b>".into()),
        ])]);
        assert_eq!(write(&d).0, "*a* **b** `c` ``a`b`` `` ` `` `  x  `<b>\n");
    }

    #[test]
    fn links_and_images() {
        let d = doc(vec![Block::Paragraph(vec![
            Inline::Link {
                dest: "https://x/a(b)".into(),
                title: "t\"q".into(),
                content: vec![t("l")],
            },
            Inline::Image {
                dest: "a b".into(),
                title: String::new(),
                alt: vec![t("alt")],
            },
            Inline::Link {
                dest: String::new(),
                title: String::new(),
                content: vec![],
            },
        ])]);
        assert_eq!(
            write(&d).0,
            "[l](https://x/a\\(b\\) \"t\\\"q\")![alt](<a b>)[](<>)\n"
        );
    }

    #[test]
    fn heading_breaks_become_spaces_and_markers_stay_plain() {
        let d = doc(vec![Block::Heading {
            level: 1,
            content: vec![t("1. a"), Inline::HardBreak, t("b")],
        }]);
        assert_eq!(write(&d).0, "# 1. a b\n");
    }
}
