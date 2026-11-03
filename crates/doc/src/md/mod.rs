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
mod inline;
mod punct;
mod scan;

use crate::budget::{Meter, Sink};
use crate::{Align, Block, Dialect, Document, Inline, LimitError, Limits, Loss, ReadError};
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

/// [`read`] in the given dialect. `CommonMark` is exactly [`read`].
pub fn read_with(bytes: &[u8], dialect: Dialect) -> Result<Document, ReadError> {
    read_limited_with(bytes, Limits::UNLIMITED, dialect)
}

/// [`read`] under `limits`: the input size is checked first; the NUL
/// replacement copy, every block and inline node, every owned string and
/// every copy of a reference's destination and title is charged before it
/// is made, so the parse stops at the budget (`ReadError::Limit`).
pub fn read_limited(bytes: &[u8], limits: Limits) -> Result<Document, ReadError> {
    read_limited_with(bytes, limits, Dialect::CommonMark)
}

/// [`read_limited`] in the given dialect; `CommonMark` is exactly
/// [`read_limited`].
pub fn read_limited_with(
    bytes: &[u8],
    limits: Limits,
    dialect: Dialect,
) -> Result<Document, ReadError> {
    let mut m = Meter::new(limits);
    read_metered(bytes, limits, dialect, &mut m)
}

/// [`read_limited_with`] under what is left of a budget several documents
/// share: the document is read under `left`, and once it is read its nodes
/// and owned text are taken off `left.nodes` and `left.text_bytes`, so
/// the documents read in turn together never pass the budget `left`
/// started with. `left.input_bytes` stays one document's limit and the
/// output budget is untouched. On an error `left` is unchanged (the
/// failed document holds nothing).
pub fn read_spending(
    bytes: &[u8],
    left: &mut Limits,
    dialect: Dialect,
) -> Result<Document, ReadError> {
    let mut m = Meter::new(*left);
    let doc = read_metered(bytes, *left, dialect, &mut m)?;
    let (nodes, text) = m.spent();
    // The meter never lets a charge pass its limits: no underflow.
    left.nodes -= nodes;
    left.text_bytes -= text;
    Ok(doc)
}

fn read_metered(
    bytes: &[u8],
    limits: Limits,
    dialect: Dialect,
    m: &mut Meter,
) -> Result<Document, ReadError> {
    if bytes.len() > limits.input_bytes {
        return Err(LimitError::Exceeded("input").into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ReadError::NotText)?;
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
        block::parse(&copy, dialect, m)?
    } else {
        block::parse(text, dialect, m)?
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
fn inlines(raw: &str, refs: &Refs, dialect: Dialect, m: &mut Meter) -> Vec<Inline> {
    inline::parse(raw, refs, dialect, m)
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

/// Highest ordered-list number written: CommonMark reads at most nine
/// digits as a list marker, so a larger number would not read back as a
/// list. Readers of other formats clamp to it too; no real book counts
/// that far.
pub(crate) const MAX_LIST_NUMBER: u64 = 999_999_999;

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
    out.push_str(xml::entity(scan::utf8(&s[j..j + n]))?);
    Some(j + n + 1)
}

/// Writes CommonMark. Text is escaped so that it can never be read as
/// structure: the characters `\ ` * _ [ ] < > & | ! #` always get a
/// backslash, line-start `- + = ~` and the `.`/`)` of a line-start `N.`/`N)`
/// likewise, and any whitespace character at the start or end of a line becomes a
/// numeric entity (`&#32;`, `&#9;`, `&#xA0;`, ...). A paragraph line made
/// only of `:` and dashes (`:--`, `:-:`) under another line gets `\:`: a
/// GFM reader would take it for a table's delimiter row.
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
    write_limited_with(doc, limits, Dialect::CommonMark)
}

/// [`write_limited`] for a `dialect`. `CommonMark` is [`write_limited`]
/// byte for byte. `Gfm` writes the two GitHub extensions the CommonMark
/// mode has to flatten, and everything else the same way:
/// - A table is a pipe table: a header row (kept when every header cell is
///   empty, since a GFM table needs one), the delimiter row (`---`, `:--`,
///   `:-:`, `--:` per column alignment), then the body rows. A row with
///   fewer cells than the widest one is padded with empty cells, so no
///   cell is dropped. A `|` anywhere in a cell (code spans, link targets
///   and raw HTML included) is written `\|`, which a GFM reader takes off
///   before it reads the cell's inlines. A cell is one line: a line break
///   in it becomes a space, and so does a line ending inside its code,
///   HTML or link title, reported as [`Loss::CellsFlattened`] only when
///   that happened. A table with no column writes nothing. In a tight
///   list item a table follows a paragraph after one newline (its header
///   row ends the paragraph), so the list stays tight; a block after a
///   table needs a blank line (a paragraph line would be a body row), so
///   an item with one reads back loose.
/// - Strikethrough is `~~...~~`. Adjacent strikethroughs are written as
///   one (`~~~~` would be no delimiter), one inside another as the outer
///   one, and one with nothing to show is left out; a tilde of the text
///   next to a delimiter is escaped, so it cannot lengthen the run.
/// - Emphasis, strong emphasis and strikethrough are written so that a
///   GFM reader pairs the delimiters as written, or not at all: white
///   space at the edges of the content goes outside the delimiters; a
///   delimiter that would touch a run of its own character (a mark next
///   to it, or at the edge of an enclosing one) takes the other emphasis
///   character (`_` for `*`); and where the opener still could not open
///   or the closer close by the flanking rules (punctuation inside next
///   to a letter outside, `a<del>"b"</del>c`), or the opener could close
///   an enclosing mark of the same delimiter, the mark is left out and
///   [`Loss::Strikethrough`] or [`Loss::Emphasis`] reported, its content
///   written as it is. The test is made before the mark is written, on
///   the first and last characters its content will put down, and again
///   on what it did (a backstop). Hard breaks that end a paragraph are
///   not written, inside a mark too.
/// - A `:` at the start of a line is escaped too: under a paragraph line
///   it could start a delimiter row (`:--`) and turn the paragraph into a
///   table.
/// - A `www.` or `://` in text that a GFM reader would make a link holding
///   one of the writer's escapes gets an entity for its `.` or `:` (see
///   `autolink_guards`); so does the `www.` of an email autolink's text.
pub fn write_limited_with(
    doc: &Document,
    limits: Limits,
    dialect: Dialect,
) -> Result<(String, BTreeSet<Loss>), LimitError> {
    write_part(doc, limits, dialect, None).map(|(out, losses, _)| (out, losses))
}

/// The list a written part ends with at its top level: its kind and the
/// marker it took. [`write_part`] gives it for one part and takes it for
/// the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListEnd {
    ordered: bool,
    alt: bool,
}

/// [`write_limited_with`] for one part of a longer Markdown file (a
/// book's chapter), written after a part that ended with the list `after`
/// (what this returned for it) and a blank line: a first list of the same
/// kind takes the other marker (`-` and `*`, `.` and `)`), as a list after
/// a list in one document does, so a reader does not join it to the list
/// above (an ordered one keeps its own start). Returns what this part
/// ends with too: a list, or `None` for any other block. An empty part
/// ends as the part before it did.
pub fn write_part(
    doc: &Document,
    limits: Limits,
    dialect: Dialect,
    after: Option<ListEnd>,
) -> Result<(String, BTreeSet<Loss>, Option<ListEnd>), LimitError> {
    let mut losses = BTreeSet::new();
    let mut sink = Sink::new(limits);
    let start = after.map(|l| (l.ordered, l.alt));
    let (mut out, end) = blocks_after(&doc.blocks, true, dialect, &mut losses, &mut sink, start);
    if !out.is_empty() {
        sink.push(&mut out, '\n');
    }
    sink.result()?;
    let end = end.map(|(ordered, alt)| ListEnd { ordered, alt });
    Ok((out, losses, end))
}

fn blocks_to_string(
    blocks: &[Block],
    loose: bool,
    dialect: Dialect,
    losses: &mut BTreeSet<Loss>,
    sink: &mut Sink,
) -> String {
    blocks_after(blocks, loose, dialect, losses, sink, None).0
}

/// [`blocks_to_string`] directly after the list `prev_list` (ordered,
/// alternate delimiter), and the list it ends with, if it does.
fn blocks_after(
    blocks: &[Block],
    loose: bool,
    dialect: Dialect,
    losses: &mut BTreeSet<Loss>,
    sink: &mut Sink,
    mut prev_list: Option<(bool, bool)>,
) -> (String, Option<(bool, bool)>) {
    let mut out = String::new();
    let mut prev: Option<&Block> = None;
    for b in blocks {
        if sink.failed() {
            break;
        }
        let alt = match (b, prev_list) {
            (Block::List { start, .. }, Some((ordered, alt))) if ordered == start.is_some() => !alt,
            _ => false,
        };
        let s = block_to_string(b, alt, dialect, losses, sink);
        if s.is_empty() {
            continue;
        }
        if let Some(p) = prev {
            let sep = if loose || needs_blank(p, b, dialect) {
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
    (out, prev_list)
}

/// Whether a single newline between two blocks (tight list item) could
/// change the meaning: lazy continuation, setext underline, HTML block
/// continuation, or a list that cannot interrupt a paragraph.
fn needs_blank(prev: &Block, next: &Block, dialect: Dialect) -> bool {
    match prev {
        Block::List { .. } | Block::BlockQuote(_) | Block::HtmlBlock(_) => true,
        // A CommonMark-mode table is written as paragraph text, so it
        // follows the same rules. A pipe table goes on with every line
        // that is not a blank one or another block's start, so a
        // paragraph or table after it needs the blank line too; its own
        // header row ends a paragraph above it, so it needs none there.
        Block::Paragraph(_) | Block::Table { .. } => match next {
            Block::Table { .. } => {
                !(dialect == Dialect::Gfm && matches!(prev, Block::Paragraph(_)))
            }
            Block::Paragraph(_) | Block::HtmlBlock(_) | Block::ThematicBreak => true,
            Block::List { start, items, .. } => {
                !(matches!(start, None | Some(1))
                    && items.first().is_some_and(|i| !i.blocks.is_empty()))
            }
            _ => false,
        },
        _ => matches!(next, Block::HtmlBlock(_)),
    }
}

fn block_to_string(
    b: &Block,
    alt: bool,
    dialect: Dialect,
    losses: &mut BTreeSet<Loss>,
    sink: &mut Sink,
) -> String {
    let gfm = dialect == Dialect::Gfm;
    match b {
        Block::Paragraph(c) => {
            let mut w = InlineWriter::new(true, false, losses, sink);
            w.gfm = gfm;
            w.inlines(c, true);
            w.out
        }
        Block::Heading { level, content } => {
            let mut w = InlineWriter::new(false, true, losses, sink);
            w.gfm = gfm;
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
            let body = blocks_to_string(inner, true, dialect, losses, sink);
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
                        n.saturating_add(i as u64).min(MAX_LIST_NUMBER),
                        if alt { ')' } else { '.' }
                    ),
                };
                let mut body = blocks_to_string(&item.blocks, !*tight, dialect, losses, sink);
                if let Some(done) = item.checked {
                    // The GFM task marker; plain text never starts with an
                    // unescaped `[`, so it reads back as a marker.
                    let mark = if done { "[x]" } else { "[ ]" };
                    let mut marked = String::new();
                    sink.push_str(&mut marked, mark);
                    if !body.is_empty() {
                        sink.push(&mut marked, ' ');
                        sink.push_str(&mut marked, &body);
                    }
                    body = marked;
                }
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
        Block::Table { align, head, rows } if gfm => pipe_table(align, head, rows, losses, sink),
        Block::Table { head, rows, .. } => {
            // CommonMark mode: flattened to one line per row, cells
            // separated by a space. The loss copy ("table too wide; rows
            // listed one by one") speaks of width, which does not apply
            // here: this mode flattens every table whatever its width.
            // Accepted: its shipped path (txt -> md) builds no tables; a
            // tree with tables goes through the GFM mode.
            losses.insert(Loss::TableLayout);
            let mut w = InlineWriter::new(true, false, losses, sink);
            // A row with no content would be a blank line and split the
            // block, so it is left out.
            let all: Vec<&Vec<Vec<Inline>>> = std::iter::once(head)
                .chain(rows.iter())
                .filter(|r| r.iter().any(|c| !c.is_empty()))
                .collect();
            let n = all.len();
            for (i, row) in all.into_iter().enumerate() {
                for (j, cell) in row.iter().enumerate() {
                    if j > 0 {
                        w.sink.push(&mut w.out, ' ');
                    }
                    w.inlines(cell, i + 1 == n && j + 1 == row.len());
                }
                if i + 1 < n {
                    w.line_break("\n");
                }
            }
            w.out
        }
        Block::ThematicBreak => "---".to_string(),
        Block::HtmlBlock(h) => h.trim_end_matches('\n').to_string(),
    }
}

/// A GFM pipe table (see [`write_limited_with`]). Every row is a line
/// that starts with `|`, which no block start does, so a row cannot be
/// read as anything else; the cells are written apart and copied once, so
/// the time is linear in the output.
fn pipe_table(
    align: &[Align],
    head: &[Vec<Inline>],
    rows: &[Vec<Vec<Inline>>],
    losses: &mut BTreeSet<Loss>,
    sink: &mut Sink,
) -> String {
    let cols = rows
        .iter()
        .map(Vec::len)
        .chain([head.len(), align.len()])
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    if cols == 0 {
        return out;
    }
    let mut flattened = false;
    let mut row = |cells: &[Vec<Inline>], out: &mut String, sink: &mut Sink| {
        sink.push(out, '|');
        for j in 0..cols {
            if sink.failed() {
                return;
            }
            sink.push(out, ' ');
            if let Some(cell) = cells.get(j) {
                let mut w = InlineWriter::new(false, true, losses, sink);
                w.gfm = true;
                w.cell = true;
                w.inlines(cell, true);
                let (text, flat) = (w.out, w.flattened);
                flattened |= flat;
                // A pipe would end the cell, a line ending the row: every
                // `|` gets its backslash (`unescape_pipes` of the reader
                // takes exactly one off), any line ending left (raw HTML,
                // code, a title) is a space.
                let mut rest = text.as_str();
                while let Some(k) = rest.find(['|', '\n', '\r']) {
                    sink.push_str(out, &rest[..k]);
                    if rest.as_bytes()[k] == b'|' {
                        sink.push_str(out, "\\|");
                    } else {
                        flattened = true;
                        sink.push(out, ' ');
                    }
                    rest = &rest[k + 1..];
                }
                sink.push_str(out, rest);
            }
            sink.push_str(out, " |");
        }
    };
    row(head, &mut out, sink);
    sink.push(&mut out, '\n');
    sink.push(&mut out, '|');
    for j in 0..cols {
        sink.push_str(
            &mut out,
            match align.get(j) {
                Some(Align::Left) => " :-- |",
                Some(Align::Center) => " :-: |",
                Some(Align::Right) => " --: |",
                Some(Align::None) | None => " --- |",
            },
        );
    }
    for r in rows {
        if sink.failed() {
            break;
        }
        sink.push(&mut out, '\n');
        row(r, &mut out, sink);
    }
    if flattened {
        losses.insert(Loss::CellsFlattened);
    }
    out
}

/// Fenced code block. The fence is longer than any run of the fence
/// character in the text; an info string containing a backtick forces a
/// tilde fence (a backtick fence's info string may not contain backticks).
/// The reader takes backslash escapes and entities off an info string, so
/// every `\` and `&` in it gets a backslash and reads back as it was.
fn code_block(info: &str, text: &str, sink: &mut Sink) -> String {
    let info = info.replace(['\n', '\r'], " ");
    let info = info.trim();
    let mut escaped = String::new();
    for c in info.chars() {
        if matches!(c, '\\' | '&') {
            sink.push(&mut escaped, '\\');
        }
        sink.push(&mut escaped, c);
    }
    if sink.failed() {
        return String::new();
    }
    let info = escaped.as_str();
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
    /// The delimiter of the `Emph` the inlines being written are inside
    /// (at any depth), so a nested one at its edge can take the other.
    in_emph: Option<&'static str>,
    /// GFM mode: strikethrough is written, a line-start `:` escaped.
    gfm: bool,
    /// Inside a GFM table cell: `|` is left to the table writer, which
    /// escapes every one of them.
    cell: bool,
    /// A line break was written as a space (`single_line`).
    flattened: bool,
    /// Inside a written `~~...~~`: a nested strikethrough adds nothing.
    in_strike: bool,
    /// The list being written is directly followed by a `~~` delimiter,
    /// so a tilde ending its last text must be escaped. Taken (reset) by
    /// every [`InlineWriter::inlines`] call as it starts.
    tilde_follows: bool,
    /// GFM: what is written right after the list being written ends (at
    /// worst), for the flanking test of a closer at its end.
    follow: Class,
    /// GFM: where the content of the innermost open mark starts in `out`,
    /// and what precedes that mark. A mark opened right there may lose
    /// that opener (left out after its content), so it takes what
    /// precedes the opener as its own neighbour.
    open_at: usize,
    open_before: Class,
    /// GFM: the delimiter of every mark open around what is being
    /// written, innermost last, and how far its content from its
    /// `open_at` is known to be edge white space only (`usize::MAX`: it
    /// is not), so the check costs each byte once.
    open_marks: Vec<(&'static str, usize)>,
    /// GFM: the end of the list being written is the end of the
    /// innermost open mark's content (edge white space aside).
    at_end: bool,
}

/// What a delimiter run's neighbour is for the flanking rules: Unicode
/// white space (a line edge too), Unicode punctuation, or anything else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Space,
    Punct,
    Other,
}

impl Class {
    fn of(c: char) -> Class {
        if inline::is_space(c) {
            Class::Space
        } else if inline::is_punct(c) {
            Class::Punct
        } else {
            Class::Other
        }
    }

    /// A mark's edge that is `self` if the mark is left out and its
    /// delimiter (punctuation) if not: the worse of the two for a
    /// delimiter next to it (white space, then punctuation).
    fn or_delimiter(self) -> Class {
        if self == Class::Space {
            Class::Space
        } else {
            Class::Punct
        }
    }

    /// What follows the content of a mark followed by `self`: its closer
    /// (punctuation) if it is written, `self` if it is left out.
    fn behind_closer(self) -> Class {
        if self == Class::Other {
            Class::Other
        } else {
            Class::Punct
        }
    }
}

/// Whether a run of `ch` between `before` and `first` can open (the
/// reader's flanking rules; `~` follows `*`).
fn can_open(ch: u8, before: Class, first: Class) -> bool {
    let left = first != Class::Space && (first != Class::Punct || before != Class::Other);
    let right = before != Class::Space && (before != Class::Punct || first != Class::Other);
    if ch == b'_' {
        left && (!right || before == Class::Punct)
    } else {
        left
    }
}

/// Whether a run of `ch` between `last` and `after` can close.
fn can_close(ch: u8, last: Class, after: Class) -> bool {
    let right = last != Class::Space && (last != Class::Punct || after != Class::Other);
    let left = after != Class::Space && (after != Class::Punct || last != Class::Other);
    if ch == b'_' {
        right && (!left || after == Class::Punct)
    } else {
        right
    }
}

/// The class of the first character the GFM writer puts down for
/// `items` (`None`: they write nothing), at worst: a mark whose content
/// starts with a letter may be left out, so it counts as that letter.
/// Stops at the first item that writes something, so a walk over a list
/// costs the items it passes, each down to its first written character.
/// A list's items look ahead through [`Ahead`], not by calling this on
/// the rest of the list per item.
fn lead(items: &[Inline]) -> Option<Class> {
    for i in items {
        let c = match i {
            // A white space character is written as it is or as an
            // entity (`&`): both are no letter.
            Inline::Text(t) => t.chars().next().map(Class::of),
            Inline::Html(h) => h.chars().next().map(Class::of),
            Inline::SoftBreak => Some(Class::Space),
            // `\` before the line ending, or nothing at a line end.
            Inline::HardBreak => Some(Class::Punct),
            Inline::Code(t) => (!t.is_empty()).then_some(Class::Punct),
            Inline::Image { .. } => Some(Class::Punct),
            Inline::Link {
                auto: true,
                content,
                ..
            } => lead(content),
            Inline::Link { .. } => Some(Class::Punct),
            Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => match lead(c) {
                // Edge white space goes in front of the mark.
                Some(Class::Space) => Some(Class::Space),
                Some(Class::Other) => Some(Class::Other),
                Some(Class::Punct) => Some(Class::Punct),
                None => None,
            },
        };
        if c.is_some() {
            return c;
        }
    }
    None
}

/// Whether `items` write nothing but hard breaks (`gfm`: marks holding
/// only those too, as the GFM mode writes no mark around nothing).
fn breaks_only(items: &[Inline], gfm: bool) -> bool {
    items.iter().all(|i| match i {
        Inline::HardBreak => true,
        Inline::Text(t) | Inline::Code(t) | Inline::Html(t) => t.is_empty(),
        Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => {
            gfm && breaks_only(c, gfm)
        }
        _ => false,
    })
}

/// Whether `items` write nothing but edge white space (which a mark
/// around them moves out) or hard breaks (which one at a line end takes
/// off), so what precedes them may end where they do.
fn ends_here(items: &[Inline]) -> bool {
    items.iter().all(|i| match i {
        Inline::HardBreak => true,
        Inline::Text(t) => t.chars().all(edge_space),
        Inline::Code(t) | Inline::Html(t) => t.is_empty(),
        Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => ends_here(c),
        _ => false,
    })
}

/// A look ahead over a list that is read front to back: the first item
/// at or after an index where `stop` finds something, and what. A scan
/// goes on from where the last one stopped while the index has not
/// passed that item, so the whole list costs each item one `stop`
/// (which walks into that item's content); scanning the rest of the
/// list again per item would cost the square of a long run (hard breaks,
/// empty marks), minutes for one chapter.
struct Ahead<T> {
    /// Where the last scan started (`usize::MAX`: none yet), the item it
    /// stopped at (the list's length: none), and what `stop` found there.
    from: usize,
    at: usize,
    found: Option<T>,
}

impl<T: Copy> Ahead<T> {
    fn new() -> Self {
        Ahead {
            from: usize::MAX,
            at: 0,
            found: None,
        }
    }

    /// The first item of `items[from..]` for which `stop` gives
    /// something, as that (`None`: no such item). Indices passed in
    /// must not go down, or a call scans again.
    fn find(
        &mut self,
        items: &[Inline],
        from: usize,
        stop: impl Fn(&Inline) -> Option<T>,
    ) -> Option<T> {
        if !(self.from <= from && from <= self.at) {
            self.from = from;
            self.at = from;
            self.found = None;
            while let Some(i) = items.get(self.at) {
                self.found = stop(i);
                if self.found.is_some() {
                    break;
                }
                self.at += 1;
            }
        }
        self.found
    }
}

/// [`Ahead`] stops for [`breaks_only`] and [`ends_here`] (an item that
/// is not such), and for [`lead`] (an item that writes something).
fn not_break(gfm: bool) -> impl Fn(&Inline) -> Option<()> {
    move |i| (!breaks_only(std::slice::from_ref(i), gfm)).then_some(())
}
fn not_end(i: &Inline) -> Option<()> {
    (!ends_here(std::slice::from_ref(i))).then_some(())
}
fn item_lead(i: &Inline) -> Option<Class> {
    lead(std::slice::from_ref(i))
}

/// Whether `items` write nothing at all (the GFM mode writes no mark
/// around nothing).
fn writes_nothing(items: &[Inline]) -> bool {
    items.iter().all(|i| match i {
        Inline::Text(t) | Inline::Code(t) | Inline::Html(t) => t.is_empty(),
        Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => writes_nothing(c),
        _ => false,
    })
}

/// The first character the GFM writer puts down for `items` past edge
/// white space (`None`: there is none), at worst for an opener before
/// it: a mark inside counts as the worse of its delimiter (punctuation)
/// and its content's first character, as it may be left out. And whether edge white space
/// comes first (it is moved out in front of the mark). Walks to the
/// first character only: its cost is the items before it that write
/// nothing (edge white space, empty marks) and the nesting depth. It runs
/// once per mark, on that mark's content.
fn core_lead(items: &[Inline]) -> (Option<Class>, bool) {
    let mut ws = false;
    for i in items {
        let c = match i {
            Inline::Text(t) | Inline::Html(t) => {
                let r = t.trim_start_matches(edge_space);
                ws |= r.len() < t.len();
                r.chars().next().map(Class::of)
            }
            Inline::Link {
                auto: true,
                content,
                ..
            } => {
                let (c, w) = core_lead(content);
                ws |= w;
                c
            }
            Inline::SoftBreak => Some(Class::Space),
            Inline::HardBreak | Inline::Image { .. } | Inline::Link { .. } => Some(Class::Punct),
            Inline::Code(t) => (!t.is_empty()).then_some(Class::Punct),
            Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => {
                let (f, w) = core_lead(c);
                ws |= w;
                f.map(Class::or_delimiter)
            }
        };
        if c.is_some() {
            return (c, ws);
        }
    }
    (None, ws)
}

/// [`core_lead`] from the end, at worst for a closer after it. At a line
/// end (`eol`) hard breaks at the end are not written.
fn core_trail(items: &[Inline], eol: bool) -> (Option<Class>, bool) {
    let mut ws = false;
    for i in items.iter().rev() {
        let c = match i {
            Inline::Text(t) | Inline::Html(t) => {
                let r = t.trim_end_matches(edge_space);
                ws |= r.len() < t.len();
                r.chars().next_back().map(Class::of)
            }
            Inline::Link {
                auto: true,
                content,
                ..
            } => {
                let (c, w) = core_trail(content, false);
                ws |= w;
                c
            }
            Inline::HardBreak if eol => None,
            Inline::SoftBreak | Inline::HardBreak => Some(Class::Space),
            Inline::Image { .. } | Inline::Link { .. } => Some(Class::Punct),
            Inline::Code(t) => (!t.is_empty()).then_some(Class::Punct),
            Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => {
                let (f, w) = core_trail(c, false);
                ws |= w;
                f.map(Class::or_delimiter)
            }
        };
        if c.is_some() {
            return (c, ws);
        }
    }
    (None, ws)
}

/// Where an open mark (`Emph`, `Strong`, a run of `Strikethrough`) stands
/// in the output.
#[derive(Clone, Copy)]
struct Open {
    /// Where its opener starts.
    start: usize,
    /// Where its content starts.
    body: usize,
    /// The line state before the opener.
    line_start: bool,
    /// What precedes the opener, at worst for opening (see `open_at`),
    /// and as written now.
    before: Class,
    before_now: Class,
}

/// White space that goes outside a mark (a line ending stays in it).
fn edge_space(c: char) -> bool {
    c != '\n' && c != '\r' && inline::is_space(c)
}

/// The byte offset in `s` of a tilde that is text (one alone, unescaped)
/// at the start (`front`) or end of `s`; a `~~` there is a delimiter.
fn lone_tilde(s: &str, front: bool) -> Option<usize> {
    let b = s.as_bytes();
    if front {
        (b.first() == Some(&b'~') && b.get(1) != Some(&b'~')).then_some(0)
    } else {
        let n = b.len().checked_sub(1)?;
        let before = &b[..n];
        let slashes = before.iter().rev().take_while(|&&c| c == b'\\').count();
        (b[n] == b'~' && before.last() != Some(&b'~') && slashes % 2 == 0).then_some(n)
    }
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
            in_emph: None,
            gfm: false,
            cell: false,
            flattened: false,
            in_strike: false,
            tilde_follows: false,
            follow: Class::Space,
            open_at: usize::MAX,
            open_before: Class::Space,
            open_marks: Vec::new(),
            at_end: false,
        }
    }

    /// Whether `out[..at]` ends with an unescaped `ch`.
    fn ends_with_delim(&self, ch: u8, at: usize) -> bool {
        let b = &self.out.as_bytes()[..at];
        let Some((&last, rest)) = b.split_last() else {
            return false;
        };
        last == ch && rest.iter().rev().take_while(|&&c| c == b'\\').count() % 2 == 0
    }

    /// Whether nothing but edge white space was written since the
    /// innermost open mark's opener (so a mark opened now would end up
    /// right after it).
    fn at_mark_start(&mut self) -> bool {
        let at = self.open_at;
        let len = self.out.len();
        let Some((_, seen)) = self.open_marks.last_mut() else {
            return false;
        };
        if *seen == usize::MAX {
            return false;
        }
        let from = (*seen).max(at);
        *seen = if self.out[from..].chars().all(edge_space) {
            len
        } else {
            usize::MAX
        };
        *seen != usize::MAX
    }

    /// A GFM `Emph` (`*`) or `Strong` (`**`, `two`). Two runs of the
    /// same delimiter character side by side are one run to a reader,
    /// which pairs it otherwise (`*a**b*` is no emphasis): where the
    /// opener would follow such a character, or this mark would start or
    /// end its enclosing mark of that character (`***x**y*`), the other
    /// emphasis character (`_`) is used; where both would, or where
    /// [`InlineWriter::decide`] finds that it cannot open or close, the
    /// mark is left out: its content is written as it is, with `loss`.
    /// `at_end`: it ends where its enclosing mark does (the items after
    /// it only [`ends_here`]); `follow` and `eol` as for
    /// [`InlineWriter::close`].
    fn gfm_mark(&mut self, two: bool, c: &[Inline], at_end: bool, follow: Class, eol: bool) {
        let at_start = self.at_mark_start();
        let parent = self.open_marks.last().map(|&(d, _)| d.as_bytes()[0]);
        let len = self.out.len();
        let free = |w: &Self, ch: u8| {
            !w.ends_with_delim(ch, len) && !((at_start || at_end) && parent == Some(ch))
        };
        match b"*_".iter().copied().find(|&ch| free(self, ch)) {
            Some(ch) => {
                let delim = match (two, ch) {
                    (true, b'*') => "**",
                    (true, _) => "__",
                    (false, b'*') => "*",
                    (false, _) => "_",
                };
                if self.decide(delim, core_lead(c), core_trail(c, eol), follow, at_start) {
                    self.mark(delim, c, follow, eol);
                } else {
                    self.unmarked(c, at_end, follow, eol, Loss::Emphasis);
                }
            }
            _ => self.unmarked(c, at_end, follow, eol, Loss::Emphasis),
        }
    }

    /// Whether a mark `delim` can be written around content whose first
    /// and last characters past edge white space are `lead` and `trail`
    /// (with whether edge white space is there, see [`core_lead`]), with
    /// `follow` after it: its opener must open and its closer close by
    /// the flanking rules, where edge white space moved out is a neighbour
    /// too. Inside a mark of the same delimiter, an opener that could also
    /// close would close that one (a reader pairs a closer with the
    /// nearest opener it may), so it must not; at the start of that mark
    /// (`at_start`) the opener ends up next to its delimiter. A mark of
    /// the same character but the other length does not count: the
    /// reader's rule of 3 keeps a run of 1 and one of 2 that can both
    /// open and close from pairing (`*a**b**c*`).
    fn decide(
        &self,
        delim: &str,
        lead: (Option<Class>, bool),
        trail: (Option<Class>, bool),
        follow: Class,
        at_start: bool,
    ) -> bool {
        let ((Some(first), lead_ws), (Some(last), trail_ws)) = (lead, trail) else {
            return false;
        };
        let ch = delim.as_bytes()[0];
        // White space moved out to a line start is an entity (`&#32;`).
        let before = match (lead_ws, self.line_start) {
            (true, true) => Class::Punct,
            (true, false) => Class::Space,
            (false, _) => self.before(),
        };
        let after = if trail_ws { Class::Space } else { follow };
        let nested = self.open_marks.iter().any(|&(d, _)| d == delim);
        let closes_outer =
            can_close(ch, before, first) || (at_start && can_close(ch, Class::Punct, first));
        can_open(ch, before, first) && can_close(ch, last, after) && !(nested && closes_outer)
    }

    /// The content of a mark that is left out, written as it is (`loss`
    /// reported if it shows anything). `at_end` as for
    /// [`InlineWriter::gfm_mark`].
    fn unmarked(&mut self, c: &[Inline], at_end: bool, follow: Class, eol: bool, loss: Loss) {
        if !writes_nothing(c) {
            self.losses.insert(loss);
        }
        let list_follow = std::mem::replace(&mut self.follow, follow);
        let at_end = std::mem::replace(&mut self.at_end, at_end);
        self.inlines(c, eol);
        self.follow = list_follow;
        self.at_end = at_end;
    }

    /// Whether the current line of `out` is 1-9 ASCII digits so far (an
    /// ordered list marker's number). Looks back 10 bytes at most.
    fn after_line_digits(&self) -> bool {
        let b = self.out.as_bytes();
        let n = b
            .iter()
            .rev()
            .take(10)
            .take_while(|c| c.is_ascii_digit())
            .count();
        (1..=9).contains(&n) && (b.len() == n || b[b.len() - n - 1] == b'\n')
    }

    /// What precedes the next character written (see `open_at`).
    fn before(&self) -> Class {
        if self.out.len() == self.open_at {
            return self.open_before;
        }
        self.out.chars().next_back().map_or(Class::Space, Class::of)
    }

    /// Opens a GFM mark: its opener `delim` is written, and its content
    /// (written next) sees it as its `open_at`. Returns the mark and the
    /// state to give back to [`InlineWriter::close`].
    fn open(&mut self, delim: &'static str) -> (Open, (usize, Class)) {
        let mut open = Open {
            start: self.out.len(),
            body: 0,
            line_start: self.line_start,
            before: self.before(),
            before_now: self.out.chars().next_back().map_or(Class::Space, Class::of),
        };
        self.line_start = false;
        self.sink.push_str(&mut self.out, delim);
        open.body = self.out.len();
        let outer = (self.open_at, self.open_before);
        (self.open_at, self.open_before) = (open.body, open.before);
        self.open_marks.push((delim, open.body));
        (open, outer)
    }

    /// Closes a GFM mark opened by [`InlineWriter::open`] once its content
    /// is written: `follow` is what comes after it (at worst), `eol`
    /// whether that is the end of the line. White space at the edges of
    /// the content goes outside the delimiters (an entity at a line edge).
    /// Then, if the opener cannot open or the closer cannot close by the
    /// flanking rules, the marks are left out and `loss` reported; what
    /// they kept apart is escaped where it would now mean something (a
    /// list marker at the line start, a lone tilde next to another).
    /// Content that writes nothing takes its opener back. Copies the
    /// content only when it changes something, so a mark costs its
    /// content once per enclosing mark at most (the readers bound the
    /// nesting depth).
    fn close(
        &mut self,
        open: Open,
        outer: (usize, Class),
        delim: &str,
        follow: Class,
        eol: bool,
        loss: Loss,
    ) {
        (self.open_at, self.open_before) = outer;
        self.open_marks.pop();
        if self.sink.failed() {
            return;
        }
        // A hard break at the end of a line shows nothing (the paragraph
        // writer leaves it out), but its `\` would show once the break
        // is followed by nothing: at a line end, one ending the content
        // (edge white space before it aside) is taken off.
        if eol {
            loop {
                let body = &self.out[open.body..];
                let kept = body.trim_end_matches(edge_space);
                let Some(cut) = kept.strip_suffix("\\\n") else {
                    break;
                };
                if cut.bytes().rev().take_while(|&c| c == b'\\').count() % 2 == 1 {
                    break;
                }
                let at = open.body + cut.len();
                self.out.replace_range(at..at + 2, "");
            }
        }
        let content = &self.out[open.body..];
        if content.is_empty() {
            self.out.truncate(open.start);
            self.line_start = open.line_start;
            return;
        }
        let core_start = content.len() - content.trim_start_matches(edge_space).len();
        let core_end = content.trim_end_matches(edge_space).len().max(core_start);
        let core = &content[core_start..core_end];
        let before = if core_start > 0 {
            Class::Space
        } else {
            open.before
        };
        let after = if core_end < content.len() {
            Class::Space
        } else {
            follow
        };
        let ch = delim.as_bytes()[0];
        let (first, last) = (core.chars().next(), core.chars().next_back());
        // The backstop of `decide` on what was written (see there).
        let nested = self.open_marks.iter().any(|&(d, _)| d == delim);
        let now = if core_start > 0 {
            Class::Space
        } else {
            open.before_now
        };
        let keep = match (first, last) {
            (Some(f), Some(l)) => {
                let f = Class::of(f);
                can_open(ch, before, f)
                    && can_close(ch, Class::of(l), after)
                    && !(nested && (can_close(ch, before, f) || can_close(ch, now, f)))
            }
            _ => false,
        };
        let tilde_edge = delim == "~~"
            && (lone_tilde(core, true).is_some() || lone_tilde(core, false).is_some());
        if keep && core_start == 0 && core_end == content.len() && !tilde_edge {
            self.sink.push_str(&mut self.out, delim);
            return;
        }
        if !keep {
            self.losses.insert(loss);
        }
        let tail = self.out.split_off(open.start);
        let body = &tail[open.body - open.start..];
        let (lead, core, trail) = (
            &body[..core_start],
            &body[core_start..core_end],
            &body[core_end..],
        );
        self.spaces(lead, open.line_start || (core.is_empty() && eol));
        if keep {
            self.sink.push_str(&mut self.out, delim);
        }
        let mut from = 0;
        if !keep && open.line_start && lead.is_empty() {
            from = self.line_start_escape(core);
        }
        let front = lone_tilde(&core[from..], true).map(|k| k + from);
        let back = lone_tilde(core, false).filter(|&k| k >= from && Some(k) != front);
        for k in [front, back].into_iter().flatten() {
            self.sink.push_str(&mut self.out, &core[from..k]);
            self.sink.push(&mut self.out, '\\');
            from = k;
        }
        self.sink.push_str(&mut self.out, &core[from..]);
        if keep {
            self.sink.push_str(&mut self.out, delim);
        }
        self.spaces(trail, eol);
        // A line break ending what was written starts a new line.
        self.line_start = self.out.ends_with('\n') || (open.line_start && core.is_empty());
    }

    /// Writes `core`'s start as the text writer would at a line start:
    /// a `- + = :` gets a backslash, so does the `.`/`)` after up to nine
    /// digits (a lone `~` is escaped by the caller, a `~~` there is a
    /// delimiter). Returns how much of `core` it wrote.
    fn line_start_escape(&mut self, core: &str) -> usize {
        let digits = core.bytes().take_while(u8::is_ascii_digit).count();
        match core.as_bytes().get(digits) {
            Some(b'.' | b')') if (1..=9).contains(&digits) => {
                self.sink.push_str(&mut self.out, &core[..digits]);
                self.sink.push(&mut self.out, '\\');
                digits
            }
            Some(b'-' | b'+' | b'=' | b':') if digits == 0 => {
                self.sink.push(&mut self.out, '\\');
                0
            }
            _ => 0,
        }
    }

    /// White space moved out of a mark: as it is, or as entities at a
    /// line edge (where rendering would strip it).
    fn spaces(&mut self, ws: &str, entity: bool) {
        if !entity {
            self.sink.push_str(&mut self.out, ws);
            return;
        }
        for ch in ws.chars() {
            match ch {
                ' ' => self.sink.push_str(&mut self.out, "&#32;"),
                '\t' => self.sink.push_str(&mut self.out, "&#9;"),
                _ => self
                    .sink
                    .push_str(&mut self.out, &format!("&#x{:X};", ch as u32)),
            }
        }
    }

    /// A link's text or an image's alt text between its brackets: a scope
    /// of its own for emphasis (a reader pairs no delimiter inside with
    /// one outside), followed by `]`.
    fn link_text(&mut self, c: &[Inline]) {
        let saved = (
            std::mem::replace(&mut self.follow, Class::Punct),
            std::mem::take(&mut self.open_marks),
            std::mem::replace(&mut self.open_at, usize::MAX),
            std::mem::replace(&mut self.at_end, false),
        );
        self.inlines(c, false);
        (self.follow, self.open_marks, self.open_at, self.at_end) = saved;
    }

    /// A GFM `Emph` or `Strong` that [`InlineWriter::decide`] keeps (see
    /// [`InlineWriter::close`]).
    fn mark(&mut self, delim: &'static str, c: &[Inline], follow: Class, eol: bool) {
        let (open, outer) = self.open(delim);
        let list_follow = std::mem::replace(&mut self.follow, follow.behind_closer());
        let at_end = std::mem::replace(&mut self.at_end, true);
        self.tilde_follows = false;
        self.inlines(c, false);
        self.follow = list_follow;
        self.at_end = at_end;
        self.close(open, outer, delim, follow, eol, Loss::Emphasis);
    }

    /// `end_of_line`: the end of `items` is the end of a rendered line.
    fn inlines(&mut self, items: &[Inline], end_of_line: bool) {
        let tilde_after_list = std::mem::take(&mut self.tilde_follows);
        let list_follow = self.follow;
        // The `~~` opening the current run of strikethroughs, and the
        // marks open around it.
        let mut strike_open = (
            Open {
                start: 0,
                body: 0,
                line_start: false,
                before: Class::Space,
                before_now: Class::Space,
            },
            (usize::MAX, Class::Space),
        );
        // Whether the current run of strikethroughs is written as one.
        let mut strike_keep = false;
        // What follows an item, and what follows a run of strikethroughs:
        // each looked up from an index that only goes up.
        let gfm = self.gfm;
        let mut breaks_ahead = Ahead::new();
        let mut ends_ahead = Ahead::new();
        let mut lead_ahead = Ahead::new();
        let mut run_breaks = Ahead::new();
        let mut run_lead = Ahead::new();
        // GFM: adjacent texts are written as one, as every reader reads
        // them back: the autolink guards look at one text, and split, a
        // `www.` run reaching a text's end is guarded (more follows)
        // where the same run read back as one text is not, so the link
        // would appear a round later. The texts before the last of a run
        // gather here; one buffer per list, so it holds no more than the
        // list's own text.
        let mut joined = String::new();
        for (idx, item) in items.iter().enumerate() {
            if self.sink.failed() {
                return;
            }
            let next = items.get(idx + 1);
            let before = idx.checked_sub(1).and_then(|i| items.get(i));
            let strike = |i: Option<&Inline>| matches!(i, Some(Inline::Strikethrough(_)));
            // What comes after this item, for a GFM mark's closer, and
            // whether that ends the line.
            let mut follow = || {
                lead_ahead
                    .find(items, idx + 1, item_lead)
                    .unwrap_or(list_follow)
            };
            // Whether only breaks follow it (in the GFM mode marks holding
            // only those too).
            let mut breaks_after = || breaks_ahead.find(items, idx + 1, not_break(gfm)).is_none();
            // Whether it ends where its enclosing GFM mark does.
            let at_end = self.at_end;
            let mut ends_after = || at_end && ends_ahead.find(items, idx + 1, not_end).is_none();
            // GFM: breaks after it that are not written (only breaks up to
            // the end of the line) leave it at the line end.
            let item_eol = match next {
                None => end_of_line,
                Some(Inline::SoftBreak) => true,
                Some(_) => gfm && end_of_line && breaks_after(),
            };
            match item {
                Inline::Text(t) if gfm && matches!(next, Some(Inline::Text(_))) => {
                    joined.push_str(t);
                }
                Inline::Text(t) => {
                    let t = if joined.is_empty() {
                        t.as_str()
                    } else {
                        joined.push_str(t);
                        joined.as_str()
                    };
                    let eol = item_eol;
                    let tilde_after = self.gfm
                        && match next {
                            None => tilde_after_list,
                            Some(n) => strike(Some(n)) && !self.in_strike,
                        };
                    self.text(t, eol, tilde_after);
                    joined.clear();
                }
                Inline::Emph(c) | Inline::Strong(c) if self.gfm => {
                    // Written without its marks, its content's end is
                    // this list's end (a `~~` may follow).
                    self.tilde_follows = next.is_none() && tilde_after_list;
                    let two = matches!(item, Inline::Strong(_));
                    self.gfm_mark(two, c, ends_after(), follow(), item_eol);
                }
                Inline::Emph(c) => {
                    // An `Emph` at the edge of an `Emph` would put two
                    // `*` side by side (`**x**`), which reads back as
                    // `Strong`. `_` there is the other emphasis
                    // delimiter; it opens and closes next to `*` and
                    // punctuation, but not inside a word, so it is used
                    // only when the neighbouring text is not alphanumeric.
                    // An inner `Emph` glued to a word inside an outer one
                    // (`*x*b` as the first child) still cannot be written
                    // either way; that shape is left as it was.
                    let alnum = |i: Option<&Inline>, end: bool| match i {
                        Some(Inline::Text(t)) => {
                            let c = if end {
                                t.chars().next_back()
                            } else {
                                t.chars().next()
                            };
                            c.is_some_and(char::is_alphanumeric)
                        }
                        _ => false,
                    };
                    let edge = (before.is_none() && !alnum(next, false))
                        || (next.is_none() && !alnum(before, true));
                    let delim = match self.in_emph {
                        Some("*") if edge => "_",
                        _ => "*",
                    };
                    let outer = self.in_emph.replace(delim);
                    self.wrapped(delim, c);
                    self.in_emph = outer;
                }
                Inline::Strong(c) => self.wrapped("**", c),
                Inline::Strikethrough(c) if self.gfm && !self.in_strike => {
                    // A run of adjacent strikethroughs shares one pair of
                    // delimiters: it is one mark, decided as it starts. A
                    // `~` right before it would make a run of three.
                    if !strike(before) {
                        let n = items[idx..].iter().take_while(|&i| strike(Some(i))).count();
                        let (run, after_run) = items[idx..].split_at(n);
                        let run_eol = match after_run.first() {
                            None => end_of_line,
                            Some(Inline::SoftBreak) => true,
                            Some(_) => {
                                end_of_line
                                    && run_breaks.find(items, idx + n, not_break(true)).is_none()
                            }
                        };
                        let f = run_lead
                            .find(items, idx + n, item_lead)
                            .unwrap_or(list_follow);
                        let contents = || {
                            run.iter().filter_map(|i| match i {
                                Inline::Strikethrough(c) => Some(c.as_slice()),
                                _ => None,
                            })
                        };
                        let mut first = (None, false);
                        for c in contents() {
                            let (x, w) = core_lead(c);
                            first.1 |= w;
                            if x.is_some() {
                                first.0 = x;
                                break;
                            }
                        }
                        let mut last = (None, false);
                        for (k, c) in contents().rev().enumerate() {
                            let (x, w) = core_trail(c, k == 0 && run_eol);
                            last.1 |= w;
                            if x.is_some() {
                                last.0 = x;
                                break;
                            }
                        }
                        let at_start = self.at_mark_start();
                        strike_keep = !self.ends_with_delim(b'~', self.out.len())
                            && self.decide("~~", first, last, f, at_start);
                        if strike_keep {
                            strike_open = self.open("~~");
                        }
                    }
                    if !strike_keep {
                        // Left out: each one's content as it is.
                        self.tilde_follows = next.is_none() && tilde_after_list;
                        self.unmarked(c, ends_after(), follow(), item_eol, Loss::Strikethrough);
                        continue;
                    }
                    let last = !strike(next);
                    self.in_strike = true;
                    self.tilde_follows = last;
                    // Within the run, the next one's content follows.
                    let f = follow();
                    let inner = if last { f.behind_closer() } else { f };
                    let list_follow = std::mem::replace(&mut self.follow, inner);
                    // Each content sees the run's opener as its `open_at`.
                    self.open_at = strike_open.0.body;
                    self.open_before = strike_open.0.before;
                    self.inlines(c, false);
                    self.follow = list_follow;
                    self.in_strike = false;
                    if last {
                        strike_keep = false;
                        let (open, outer) = strike_open;
                        self.close(open, outer, "~~", f, item_eol, Loss::Strikethrough);
                    }
                }
                // Inside a written one, a strikethrough is its content.
                Inline::Strikethrough(c) if self.gfm => {
                    self.tilde_follows = next.is_none() && tilde_after_list;
                    let list_follow = std::mem::replace(&mut self.follow, follow());
                    self.inlines(c, end_of_line && next.is_none());
                    self.follow = list_follow;
                }
                Inline::Strikethrough(c) => {
                    self.losses.insert(Loss::Strikethrough);
                    self.inlines(c, end_of_line && next.is_none());
                }
                Inline::Code(t) => self.code_span(t),
                // A GFM extended autolink is written as its text, as it
                // came and unescaped: a GFM reader links it again, where a
                // backslash (`www.a\_b.c`) would cut it.
                Inline::Link {
                    dest,
                    content,
                    auto: true,
                    ..
                } => {
                    self.line_start = false;
                    let email = dest.starts_with("mailto:");
                    for c in content {
                        match c {
                            Inline::Text(t) if email => self.email_text(t),
                            Inline::Text(t) => self.sink.push_str(&mut self.out, t),
                            other => self.inlines(std::slice::from_ref(other), false),
                        }
                    }
                }
                Inline::Link {
                    dest,
                    title,
                    content,
                    ..
                } => {
                    self.line_start = false;
                    self.sink.push(&mut self.out, '[');
                    self.link_text(content);
                    self.sink.push(&mut self.out, ']');
                    self.target(dest, title);
                    // A line break in the text left a line start behind.
                    self.line_start = false;
                }
                Inline::Image { dest, title, alt } => {
                    self.line_start = false;
                    self.sink.push_str(&mut self.out, "![");
                    self.link_text(alt);
                    self.sink.push(&mut self.out, ']');
                    self.target(dest, title);
                    self.line_start = false;
                }
                Inline::SoftBreak => self.line_break("\n"),
                Inline::HardBreak => {
                    // At the end of a line a break shows nothing, so it is
                    // not written; nor is one followed by nothing but such
                    // breaks (or what writes nothing), whose `\` would
                    // be the last thing on the paragraph's last line.
                    if !end_of_line || !breaks_after() {
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
            self.flattened = true;
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

    /// The text of an email autolink. A GFM reader finds addresses after
    /// emphasis, so an `_` that can open or close emphasis would pair with
    /// another (`_a@b.c x_@d.e`). Every `_` not between two ASCII
    /// alphanumerics (where it can do neither) is escaped; the reader
    /// takes the escape before it looks for the address, so the address
    /// comes back the same. In GFM mode the `.` of a `www.` in it is an
    /// entity: the reader would start a web link there (the address is
    /// found later, in the decoded text), which runs past the address.
    fn email_text(&mut self, t: &str) {
        let b = t.as_bytes();
        let alnum = |i: Option<usize>| {
            i.and_then(|i| b.get(i))
                .is_some_and(u8::is_ascii_alphanumeric)
        };
        let mut from = 0;
        for (i, &c) in b.iter().enumerate() {
            if c == b'_' && !(alnum(i.checked_sub(1)) && alnum(Some(i + 1))) {
                self.sink.push_str(&mut self.out, &t[from..i]);
                self.sink.push_str(&mut self.out, "\\_");
                from = i + 1;
            } else if self.gfm && c == b'.' && b[..i].ends_with(b"www") {
                self.sink.push_str(&mut self.out, &t[from..i]);
                self.sink.push_str(&mut self.out, "&#46;");
                from = i + 1;
            }
        }
        self.sink.push_str(&mut self.out, &t[from..]);
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

    /// `tilde_after`: a `~~` delimiter follows `t` directly, so a tilde
    /// ending it is escaped (GFM).
    fn text(&mut self, t: &str, end_of_line: bool, tilde_after: bool) {
        // Inside a mark, white space ending the text may be moved out past
        // the closer, so the run may go on after it.
        let guards = if self.gfm {
            autolink_guards(t, end_of_line, !self.open_marks.is_empty())
        } else {
            Vec::new()
        };
        let mut rest = t;
        while let Some(c) = rest.chars().next() {
            if self.sink.failed() {
                return;
            }
            if !guards.is_empty() && guards.binary_search(&(t.len() - rest.len())).is_ok() {
                self.sink
                    .push_str(&mut self.out, if c == '.' { "&#46;" } else { "&#58;" });
                self.line_start = false;
                rest = &rest[1..];
                continue;
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
                        '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '&' | '!' | '#'
                    ) || (c == '|' && !self.cell)
                        || (self.line_start
                            && self.markers
                            && (matches!(c, '-' | '+' | '=' | '~') || (self.gfm && c == ':')))
                        // Both modes: under another line, a line `:-+:?`
                        // is a one-column delimiter row for a GFM reader.
                        || (c == ':'
                            && self.markers
                            && self.out.ends_with('\n')
                            && delimiter_row(rest, end_of_line))
                        || (c == '~' && tilde_after && rest.len() == 1)
                        // After a line start of 1-9 digits written by an
                        // earlier node (a mark left out between them), a
                        // `.`/`)` would make it a list marker.
                        || (matches!(c, '.' | ')') && self.markers && self.after_line_digits())
                        // Two tildes make strikethrough for a GFM reader; every
                        // tilde of a run of two or more is escaped, so no pair
                        // is left whatever the reader's run rule. A lone tilde
                        // stays as it was.
                        || (c == '~' && (rest[1..].starts_with('~') || self.out.ends_with('~')));
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

/// Whether `rest` (from a `:`) reads `:-+:?` to the end of its line: a
/// GFM delimiter row of one column, which needs no pipe (cmark-gfm's
/// `table_start`), so under a paragraph line it would make that line a
/// table header. The line ends at a `\n` in `rest`, or at its end where
/// that ends the line (`end_of_line`). White space after the dashes would
/// be an entity there, which no delimiter row holds; `-` and `|` are
/// escaped at a line start already.
fn delimiter_row(rest: &str, end_of_line: bool) -> bool {
    let (line, ends) = match rest.find('\n') {
        Some(k) => (&rest[..k], true),
        None => (rest, end_of_line),
    };
    let dashes = line.get(1..).unwrap_or("");
    let dashes = dashes.strip_suffix(':').unwrap_or(dashes);
    ends && !dashes.is_empty() && dashes.bytes().all(|b| b == b'-')
}

/// GFM: where text `t` must not give a GFM reader an extended autolink.
/// The reader finds one at a `www.` or a `://` and runs it to white space
/// or `<`, the raw bytes, backslashes included; so where the writer
/// would escape something in that run, or the run reaches the end of `t`
/// and more of the line follows (the next inline's syntax would join it),
/// the link would come back holding the escape, one more each round.
/// There the `.` of the `www.` or the `:` of the `://` becomes an entity,
/// and no link starts. Elsewhere the text is a link exactly as it reads.
/// Over-approximates (any `www.`, a preceding character is not checked):
/// an entity too many is harmless. Returns the sorted byte offsets of
/// those `.`/`:`; one pass per white-space-delimited run, so linear.
fn autolink_guards(t: &str, end_of_line: bool, in_mark: bool) -> Vec<usize> {
    let b = t.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    // The last white space stretch found: from where, and the first
    // character after it; the runs move forward, so each byte is scanned
    // once (a stretch of n spaces is n runs).
    let mut stretch = (0, 0);
    while start < b.len() {
        let end = b[start..]
            .iter()
            .position(|&c| c.is_ascii_whitespace() || c == b'<' || c == 0x0b)
            .map_or(b.len(), |p| start + p);
        let run = &b[start..end];
        // What the writer escapes, `~` included (pairs), entity-encodes
        // (`&`), or reads as a line break.
        let last_escape = run.iter().rposition(|c| {
            matches!(
                c,
                b'\\' | b'`' | b'*' | b'_' | b'[' | b']' | b'>' | b'&' | b'|' | b'!' | b'#' | b'~'
            )
        });
        // A `<` ends the run, but its escape is a backslash inside it.
        // White space that ends the line (the run's own non-ASCII white
        // space included) is written as entities (`&#32;`, `&#xA0;`),
        // which are no white space to a reader: the run goes on. Inside a
        // mark (`in_mark`), white space ending the text may be moved out
        // to a line end.
        let line_ws = |c: char| c.is_whitespace() && c != '\n';
        let ws_from = start + t[start..end].trim_end_matches(line_ws).len();
        let ws = if (stretch.0..stretch.1).contains(&ws_from) {
            stretch.1
        } else {
            let e = t[ws_from..]
                .find(|c: char| !line_ws(c))
                .map_or(b.len(), |p| ws_from + p);
            stretch = (ws_from, e);
            e
        };
        let trailing = ws_from < ws
            && ((ws == b.len() && (end_of_line || in_mark)) || b.get(ws) == Some(&b'\n'));
        let joins_next = (end == b.len() && !end_of_line) || b.get(end) == Some(&b'<') || trailing;
        for k in 0..run.len() {
            let guard = if run[k..].starts_with(b"www.") {
                Some(k + 3)
            } else if run[k..].starts_with(b"://") {
                Some(k)
            } else {
                None
            };
            if let Some(g) = guard
                && (joins_next || last_escape.is_some_and(|e| e >= k))
            {
                out.push(start + g);
            }
        }
        start = end + 1;
    }
    out
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
    fn longest_entity_name_is_read() {
        // The longest WHATWG name has 31 characters; MAX_ENTITY_NAME must
        // let the scan reach it (the table's own test is in the xml crate).
        let name = "CounterClockwiseContourIntegral";
        assert_eq!(name.len(), 31);
        assert!(name.len() <= MAX_ENTITY_NAME);
        assert_eq!(unescape(&format!("&{name};")), "\u{2233}");
    }

    fn md(s: &str) -> String {
        write(&doc(vec![para(s)])).0
    }
    fn list(start: Option<u64>, tight: bool, items: Vec<Vec<Block>>) -> Block {
        Block::List {
            start,
            tight,
            items: crate::plain_items(items),
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
    fn existing_escapes_unchanged() {
        // A single tilde, and a tilde run not at the line start, were never
        // escaped in the middle of a line; only the pair is new.
        assert_eq!(md("~5 min"), "\\~5 min\n");
        assert_eq!(md("a ~ b"), "a ~ b\n");
        assert_eq!(md("a~b"), "a~b\n");
        assert_eq!(md("x ~5"), "x ~5\n");
    }

    #[test]
    fn tilde_pairs_escaped_inline() {
        assert_eq!(md("a ~~b~~ c"), "a \\~\\~b\\~\\~ c\n");
        // Every tilde of a longer run: no pair is left for a reader.
        assert_eq!(md("a ~~~ b"), "a \\~\\~\\~ b\n");
    }

    /// The escape looks at what was written, not at one text node: two
    /// tildes split over adjacent nodes still count as a run (the second is
    /// escaped, so the reader sees a literal tilde, never a pair), and a tilde
    /// separated by emphasis or a code span stays alone.
    #[test]
    fn tilde_runs_split_over_nodes_and_neighbours() {
        let inline = |v: Vec<Inline>| write(&doc(vec![Block::Paragraph(v)])).0;
        assert_eq!(inline(vec![t("~"), t("~")]), "\\~\\~\n");
        assert_eq!(inline(vec![t("a~"), t("~b")]), "a~\\~b\n");
        assert_eq!(inline(vec![t("~"), t("~"), t("~")]), "\\~\\~\\~\n");
        assert_eq!(inline(vec![t("x~"), Inline::Emph(vec![t("~")])]), "x~*~*\n");
        assert_eq!(inline(vec![Inline::Emph(vec![t("~")]), t("~")]), "*~*~\n");
        assert_eq!(
            inline(vec![t("~"), Inline::Code("~".into()), t("~")]),
            "\\~`~`~\n"
        );
        assert_eq!(inline(vec![Inline::Code("~~".into()), t("~")]), "`~~`~\n");
    }

    /// `Emph` inside `Emph` must not write `**x**` (that reads back as
    /// `Strong`): the inner one takes `_` at the edge and round-trips.
    #[test]
    fn nested_emphasis_round_trips() {
        let e = |v: Vec<Inline>| Inline::Emph(v);
        let cases = [
            vec![e(vec![e(vec![t("x")])])],
            vec![e(vec![t("a "), e(vec![t("x")])])],
            vec![e(vec![e(vec![t("x")]), t(" b")])],
            vec![e(vec![t("a "), e(vec![t("x")]), t(" b")])],
            vec![e(vec![e(vec![e(vec![t("x")])])])],
            vec![e(vec![Inline::Strong(vec![e(vec![t("x")])])])],
        ];
        for inl in cases {
            let d = doc(vec![Block::Paragraph(inl)]);
            let (out, losses) = write(&d);
            assert!(losses.is_empty());
            assert_eq!(read(out.as_bytes()).unwrap(), d, "{out:?}");
        }
        let (out, _) = write(&doc(vec![Block::Paragraph(vec![e(vec![e(vec![t("x")])])])]));
        assert_eq!(out, "*_x_*\n");
    }

    #[test]
    fn table_in_tight_item_is_separated_from_a_paragraph() {
        let table = Block::Table {
            align: vec![crate::Align::None],
            head: vec![vec![t("h")]],
            rows: vec![vec![vec![t("c")]]],
        };
        let item = list(None, true, vec![vec![para("p"), table.clone(), para("q")]]);
        let (out, _) = write(&doc(vec![item]));
        assert_eq!(out, "- p\n\n  h\n  c\n\n  q\n");
    }

    #[test]
    fn all_empty_table_row_does_not_split_the_block() {
        let table = Block::Table {
            align: vec![crate::Align::None],
            head: vec![vec![t("h")]],
            rows: vec![vec![vec![]], vec![vec![t("c")]]],
        };
        let (out, _) = write(&doc(vec![table]));
        let d = read(out.as_bytes()).unwrap();
        assert_eq!(d.blocks.len(), 1, "{out:?}");
    }

    #[test]
    fn line_start_list_markers_escaped() {
        assert_eq!(md("- x"), "\\- x\n");
        assert_eq!(md("+ x"), "\\+ x\n");
        assert_eq!(md("1. x"), "1\\. x\n");
        assert_eq!(md("23) x"), "23\\) x\n");
        assert_eq!(md("=== x"), "\\=== x\n");
        // The whole run is escaped now (a pair would be strikethrough).
        assert_eq!(md("~~~"), "\\~\\~\\~\n");
        assert_eq!(md("~ x"), "\\~ x\n");
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

    /// The reader takes backslash escapes and entities off an info
    /// string, so the writer escapes every `\\` and `&` in it.
    #[test]
    fn info_string_reads_back_unchanged() {
        for info in ["a\\b", "c\\*", "x&amp;y", "&", "\\", "r`s\\&t", "\\\\"] {
            let d = doc(vec![Block::CodeBlock {
                info: info.into(),
                text: "x\n".into(),
            }]);
            for dialect in [Dialect::CommonMark, Dialect::Gfm] {
                let (out, _) = write_limited_with(&d, Limits::UNLIMITED, dialect).unwrap();
                assert_eq!(
                    read_with(out.as_bytes(), dialect).unwrap(),
                    d,
                    "{info:?}: {out:?}"
                );
            }
        }
        let info = |i: &str| {
            write(&doc(vec![Block::CodeBlock {
                info: i.into(),
                text: String::new(),
            }]))
            .0
        };
        assert_eq!(info("a\\b&c"), "```a\\\\b\\&c\n```\n");
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
                auto: false,
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
                auto: false,
            },
        ])]);
        assert_eq!(
            write(&d).0,
            "[l](https://x/a\\(b\\) \"t\\\"q\")![alt](<a b>)[](<>)\n"
        );
    }

    #[test]
    fn extended_autolink_written_as_its_text() {
        // An extended autolink is its text as written, unescaped: a GFM
        // reader links it again; a `\\_` would cut it.
        for input in [
            "www.a_b.c.d/x_y*z",
            "http://a.b/~~c~~d",
            "a_b@c.d",
            "x www.a.b.",
        ] {
            let d = read_with(input.as_bytes(), Dialect::Gfm).unwrap();
            let (s, l) = write(&d);
            assert_eq!(s, format!("{input}\n"));
            assert!(l.is_empty(), "{input:?}: {l:?}");
            assert_eq!(read_with(s.as_bytes(), Dialect::Gfm).unwrap(), d);
        }
        // In an address an `_` that could open or close emphasis is
        // escaped, so it cannot pair with one in another address; the
        // reader drops the escape before it finds the address.
        for (input, written) in [
            ("\\_a@b.c x_@d.e", "\\_a@b.c x\\_@d.e"),
            ("a.\\_b@c.d x a\\_.b@c.d", "a.\\_b@c.d x a\\_.b@c.d"),
        ] {
            let d = read_with(input.as_bytes(), Dialect::Gfm).unwrap();
            let Block::Paragraph(p) = &d.blocks[0] else {
                panic!("{d:?}")
            };
            assert_eq!(
                p.iter()
                    .filter(|i| matches!(i, Inline::Link { auto: true, .. }))
                    .count(),
                2,
                "{input:?}: {p:?}"
            );
            let (s, l) = write(&d);
            assert_eq!(s, format!("{written}\n"));
            assert!(l.is_empty(), "{input:?}: {l:?}");
            assert_eq!(read_with(s.as_bytes(), Dialect::Gfm).unwrap(), d);
        }
    }

    #[test]
    fn heading_breaks_become_spaces_and_markers_stay_plain() {
        let d = doc(vec![Block::Heading {
            level: 1,
            content: vec![t("1. a"), Inline::HardBreak, t("b")],
        }]);
        assert_eq!(write(&d).0, "# 1. a b\n");
    }

    #[test]
    fn dialect_cm_keeps_old_behaviour() {
        for input in [
            "| a | b |\n| - | - |\n| c | d |\n",
            "~~x~~ www.x.y http://x.y\n",
            "- [ ] a\n- [x] b\n",
            "<script>\n",
        ] {
            let old = read(input.as_bytes()).unwrap();
            assert_eq!(
                read_with(input.as_bytes(), Dialect::CommonMark).unwrap(),
                old
            );
            assert_eq!(
                read_limited_with(input.as_bytes(), Limits::DEFAULT, Dialect::CommonMark).unwrap(),
                read_limited(input.as_bytes(), Limits::DEFAULT).unwrap()
            );
            assert!(
                !format!("{old:?}").contains("Table")
                    && !format!("{old:?}").contains("Strikethrough"),
                "{input:?}"
            );
        }
    }

    #[test]
    fn task_item_marker_and_body() {
        let item = |checked, blocks| crate::ListItem { checked, blocks };
        let l = Block::List {
            start: None,
            tight: true,
            items: vec![
                item(Some(true), vec![para("a")]),
                item(Some(false), vec![]),
                item(Some(true), vec![]),
            ],
        };
        assert_eq!(write(&doc(vec![l])).0, "- [x] a\n- [ ]\n- [x]\n");
    }

    #[test]
    fn table_cells_are_joined_by_a_space() {
        let table = Block::Table {
            align: vec![crate::Align::None; 2],
            head: vec![vec![t("a")], vec![t("b")]],
            rows: vec![vec![vec![t("c")], vec![t("d")]]],
        };
        assert_eq!(write(&doc(vec![table])).0, "a b\nc d\n");
    }

    /// Only the last cell of the last row ends a rendered line: a space
    /// there is an entity, a space at the end of any other cell is not.
    #[test]
    fn table_trailing_space_only_in_the_last_cell_is_encoded() {
        let table = Block::Table {
            align: vec![crate::Align::None; 2],
            head: vec![vec![t("h")], vec![t("i")]],
            rows: vec![vec![vec![t("a ")], vec![t("b ")]]],
        };
        assert_eq!(write(&doc(vec![table])).0, "h i\na  b&#32;\n");
    }

    /// An inner emphasis takes `_` only at the edge of the outer one, next
    /// to something that is not a word character; elsewhere, and in front
    /// of or behind a word, it keeps `*` (the shape that cannot be
    /// written either way is left as it was).
    #[test]
    fn inner_emphasis_delimiter_by_neighbour() {
        let e = |v: Vec<Inline>| Inline::Emph(v);
        for (inner, want) in [
            (vec![e(vec![t("x")])], "*_x_*\n"),
            (vec![t("a "), e(vec![t("x")])], "*a _x_*\n"),
            (vec![e(vec![t("x")]), t(" b")], "*_x_ b*\n"),
            (vec![t("a "), e(vec![t("x")]), t(" b")], "*a *x* b*\n"),
            (vec![e(vec![t("x")]), t("b")], "**x*b*\n"),
            (vec![t("b"), e(vec![t("x")])], "*b*x**\n"),
        ] {
            let d = doc(vec![Block::Paragraph(vec![e(inner)])]);
            assert_eq!(write(&d).0, want);
        }
    }

    #[test]
    fn strikethrough_ends_a_line_only_at_the_end_of_it() {
        let d = doc(vec![Block::Paragraph(vec![
            Inline::Strikethrough(vec![t("a ")]),
            t("b"),
        ])]);
        let (out, losses) = write(&d);
        assert_eq!(out, "a b\n");
        assert_eq!(losses, BTreeSet::from([Loss::Strikethrough]));
        let d = doc(vec![Block::Paragraph(vec![
            t("b"),
            Inline::Strikethrough(vec![t("a ")]),
        ])]);
        assert_eq!(write(&d).0, "ba&#32;\n");
    }

    /// Only an email address gets its `_` escaped; the text of a web
    /// autolink is written as it came.
    #[test]
    fn web_autolink_underscores_are_not_escaped() {
        let input = "www.a.b/x_.y";
        let d = read_with(input.as_bytes(), Dialect::Gfm).unwrap();
        let Block::Paragraph(p) = &d.blocks[0] else {
            panic!("{d:?}")
        };
        assert!(matches!(p[0], Inline::Link { auto: true, .. }), "{p:?}");
        assert_eq!(write(&d).0, format!("{input}\n"));
        assert_eq!(read_with(input.as_bytes(), Dialect::Gfm).unwrap(), d);
    }

    /// A part's first list after a part that ended with a list of the
    /// same kind takes the other marker, so the two parts joined by a
    /// blank line read back as two lists, an ordered one with its start;
    /// a list of the other kind, or none, changes nothing.
    #[test]
    fn write_part_keeps_lists_apart_across_parts() {
        let part = |blocks, after| {
            let (s, _, end) =
                write_part(&doc(blocks), Limits::UNLIMITED, Dialect::Gfm, after).unwrap();
            (s, end)
        };
        let item = |s: &str| vec![para(s)];
        let (a, end) = part(vec![para("x"), list(None, true, vec![item("a")])], None);
        assert_eq!(a, "x\n\n- a\n");
        let (b, end) = part(vec![list(None, true, vec![item("b")])], end);
        assert_eq!(b, "* b\n");
        // An empty part ends as the one before it did.
        let (e, end) = part(vec![], end);
        assert_eq!(e, "");
        let (c, end) = part(
            vec![
                list(None, true, vec![item("c")]),
                list(Some(1), true, vec![item("d")]),
            ],
            end,
        );
        assert_eq!(c, "- c\n\n1. d\n");
        let (f, end) = part(vec![list(Some(5), true, vec![item("five")])], end);
        assert_eq!(f, "5) five\n");
        let whole = [a, b, c, f].join("\n");
        let d = read_with(whole.as_bytes(), Dialect::Gfm).unwrap();
        let lists: Vec<Option<u64>> = d
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::List { start, .. } => Some(*start),
                _ => None,
            })
            .collect();
        assert_eq!(lists, [None, None, None, Some(1), Some(5)], "{whole}");
        // A paragraph last: no list to keep apart from.
        let (_, end) = part(vec![list(None, true, vec![item("g")]), para("p")], end);
        assert_eq!(end, None);
        let (g, _) = part(vec![list(None, true, vec![item("h")])], end);
        assert_eq!(g, "- h\n");
        // The CommonMark mode takes the same state.
        let (s, _, _) = write_part(
            &doc(vec![list(None, true, vec![item("i")])]),
            Limits::UNLIMITED,
            Dialect::CommonMark,
            end.or(Some(ListEnd {
                ordered: false,
                alt: false,
            })),
        )
        .unwrap();
        assert_eq!(s, "* i\n");
    }

    /// The GFM writer, unbudgeted.
    fn gfm(d: &Document) -> (String, BTreeSet<Loss>) {
        write_limited_with(d, Limits::UNLIMITED, Dialect::Gfm).unwrap()
    }

    /// `d` written in GFM mode and read back by the GFM reader.
    fn gfm_again(d: &Document) -> Document {
        read_with(gfm(d).0.as_bytes(), Dialect::Gfm).unwrap()
    }

    fn table(align: Vec<Align>, head: Vec<Vec<Inline>>, rows: Vec<Vec<Vec<Inline>>>) -> Block {
        Block::Table { align, head, rows }
    }

    /// The chapter `body` through the XHTML reader.
    fn xhtml(body: &str) -> Document {
        let page = format!(
            "<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>T</title></head>\
             <body>{body}</body></html>"
        );
        crate::xhtml_read::read_limited(&page, Limits::DEFAULT)
            .unwrap()
            .0
    }

    #[test]
    fn gfm_table_with_alignment() {
        let d = doc(vec![table(
            vec![Align::None, Align::Left, Align::Center, Align::Right],
            vec![vec![t("a")], vec![t("b")], vec![t("c")], vec![t("d")]],
            vec![vec![vec![t("1")], vec![t("2")], vec![t("3")], vec![t("4")]]],
        )]);
        let (out, losses) = gfm(&d);
        assert_eq!(
            out,
            "| a | b | c | d |\n| --- | :-- | :-: | --: |\n| 1 | 2 | 3 | 4 |\n"
        );
        assert!(losses.is_empty(), "{losses:?}");
        assert_eq!(gfm_again(&d), d);
    }

    #[test]
    fn gfm_empty_header_row_kept() {
        // An XHTML table without `th` or `thead` has an empty header.
        let d = xhtml("<table><tr><td>x</td><td>y</td></tr></table>");
        let (out, losses) = gfm(&d);
        assert_eq!(out, "|  |  |\n| --- | --- |\n| x | y |\n");
        assert!(losses.is_empty(), "{losses:?}");
        assert_eq!(gfm_again(&d), d);
        // An empty body row stays a row.
        let d = doc(vec![table(
            vec![Align::None],
            vec![vec![t("h")]],
            vec![vec![vec![]], vec![vec![t("x")]]],
        )]);
        assert_eq!(gfm(&d).0, "| h |\n| --- |\n|  |\n| x |\n");
        assert_eq!(gfm_again(&d), d);
    }

    #[test]
    fn pipe_and_break_in_cell_roundtrip() {
        let d = xhtml("<table><tr><td>a | b<br/>c</td></tr></table>");
        let (out, losses) = gfm(&d);
        assert_eq!(out, "|  |\n| --- |\n| a \\| b c |\n");
        assert_eq!(losses, BTreeSet::from([Loss::CellsFlattened]));
        let Block::Table { rows, .. } = &gfm_again(&d).blocks[0] else {
            panic!("{out:?}")
        };
        assert_eq!(rows, &vec![vec![vec![t("a | b c")]]]);
        // A pipe in a code span, a link target and title, an image's alt
        // and raw HTML, and a backslash before a pipe: all come back.
        let cell = vec![
            Inline::Code("x|y".into()),
            t("\\|"),
            Inline::Link {
                dest: "/a|b".into(),
                title: "t|".into(),
                content: vec![t("l|")],
                auto: false,
            },
            Inline::Image {
                dest: "a b|".into(),
                title: String::new(),
                alt: vec![t("|")],
            },
            Inline::Html("<a title=\"|\">".into()),
        ];
        let d = doc(vec![table(vec![Align::None], vec![cell], vec![])]);
        let (out, losses) = gfm(&d);
        assert!(losses.is_empty(), "{losses:?}");
        assert_eq!(gfm_again(&d), d, "{out:?}");
        // Edge white space of a cell, which the reader trims, is kept.
        let d = doc(vec![table(vec![Align::None], vec![vec![t(" a ")]], vec![])]);
        assert_eq!(gfm(&d).0, "| &#32;a&#32; |\n| --- |\n");
        assert_eq!(gfm_again(&d), d);
    }

    /// A pipe table's header row ends the paragraph above it, so in a
    /// tight item a table follows a paragraph after one newline and the
    /// list stays tight. What follows a table still needs a blank line
    /// (a paragraph line under it would be a body row), so such an item
    /// is written loose, and reads back loose.
    #[test]
    fn gfm_table_in_tight_item_stays_tight() {
        let table = table(
            vec![Align::None],
            vec![vec![t("h")]],
            vec![vec![vec![t("c")]]],
        );
        let item = list(
            None,
            true,
            vec![vec![para("p"), table.clone()], vec![para("r")]],
        );
        let d = doc(vec![item]);
        let (out, losses) = gfm(&d);
        assert_eq!(out, "- p\n  | h |\n  | --- |\n  | c |\n- r\n");
        assert!(losses.is_empty(), "{losses:?}");
        assert_eq!(gfm_again(&d), d);
        // Through the XHTML reader: text, then a table, in a list item.
        let d = xhtml("<ul><li>p<table><tr><th>h</th></tr><tr><td>c</td></tr></table></li></ul>");
        assert!(gfm(&d).0.starts_with("- p\n  | h |\n"), "{:?}", gfm(&d).0);
        assert_eq!(gfm_again(&d), d);
        // A paragraph after the table: a blank line, so loose.
        let item = list(None, true, vec![vec![para("p"), table, para("q")]]);
        let (out, _) = gfm(&doc(vec![item]));
        assert_eq!(out, "- p\n  | h |\n  | --- |\n  | c |\n\n  q\n");
        let Block::List { tight, .. } = &read_with(out.as_bytes(), Dialect::Gfm).unwrap().blocks[0]
        else {
            panic!("{out:?}")
        };
        assert!(!tight);
    }

    #[test]
    fn block_content_in_cell_flattened_with_loss() {
        let d = xhtml("<table><tr><td><p>a</p><p>b</p><ul><li>c</li></ul></td></tr></table>");
        let (out, losses) = gfm(&d);
        assert_eq!(out, "|  |\n| --- |\n| a b c |\n");
        assert_eq!(losses, BTreeSet::from([Loss::CellsFlattened]));
        // A line ending inside code, HTML or a title is a space too.
        for inline in [
            Inline::Code("a\nb".into()),
            Inline::Html("<a\nb>".into()),
            Inline::Link {
                dest: "/".into(),
                title: "a\rb".into(),
                content: vec![t("l")],
                auto: false,
            },
            t("a\nb"),
            Inline::SoftBreak,
        ] {
            let d = doc(vec![table(
                vec![Align::None],
                vec![vec![t("x"), inline.clone(), t("y")]],
                vec![],
            )]);
            let (out, losses) = gfm(&d);
            assert_eq!(out.lines().count(), 2, "{inline:?}: {out:?}");
            assert_eq!(losses, BTreeSet::from([Loss::CellsFlattened]), "{inline:?}");
        }
        // Nothing flattened, no loss; a trailing break shows nothing.
        let d = doc(vec![table(
            vec![Align::None],
            vec![vec![t("x"), Inline::HardBreak]],
            vec![],
        )]);
        assert_eq!(gfm(&d), ("| x |\n| --- |\n".into(), BTreeSet::new()));
        // A row wider than the header pads the header, no cell is lost.
        let d = doc(vec![table(
            vec![Align::Right],
            vec![vec![t("h")]],
            vec![vec![vec![t("a")], vec![t("b")]]],
        )]);
        assert_eq!(gfm(&d).0, "| h |  |\n| --: | --- |\n| a | b |\n");
        // No column: nothing to write.
        let d = doc(vec![table(vec![], vec![], vec![vec![]])]);
        assert_eq!(gfm(&d), (String::new(), BTreeSet::new()));
    }

    #[test]
    fn strikethrough_written_and_read_back() {
        let s = |c| Inline::Strikethrough(c);
        let d = doc(vec![Block::Paragraph(vec![
            t("a "),
            s(vec![t("b "), Inline::Strong(vec![t("c")])]),
            t(" d"),
        ])]);
        let (out, losses) = gfm(&d);
        assert_eq!(out, "a ~~b **c**~~ d\n");
        assert!(losses.is_empty(), "{losses:?}");
        assert_eq!(gfm_again(&d), d);
        // From a book, in a cell too.
        let d = xhtml(
            "<p>x <del>y</del> <s>z</s></p><table><tr><td><strike>w</strike></td></tr></table>",
        );
        let (out, losses) = gfm(&d);
        assert_eq!(out, "x ~~y~~ ~~z~~\n\n|  |\n| --- |\n| ~~w~~ |\n");
        assert!(losses.is_empty(), "{losses:?}");
        assert_eq!(gfm_again(&d), d);
        // Adjacent ones share the delimiters, a nested one is its outer
        // one, an empty one is left out.
        let d = doc(vec![Block::Paragraph(vec![
            s(vec![t("a")]),
            s(vec![s(vec![t("b")])]),
            t(" "),
            s(vec![]),
            t("c"),
        ])]);
        assert_eq!(gfm(&d).0, "~~ab~~ c\n");
        let d = doc(vec![Block::Paragraph(vec![s(vec![]), s(vec![])])]);
        assert_eq!(gfm(&d).0, "");
    }

    /// The text of `c`, a line break as `\n`: what a reader shows, without
    /// the formatting.
    fn shown(c: &[Inline], out: &mut String) {
        for i in c {
            match i {
                Inline::Text(x) | Inline::Code(x) => out.push_str(x),
                Inline::SoftBreak | Inline::HardBreak => out.push('\n'),
                Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => shown(c, out),
                Inline::Link { content, .. } => shown(content, out),
                Inline::Image { alt, .. } => shown(alt, out),
                Inline::Html(h) => out.push_str(h),
            }
        }
    }

    /// The one paragraph of `d`, shown.
    fn shown_para(d: &Document) -> String {
        let mut out = String::new();
        if let [Block::Paragraph(c)] = d.blocks.as_slice() {
            shown(c, &mut out);
        }
        out
    }

    /// How many `Emph`, `Strong` and `Strikethrough` nodes `c` holds.
    fn marks(c: &[Inline]) -> usize {
        c.iter()
            .map(|i| match i {
                Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => 1 + marks(c),
                Inline::Link { content: c, .. } => marks(c),
                _ => 0,
            })
            .sum()
    }

    /// A delimiter has to open and close by the flanking rules (GFM's
    /// `~~` as `*`): white space at the edge of the content goes outside
    /// it, and where it still cannot (punctuation inside next to a letter
    /// outside, nothing left), the marks are left out and reported. A GFM
    /// reader then shows the same text with no stray `~~` or `*`.
    #[test]
    fn gfm_marks_that_cannot_flank() {
        let s = |c| Inline::Strikethrough(c);
        for (body, want, kept, loss) in [
            ("<p>a<del> x</del>b</p>", "a ~~x~~b\n", true, None),
            ("<p>x <del>a </del>y</p>", "x ~~a~~ y\n", true, None),
            ("<p>a <em>b </em>c</p>", "a *b* c\n", true, None),
            ("<p>a<strong> b</strong>c</p>", "a **b**c\n", true, None),
            (
                "<p>He said <em>\"no\"</em>.</p>",
                "He said *\"no\"*.\n",
                true,
                None,
            ),
            (
                "<p>a<del>\"b\"</del>c</p>",
                "a\"b\"c\n",
                false,
                Some(Loss::Strikethrough),
            ),
            (
                "<p>a<del>(b)</del></p>",
                "a(b)\n",
                false,
                Some(Loss::Strikethrough),
            ),
            (
                "<p>word<del>.</del>x</p>",
                "word.x\n",
                false,
                Some(Loss::Strikethrough),
            ),
            (
                "<p>a<em>\"b\"</em>c</p>",
                "a\"b\"c\n",
                false,
                Some(Loss::Emphasis),
            ),
            (
                "<p>a<b>\"b\"</b>c</p>",
                "a\"b\"c\n",
                false,
                Some(Loss::Emphasis),
            ),
            (
                "<p>x<del> </del>y</p>",
                "x y\n",
                false,
                Some(Loss::Strikethrough),
            ),
        ] {
            let d = xhtml(body);
            let (out, losses) = gfm(&d);
            assert_eq!(out, want, "{body}");
            assert_eq!(
                losses.iter().copied().find(|l| *l != Loss::Styles),
                loss,
                "{body}"
            );
            let back = gfm_again(&d);
            assert_eq!(shown_para(&back), shown_para(&d), "{body}: {out:?}");
            let Block::Paragraph(c) = &back.blocks[0] else {
                panic!("{body}")
            };
            assert_eq!(marks(c), usize::from(kept), "{body}: {back:?}");
        }
        for (c, want) in [
            // Code inside, a letter outside: no delimiter.
            (
                vec![t("a"), s(vec![Inline::Code("x".into())]), t("b")],
                "a`x`b\n",
            ),
            // A hard break inside: the break stays, the marks go.
            (vec![t("a"), s(vec![Inline::HardBreak]), t("b")], "a\\\nb\n"),
            // At a line start, what the marks kept from being a list
            // marker is escaped.
            (vec![s(vec![t("1.\"")]), t("b")], "1\\.\"b\n"),
            (vec![Inline::Emph(vec![t("-\"")]), t("b")], "\\-\"b\n"),
            // White space moved out next to a lone tilde: escaped, so the
            // delimiter stays two tildes.
            (vec![t("a"), s(vec![t(" ~b")])], "a ~~\\~b~~\n"),
            (vec![t("a"), s(vec![t("b~ ")]), t("c")], "a~~b\\~~~ c\n"),
            // Moved to a line edge: an entity, so it is not stripped.
            (vec![s(vec![t(" a ")])], "&#32;~~a~~&#32;\n"),
            // Nothing in it: nothing written, nothing lost.
            (vec![t("a"), Inline::Emph(vec![]), t("b")], "ab\n"),
        ] {
            let d = doc(vec![Block::Paragraph(c.clone())]);
            let (out, _) = gfm(&d);
            assert_eq!(out, want, "{c:?}");
            assert_eq!(shown_para(&gfm_again(&d)), shown_para(&d), "{c:?}: {out:?}");
        }
    }

    /// A reader pairs delimiter runs, not marks: two runs of the same
    /// character side by side are one, and an opener that can also close
    /// closes an outer mark of the same delimiter. The GFM writer keeps
    /// them apart (`_` for `*`) or leaves the mark out.
    #[test]
    fn gfm_marks_pair_as_written() {
        let e = |c| Inline::Emph(c);
        let st = |c| Inline::Strong(c);
        let link = |c| Inline::Link {
            dest: "u".into(),
            title: String::new(),
            content: c,
            auto: false,
        };
        for (c, want) in [
            // Side by side: the second takes `_`.
            (vec![e(vec![t("a")]), e(vec![t("b")])], "*a*_b_\n"),
            // At the end of one of the same character: `__`.
            (
                vec![t("a "), st(vec![t("x "), st(vec![t("y")])]), t(" b")],
                "a **x __y__** b\n",
            ),
            // After white space an inner opener cannot close: kept.
            (
                vec![e(vec![t("a "), e(vec![t("\"b\"")]), t(" c")])],
                "*a *\"b\"* c*\n",
            ),
            // A run of 1 and one of 2 do not pair (rule of 3): kept.
            (
                vec![e(vec![t("foo"), st(vec![t("bar")]), t("baz")])],
                "*foo**bar**baz*\n",
            ),
            // Autolink text: an entity ending the line is no white space,
            // so the link would run into it.
            (vec![t("www.x.y\u{a0}")], "www&#46;x.y&#xA0;\n"),
        ] {
            let d = doc(vec![Block::Paragraph(c.clone())]);
            assert_eq!(gfm(&d).0, want, "{c:?}");
            assert_eq!(gfm_again(&d), d, "{c:?}");
        }
        for (c, want) in [
            // At the edges of a `**`, inside a word: `*` would join its
            // run and `_` cannot open or close there, so both are left out.
            (vec![st(vec![e(vec![t("x")]), e(vec![t("y")])])], "**xy**\n"),
            // An opener that could close the outer one of its delimiter
            // (punctuation on both sides of it): left out.
            (
                vec![e(vec![t("a ("), e(vec![t("\"b\"")]), t(") c")])],
                "*a (\"b\") c*\n",
            ),
            // Hard breaks that end the paragraph show nothing, inside a
            // mark too, and leave no backslash behind.
            (
                vec![e(vec![t("b"), Inline::HardBreak]), Inline::HardBreak],
                "*b*\n",
            ),
            (vec![t("a"), Inline::HardBreak, Inline::HardBreak], "a\n"),
            // A number and its `.` from two nodes still make no list.
            (vec![t("7"), e(vec![t(". x")])], "7\\. x\n"),
            // After a link whose text ends in a line break, a space is no
            // longer at a line start.
            (
                vec![link(vec![t("a"), Inline::HardBreak]), t(" b")],
                "[a\\\n](u) b\n",
            ),
        ] {
            let d = doc(vec![Block::Paragraph(c.clone())]);
            let (out, _) = gfm(&d);
            assert_eq!(out, want, "{c:?}");
            // Breaks that end the paragraph are not written.
            let shown = |d: &Document| shown_para(d).trim_end_matches('\n').to_string();
            assert_eq!(shown(&gfm_again(&d)), shown(&d), "{c:?}: {out:?}");
        }
    }

    #[test]
    fn literal_tildes_still_escaped() {
        for (text, want) in [
            ("a~~b", "a\\~\\~b"),
            ("~~~", "\\~\\~\\~"),
            ("a~b", "a~b"),
            ("~a", "\\~a"),
        ] {
            let d = doc(vec![para(text)]);
            assert_eq!(gfm(&d).0, format!("{want}\n"), "{text:?}");
            assert_eq!(gfm_again(&d), d, "{text:?}");
        }
        // A lone tilde next to a delimiter would lengthen it.
        let s = |c| Inline::Strikethrough(c);
        let d = doc(vec![Block::Paragraph(vec![
            t("a~"),
            s(vec![t("~b~")]),
            t("~c"),
        ])]);
        assert_eq!(gfm(&d).0, "a\\~~~\\~b\\~~~\\~c\n");
        assert_eq!(gfm_again(&d), d);
        // So would one inside a nested strikethrough at the end.
        let d = doc(vec![Block::Paragraph(vec![s(vec![s(vec![t("b~")])])])]);
        assert_eq!(gfm(&d).0, "~~b\\~~~\n");
        // A line-start colon could start a delimiter row under a paragraph
        // line and make a table of it.
        let d = doc(vec![Block::Paragraph(vec![
            t("a"),
            Inline::HardBreak,
            t(":--"),
        ])]);
        assert_eq!(gfm(&d).0, "a\\\n\\:--\n");
        assert_eq!(gfm_again(&d), d);
    }

    /// A paragraph line made only of `:` and dashes (`:--`, `:-:`) under
    /// another line is a one-column delimiter row for a GFM reader, which
    /// makes the line above a table header: its `:` is escaped in both
    /// modes, and only there (the CommonMark mode writes txt -> md, so
    /// every other line keeps its bytes).
    #[test]
    fn delimiter_row_shape_escaped_in_both_modes() {
        for (text, want) in [
            ("a\n:--", "a\\\n\\:--\n"),
            ("a\n:-:", "a\\\n\\:-:\n"),
            ("x\ny\n:--:", "x\\\ny\\\n\\:--:\n"),
            ("a\n:-\n\nb", "a\\\n\\:-\n\nb\n"),
            // Not a delimiter row: unchanged in the CommonMark mode.
            (":--", ":--\n"),
            ("a\n:--\nb", "a\\\n:--\\\nb\n"),
            ("a\n:-- x", "a\\\n:-- x\n"),
            ("a\n:", "a\\\n:\n"),
            ("a\n:x-", "a\\\n:x-\n"),
            ("a\n :--", "a\\\n&#32;:--\n"),
            ("a\n:-- ", "a\\\n:--&#32;\n"),
        ] {
            let (d, _) = crate::txt::read(text.as_bytes()).unwrap();
            let (out, _) = write(&d);
            assert_eq!(out, want, "{text:?}");
            // No GFM reader finds a table in it, and it reads back as it was.
            let back = read_with(out.as_bytes(), Dialect::Gfm).unwrap();
            assert_eq!(back, d, "{text:?}: {out:?}");
            assert_eq!(read(out.as_bytes()).unwrap(), d, "{text:?}");
        }
        // The GFM mode escapes every line-start `:` (as before).
        let (d, _) = crate::txt::read(b"a\n:--").unwrap();
        assert_eq!(gfm(&d).0, "a\\\n\\:--\n");
        assert_eq!(gfm_again(&d), d);
    }

    #[test]
    fn task_list_and_autolink_unchanged() {
        for input in [
            "- [x] done\n- [ ] open\n",
            "www.a_b.c.d/x_y*z\n",
            "see http://a.b/~~c~~d and a_b@c.d\n",
        ] {
            let d = read_with(input.as_bytes(), Dialect::Gfm).unwrap();
            assert_eq!(gfm(&d), write(&d), "{input:?}");
            assert_eq!(gfm(&d).0, input);
            assert_eq!(gfm_again(&d), d, "{input:?}");
        }
    }

    /// A URL in text that a GFM reader would link with one of the writer's
    /// escapes inside gets no link (an entity where it would start); one
    /// that needs no escape is linked, as it reads. Each is stable.
    #[test]
    fn bare_url_text_never_holds_an_escape() {
        for (text, want, same_tree) in [
            ("www.z[!,", "www&#46;z\\[\\!,", true),
            (
                "see http://x.y/a_b now",
                "see http&#58;//x.y/a\\_b now",
                true,
            ),
            // The first one is linked, the second not.
            ("www.a www.b_c", "www.a www&#46;b\\_c", false),
            ("www.a.b/c-d.", "www.a.b/c-d.", false),
            ("www.co'/</do", "www&#46;co'/\\</do", true),
        ] {
            let d = doc(vec![para(text)]);
            let w1 = gfm(&d).0;
            assert_eq!(w1, format!("{want}\n"), "{text:?}");
            let d2 = gfm_again(&d);
            assert_eq!(gfm(&d2).0, w1, "{text:?}");
            if same_tree {
                assert_eq!(d2, d, "{text:?}");
            }
        }
        // Text that more syntax follows on the line would run into it.
        let d = doc(vec![Block::Paragraph(vec![
            t("www.z"),
            Inline::Emph(vec![t("x")]),
        ])]);
        assert_eq!(gfm(&d).0, "www&#46;z*x*\n");
        assert_eq!(gfm_again(&d), d);
        // An address holding a `www.` is not read as a web link.
        // (A GFM reader makes one of the text where its `www.` is an
        // entity.)
        let d = read_with(b"www&#46;goog@e.ch x", Dialect::Gfm).unwrap();
        let Block::Paragraph(p) = &d.blocks[0] else {
            panic!("{d:?}")
        };
        assert!(
            matches!(&p[0], Inline::Link { auto: true, dest, .. } if dest.starts_with("mailto:")),
            "{p:?}"
        );
        assert_eq!(gfm(&d).0, "www&#46;goog@e.ch x\n");
        assert_eq!(gfm_again(&d), d);
        // The CommonMark mode keeps its old output.
        assert_eq!(write(&doc(vec![para("www.z_")])).0, "www.z\\_\n");
        // One pass per run: a run of candidates stays linear.
        let long = "www.".repeat(1 << 16) + "_";
        assert_eq!(autolink_guards(&long, true, false).len(), 1 << 16);
    }

    /// The table writer is linear in the cells: four times the cells (as
    /// columns or as rows) take about four times as long, where a quadratic
    /// path would take sixteen. The output grows by the same factor.
    /// Marks whose edge white space moves out, and marks left out, cost
    /// linear time: four times the input, under eight times the time (a
    /// bound loose enough for load, far below the sixteen of a quadratic
    /// path, which a white space stretch rescanned per run once was).
    #[test]
    fn gfm_marks_are_linear() {
        use std::time::{Duration, Instant};
        // Nested as deep as the readers allow (256), alternating kinds.
        let nest = |leaf: Vec<Inline>| {
            let mut x = leaf;
            for i in 0..255 {
                x = vec![
                    match i % 3 {
                        0 => Inline::Emph(x),
                        1 => Inline::Strong(x),
                        _ => Inline::Strikethrough(x),
                    },
                    t(" "),
                ];
            }
            doc(vec![Block::Paragraph(
                [vec![t("a"), Inline::Emph(x), t("b")]].concat(),
            )])
        };
        let shapes = |n: usize| {
            [
                // One nest around long edge white space.
                nest(vec![t(&" ".repeat(n)), t("\"x\""), t(&" ".repeat(n))]),
                // Many marks left out (punctuation inside, letters out).
                doc(vec![Block::Paragraph(
                    (0..n)
                        .map(|i| {
                            if i % 2 == 0 {
                                Inline::Strikethrough(vec![t(" \"x\" ")])
                            } else {
                                t("a")
                            }
                        })
                        .collect(),
                )]),
            ]
        };
        let time = |d: &Document| {
            (0..3)
                .map(|_| {
                    let t = Instant::now();
                    let n = gfm(d).0.len();
                    (t.elapsed(), n)
                })
                .min()
                .unwrap()
        };
        for (small, big) in shapes(4_000).iter().zip(shapes(16_000).iter()) {
            let ((t1, n1), (t4, n4)) = (time(small), time(big));
            assert!(n4 <= 4 * n1 + 4096, "{n1} -> {n4} bytes");
            assert!(
                t4 < 8 * t1.max(Duration::from_millis(1)),
                "{t1:?} -> {t4:?} for four times the input"
            );
        }
    }

    /// What an item needs to know about the items after it (only hard
    /// breaks follow, only edge white space or breaks follow, the next
    /// character written) costs linear time over a list: four times the
    /// items, under eight times the time (loose enough for load, far
    /// below the sixteen of a list rescanned per item). The shapes are
    /// the long runs such a rescan walks: hard breaks in both modes (a
    /// reader builds them from `a\` and lines holding only `\`), marks
    /// holding only a break inside a mark (`<em>a<b><br/></b>…</em>`),
    /// and empty marks before one that writes something.
    #[test]
    fn suffix_checks_are_linear() {
        use std::time::{Duration, Instant};
        let para = |items: Vec<Inline>| doc(vec![Block::Paragraph(items)]);
        let breaks = |n: usize| [vec![t("a")], vec![Inline::HardBreak; n], vec![t("b")]].concat();
        let shapes = |n: usize| {
            [
                (para(breaks(n)), Dialect::Gfm),
                (para(breaks(n)), Dialect::CommonMark),
                (
                    para(vec![Inline::Emph(
                        [
                            vec![t("a")],
                            vec![Inline::Strong(vec![Inline::HardBreak]); n],
                        ]
                        .concat(),
                    )]),
                    Dialect::Gfm,
                ),
                (
                    para(
                        [
                            vec![t("a")],
                            vec![Inline::Emph(vec![]); n],
                            vec![Inline::Emph(vec![t("b")]), t("c")],
                        ]
                        .concat(),
                    ),
                    Dialect::Gfm,
                ),
            ]
        };
        // The fastest of a few runs: the least disturbed by other load.
        let time = |(d, dialect): &(Document, Dialect)| {
            (0..3)
                .map(|_| {
                    let t = Instant::now();
                    let n = write_limited_with(d, Limits::UNLIMITED, *dialect)
                        .unwrap()
                        .0
                        .len();
                    (t.elapsed(), n)
                })
                .min()
                .unwrap()
        };
        for (small, big) in shapes(4_000).iter().zip(shapes(16_000).iter()) {
            let ((t1, n1), (t4, n4)) = (time(small), time(big));
            assert!(n4 <= 4 * n1 + 4096, "{n1} -> {n4} bytes");
            assert!(
                t4 < 8 * t1.max(Duration::from_millis(1)),
                "{:?}: {t1:?} -> {t4:?} for four times the items",
                small.1
            );
        }
    }

    #[test]
    fn table_output_is_linear() {
        use std::time::{Duration, Instant};
        let shape = |cols: usize, rows: usize| {
            let cell = |_| vec![t("c|"), Inline::HardBreak, t("x")];
            doc(vec![table(
                vec![Align::Center; cols],
                (0..cols).map(cell).collect(),
                (0..rows).map(|_| (0..cols).map(cell).collect()).collect(),
            )])
        };
        // The fastest of a few runs: the least disturbed by other load.
        let time = |d: &Document| {
            (0..3)
                .map(|_| {
                    let t = Instant::now();
                    let n = gfm(d).0.len();
                    (t.elapsed(), n)
                })
                .min()
                .unwrap()
        };
        for (small, big) in [
            (shape(2_000, 10), shape(8_000, 10)),
            (shape(10, 2_000), shape(10, 8_000)),
        ] {
            let ((t1, n1), (t4, n4)) = (time(&small), time(&big));
            assert!(n4 <= 4 * n1, "{n1} -> {n4} bytes");
            assert!(
                t4 < 8 * t1.max(Duration::from_millis(1)),
                "{t1:?} -> {t4:?} for four times the cells"
            );
        }
    }

    /// `inlines` with emphasis taken out (its content left in place, so
    /// texts end up side by side) and autolinks made ordinary links: the
    /// markdown fuzz target's shape for its GFM fixed point.
    fn flatten(c: &mut Vec<Inline>) {
        let mut out = Vec::with_capacity(c.len());
        for mut i in std::mem::take(c) {
            match &mut i {
                Inline::Emph(inner) | Inline::Strong(inner) => {
                    flatten(inner);
                    out.append(inner);
                    continue;
                }
                Inline::Link { auto, content, .. } => {
                    *auto = false;
                    flatten(content);
                }
                Inline::Strikethrough(inner) => flatten(inner),
                _ => {}
            }
            out.push(i);
        }
        *c = out;
    }

    /// Writes `input`, read and flattened, three times in `dialect`, each
    /// time from the tree the last one reads back (flattened again).
    fn three_rounds(input: &str, dialect: Dialect) -> [String; 3] {
        let mut d = read_with(input.as_bytes(), dialect).unwrap();
        let mut w: [String; 3] = Default::default();
        for out in &mut w {
            for b in &mut d.blocks {
                if let Block::Paragraph(c) = b {
                    flatten(c);
                }
            }
            *out = write_limited_with(&d, Limits::UNLIMITED, dialect)
                .unwrap()
                .0;
            d = read_with(out.as_bytes(), dialect).unwrap();
        }
        w
    }

    /// Regression (fuzz): texts side by side were guarded one by one, a
    /// `www.` run reaching the end of one getting an entity that the
    /// same text read back as one did not, so the output changed one
    /// round later (`\://(www&#46;b/`, `\://(www.b/`, `&#58;//(...`).
    #[test]
    fn adjacent_texts_autolink_guard_is_stable() {
        for (input, first) in [
            (":/_/(www.b_/", "\\://(www.b/\n"),
            (":/_/(www.b_.", "&#58;//([www&#46;b](http://www.b).\n"),
        ] {
            let [w1, w2, w3] = three_rounds(input, Dialect::Gfm);
            assert_eq!(w1, first, "{input:?}");
            assert_eq!(w2, w3, "{input:?}: first round {w1:?}");
        }
        // Split texts are written as the one text they read back as.
        let split = doc(vec![Block::Paragraph(vec![t(":/"), t("/(www.b"), t("/")])]);
        let one = doc(vec![para("://(www.b/")]);
        assert_eq!(gfm(&split), gfm(&one));
        assert_eq!(gfm(&one).0, "\\://(www.b/\n");
        // The CommonMark mode writes texts one by one, as before (it
        // has no autolink guards, nor the line-start `:` escape).
        assert_eq!(write(&split).0, "://(www.b/\n");
    }

    /// The GFM fixed point over generated strings of the autolink family
    /// (`www.`, schemes, `://`, emphasis delimiters, brackets, `.`/`/`
    /// endings, entities for `.` and `:`): after one round the output
    /// no longer changes, wherever the CommonMark mode (the code both
    /// modes share) is stable itself, as the fuzz target checks.
    #[test]
    fn autolink_family_reaches_a_fixed_point() {
        const TOKENS: [&str; 16] = [
            "www.", "http://", "://", "_", "*", "(", ")", ".", "/", "b", ":", " ", "&#46;",
            "&#58;", "\\_", "~~",
        ];
        // 20 000 strings of up to 10 tokens (the artifacts have 9): a few
        // seconds in a debug build; a fixed seed keeps a failure
        // reproducible.
        const CASES: usize = 20_000;
        const MAX_TOKENS: u64 = 10;
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            // Knuth's MMIX linear congruential generator.
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            seed >> 33
        };
        for _ in 0..CASES {
            let n = 1 + next() % MAX_TOKENS;
            let input: String = (0..n)
                .map(|_| TOKENS[(next() % TOKENS.len() as u64) as usize])
                .collect();
            let [_, c2, c3] = three_rounds(&input, Dialect::CommonMark);
            if c2 != c3 {
                continue;
            }
            let [w1, w2, w3] = three_rounds(&input, Dialect::Gfm);
            assert_eq!(w2, w3, "{input:?}: first round {w1:?}");
        }
    }
}
