//! CommonMark 0.31.2 block structure: the first phase of the spec's
//! "Appendix: A parsing strategy", mirroring cmark's `blocks.c` so the
//! results match the reference implementation.
//!
//! Lines are fed one by one into a tree of open blocks kept in an arena
//! (no recursion while parsing). Each line first matches the prefixes of the
//! open containers, then may open new blocks, then its rest is added to the
//! innermost block. Paragraph content is collected raw; link reference
//! definitions are taken off the front of a paragraph when it closes (or
//! when it becomes a setext heading). After the last line the arena is
//! converted into the [`Block`] tree, and paragraph/heading text goes
//! through [`super::inlines`] with all definitions known.
//!
//! Container nesting (block quotes, list items) is capped at [`MAX_DEPTH`]:
//! a marker that would open a deeper container is left as text. This bounds
//! the recursion of the conversion, of every writer and of `Drop`. Every
//! line costs O(depth + length), so parsing is linear in the input.

use super::scan::{
    close_tag, is_space_or_tab, link_destination, link_label, link_title, open_tag, peek,
    skip_spaces, skip_spnl, utf8,
};
use super::{LinkDef, Refs, decoded_len, inlines, normalize_label, unescape};
use crate::budget::Meter;
use crate::{Align, Block, Document, Inline};
use crate::{Dialect, ListItem};

const TAB_STOP: usize = 4;
const CODE_INDENT: usize = 4;
/// Maximum number of nested block quotes and list items.
pub(super) const MAX_DEPTH: u32 = 256;
const ROOT: usize = 0;
/// Most cells a GFM table row may have: cmark-gfm counts them in a `u16`
/// and a line with more is not a row (no table opens; an open table ends).
const MAX_COLUMNS: usize = u16::MAX as usize;
/// Cells a GFM table may get as padding of short rows before it refuses
/// further rows: cmark-gfm 0.29.0.gfm.13's `MAX_AUTOCOMPLETED_CELLS`. A
/// header of many columns over many one-cell rows would otherwise grow the
/// cells with the product of the two; past it the line ends the table and
/// starts a paragraph, as in the reference.
const MAX_PADDING_CELLS: usize = 0x80000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Marker {
    /// `-`, `+` or `*`.
    Bullet(u8),
    /// `.` or `)`.
    Ordered(u8),
}

#[derive(Clone, Copy, Debug)]
struct ListData {
    marker: Marker,
    start: u64,
    /// Indentation of the marker, in columns.
    marker_offset: usize,
    /// Marker width plus the spaces after it, in columns.
    padding: usize,
}

#[derive(Debug)]
enum Kind {
    Document,
    BlockQuote,
    List {
        data: ListData,
        tight: bool,
    },
    Item(ListData),
    Paragraph,
    Heading {
        level: u8,
        setext: bool,
    },
    CodeBlock {
        fenced: bool,
        fence_char: u8,
        fence_len: usize,
        fence_offset: usize,
        info: String,
    },
    HtmlBlock(u8),
    ThematicBreak,
    /// GFM; a paragraph whose next line was a matching delimiter row. Cells
    /// are raw (pipe escapes taken off, trimmed) until the conversion reads
    /// their inlines; every row has `align.len()` cells.
    Table {
        align: Vec<Align>,
        head: Vec<String>,
        rows: Vec<Vec<String>>,
        /// Cells taken from the input, header included, for the padding
        /// cap ([`MAX_PADDING_CELLS`]).
        filled: usize,
    },
}

struct Node {
    kind: Kind,
    parent: usize,
    children: Vec<usize>,
    open: bool,
    /// Raw lines, each ending in `\n` (headings: the text only).
    content: String,
    last_line_blank: bool,
    last_line_checked: bool,
    start_line: usize,
    /// Block quotes and list items on the path from the root, self included.
    depth: u32,
    /// GFM: this paragraph already failed to be a table header, and is not
    /// tried again (as in cmark-gfm), so no paragraph is rescanned per line.
    table_tried: bool,
}

/// Parses `text` (valid UTF-8, NUL already replaced) into a document. The
/// meter is charged while the arena and the tree are built (`Err` is the
/// first budget that ran out).
pub(super) fn parse(
    text: &str,
    dialect: Dialect,
    m: &mut Meter,
) -> Result<Document, crate::LimitError> {
    let mut p = run(text, dialect, m);
    let blocks = p.convert_children(ROOT);
    p.m.result()?;
    Ok(Document { blocks })
}

/// The block phase alone: every block closed, definitions collected.
fn run<'m>(text: &str, dialect: Dialect, m: &'m mut Meter) -> Parser<'m> {
    let mut p = Parser::new(dialect, m);
    let mut line = String::new();
    let mut rest = text;
    while !rest.is_empty() && !p.m.failed() {
        let end = rest.find(['\n', '\r']).unwrap_or(rest.len());
        line.clear();
        line.push_str(&rest[..end]);
        line.push('\n');
        p.process_line(&line);
        let skip = match rest.as_bytes().get(end) {
            Some(b'\r') if rest.as_bytes().get(end + 1) == Some(&b'\n') => 2,
            Some(_) => 1,
            None => 0,
        };
        rest = &rest[end + skip..];
    }
    while p.current != ROOT {
        p.current = p.finalize(p.current);
    }
    p
}

fn is_line_end(c: u8) -> bool {
    c == b'\n' || c == b'\r'
}

fn is_blank(s: &str) -> bool {
    s.bytes().all(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r'))
}

struct Parser<'m> {
    /// The budget; the first failure stops the line loop and the
    /// conversion, and `add_child` stops allocating.
    m: &'m mut Meter,
    /// Which syntax the block and inline phases read.
    dialect: Dialect,
    nodes: Vec<Node>,
    refs: Refs,
    /// The innermost open block.
    current: usize,
    line_number: usize,
    // Per-line state, as in cmark.
    offset: usize,
    column: usize,
    first_nonspace: usize,
    first_nonspace_column: usize,
    indent: usize,
    blank: bool,
    partially_consumed_tab: bool,
    thematic_break_kill_pos: usize,
}

impl<'m> Parser<'m> {
    fn new(dialect: Dialect, m: &'m mut Meter) -> Self {
        Parser {
            m,
            dialect,
            nodes: vec![Node {
                kind: Kind::Document,
                parent: ROOT,
                children: Vec::new(),
                open: true,
                content: String::new(),
                last_line_blank: false,
                last_line_checked: false,
                start_line: 0,
                depth: 0,
                table_tried: false,
            }],
            refs: Refs::new(),
            current: ROOT,
            line_number: 0,
            offset: 0,
            column: 0,
            first_nonspace: 0,
            first_nonspace_column: 0,
            indent: 0,
            blank: false,
            partially_consumed_tab: false,
            thematic_break_kill_pos: 0,
        }
    }

    /// `line` ends with exactly one `\n` and contains no other line ending.
    fn process_line(&mut self, line: &str) {
        self.offset = 0;
        self.column = 0;
        self.first_nonspace = 0;
        self.first_nonspace_column = 0;
        self.indent = 0;
        self.blank = false;
        self.partially_consumed_tab = false;
        self.thematic_break_kill_pos = 0;
        self.line_number += 1;

        let b = line.as_bytes();
        let Some((last_matched, all_matched)) = self.check_open_blocks(b) else {
            return;
        };
        let mut container = last_matched;
        self.open_new_blocks(&mut container, b, all_matched);
        self.add_text_to_container(container, last_matched, line);
    }

    fn advance_offset(&mut self, b: &[u8], mut count: usize, columns: bool) {
        while count > 0 {
            let c = peek(b, self.offset);
            if c == 0 {
                break;
            }
            if c == b'\t' {
                let to_tab = TAB_STOP - self.column % TAB_STOP;
                if columns {
                    self.partially_consumed_tab = to_tab > count;
                    let n = count.min(to_tab);
                    self.column += n;
                    if !self.partially_consumed_tab {
                        self.offset += 1;
                    }
                    count -= n;
                } else {
                    self.partially_consumed_tab = false;
                    self.column += to_tab;
                    self.offset += 1;
                    count -= 1;
                }
            } else {
                self.partially_consumed_tab = false;
                self.offset += 1;
                self.column += 1;
                count -= 1;
            }
        }
    }

    fn find_first_nonspace(&mut self, b: &[u8]) {
        let mut to_tab = TAB_STOP - self.column % TAB_STOP;
        if self.first_nonspace <= self.offset {
            self.first_nonspace = self.offset;
            self.first_nonspace_column = self.column;
            loop {
                match peek(b, self.first_nonspace) {
                    b' ' => {
                        self.first_nonspace += 1;
                        self.first_nonspace_column += 1;
                        to_tab -= 1;
                        if to_tab == 0 {
                            to_tab = TAB_STOP;
                        }
                    }
                    b'\t' => {
                        self.first_nonspace += 1;
                        self.first_nonspace_column += to_tab;
                        to_tab = TAB_STOP;
                    }
                    _ => break,
                }
            }
        }
        self.indent = self.first_nonspace_column.saturating_sub(self.column);
        self.blank = is_line_end(peek(b, self.first_nonspace));
    }

    /// Matches the line against the open containers. Returns the last
    /// matched block and whether all open blocks matched, or `None` when the
    /// line was consumed (closing code fence).
    fn check_open_blocks(&mut self, b: &[u8]) -> Option<(usize, bool)> {
        let mut container = ROOT;
        let mut all_matched = true;
        while let Some(&last) = self.nodes[container].children.last()
            && self.nodes[last].open
        {
            container = last;
            self.find_first_nonspace(b);
            let matched = match &self.nodes[container].kind {
                Kind::BlockQuote => self.block_quote_prefix(b),
                Kind::Item(data) => {
                    let width = data.marker_offset + data.padding;
                    if self.indent >= width {
                        self.advance_offset(b, width, true);
                        true
                    } else if self.blank && !self.nodes[container].children.is_empty() {
                        self.advance_offset(b, self.first_nonspace - self.offset, false);
                        true
                    } else {
                        false
                    }
                }
                &Kind::CodeBlock { fenced: false, .. } => {
                    if self.indent >= CODE_INDENT {
                        self.advance_offset(b, CODE_INDENT, true);
                        true
                    } else if self.blank {
                        self.advance_offset(b, self.first_nonspace - self.offset, false);
                        true
                    } else {
                        false
                    }
                }
                &Kind::CodeBlock {
                    fence_char,
                    fence_len,
                    fence_offset,
                    ..
                } => {
                    let fns = self.first_nonspace;
                    let close = if self.indent <= 3 && peek(b, fns) == fence_char {
                        scan_close_fence(b, fns)
                    } else {
                        0
                    };
                    if close >= fence_len {
                        // The closing fence ends the line's processing.
                        self.current = self.finalize(container);
                        return None;
                    }
                    let mut i = fence_offset;
                    while i > 0 && is_space_or_tab(peek(b, self.offset)) {
                        self.advance_offset(b, 1, true);
                        i -= 1;
                    }
                    true
                }
                Kind::Heading { .. } => false,
                // Any line that reads as a row of at least one cell.
                Kind::Table { .. } => scan_row(&b[self.first_nonspace..], |_, _| {}).is_some(),
                &Kind::HtmlBlock(t) => t <= 5 || !self.blank,
                Kind::Paragraph => !self.blank,
                _ => true,
            };
            if !matched {
                all_matched = false;
                container = self.nodes[container].parent;
                break;
            }
        }
        Some((container, all_matched))
    }

    fn block_quote_prefix(&mut self, b: &[u8]) -> bool {
        if self.indent <= 3 && peek(b, self.first_nonspace) == b'>' {
            self.advance_offset(b, self.indent + 1, true);
            if is_space_or_tab(peek(b, self.offset)) {
                self.advance_offset(b, 1, true);
            }
            true
        } else {
            false
        }
    }

    fn open_new_blocks(&mut self, container: &mut usize, b: &[u8], all_matched: bool) {
        let mut maybe_lazy = matches!(self.nodes[self.current].kind, Kind::Paragraph);
        while !self.m.failed()
            && !matches!(
                self.nodes[*container].kind,
                Kind::CodeBlock { .. } | Kind::HtmlBlock(_)
            )
        {
            self.find_first_nonspace(b);
            let indented = self.indent >= CODE_INDENT;
            let fns = self.first_nonspace;
            let c = peek(b, fns);
            let is_para = matches!(self.nodes[*container].kind, Kind::Paragraph);
            let room = self.nodes[*container].depth < MAX_DEPTH;

            if !indented && c == b'>' && room {
                self.advance_offset(b, fns + 1 - self.offset, false);
                if is_space_or_tab(peek(b, self.offset)) {
                    self.advance_offset(b, 1, true);
                }
                *container = self.add_child(*container, Kind::BlockQuote);
            } else if !indented && let Some((len, level)) = scan_atx_start(b, fns) {
                self.advance_offset(b, fns + len - self.offset, false);
                *container = self.add_child(
                    *container,
                    Kind::Heading {
                        level,
                        setext: false,
                    },
                );
            } else if !indented && let Some(len) = scan_open_fence(b, fns) {
                let kind = Kind::CodeBlock {
                    fenced: true,
                    fence_char: c,
                    fence_len: len,
                    fence_offset: fns - self.offset,
                    info: String::new(),
                };
                *container = self.add_child(*container, kind);
                self.advance_offset(b, fns + len - self.offset, false);
            } else if !indented && let Some(t) = html_block_start(&b[fns..b.len() - 1], is_para) {
                // The tag is part of the content: the offset stays.
                *container = self.add_child(*container, Kind::HtmlBlock(t));
            } else if !indented
                && is_para
                && let Some(level) = scan_setext(b, fns)
            {
                if self.resolve_link_defs(*container) {
                    self.nodes[*container].kind = Kind::Heading {
                        level,
                        setext: true,
                    };
                    self.advance_offset(b, b.len() - 1 - self.offset, false);
                }
            } else if !indented
                && !(is_para && !all_matched)
                && self.thematic_break_kill_pos <= fns
                && self.scan_thematic_break(b, fns)
            {
                *container = self.add_child(*container, Kind::ThematicBreak);
                self.advance_offset(b, b.len() - 1 - self.offset, false);
            } else if !indented
                && room
                && let Some((len, mut data)) = parse_list_marker(b, fns, is_para)
            {
                self.advance_offset(b, fns + len - self.offset, false);
                let saved = (self.partially_consumed_tab, self.offset, self.column);
                while self.column - saved.2 <= 5 && is_space_or_tab(peek(b, self.offset)) {
                    self.advance_offset(b, 1, true);
                }
                let spaces = self.column - saved.2;
                if !(1..5).contains(&spaces) || is_line_end(peek(b, self.offset)) {
                    // Content starts one column after the marker; the rest
                    // (indented code, or nothing) belongs to the item.
                    data.padding = len + 1;
                    (self.partially_consumed_tab, self.offset, self.column) = saved;
                    if spaces > 0 {
                        self.advance_offset(b, 1, true);
                    }
                } else {
                    data.padding = len + spaces;
                }
                data.marker_offset = self.indent;
                let continues = match &self.nodes[*container].kind {
                    Kind::List { data: d, .. } => d.marker == data.marker,
                    _ => false,
                };
                if !continues {
                    *container = self.add_child(*container, Kind::List { data, tight: false });
                }
                *container = self.add_child(*container, Kind::Item(data));
            } else if indented && !maybe_lazy && !self.blank {
                self.advance_offset(b, CODE_INDENT, true);
                let kind = Kind::CodeBlock {
                    fenced: false,
                    fence_char: 0,
                    fence_len: 0,
                    fence_offset: 0,
                    info: String::new(),
                };
                *container = self.add_child(*container, kind);
            } else if self.dialect == Dialect::Gfm && !indented && self.table_line(*container, b) {
                // The container (now) is the table; the line is consumed.
            } else {
                break;
            }

            if accepts_lines(&self.nodes[*container].kind) {
                break;
            }
            maybe_lazy = false;
        }
    }

    fn add_text_to_container(&mut self, mut container: usize, last_matched: usize, line: &str) {
        let b = line.as_bytes();
        self.find_first_nonspace(b);
        if self.blank
            && let Some(&last) = self.nodes[container].children.last()
        {
            self.nodes[last].last_line_blank = true;
        }
        // Block quote lines are never blank (they start with `>`), blanks in
        // fenced code do not make a list loose, and neither does the blank
        // rest of a list item's first line.
        let node = &self.nodes[container];
        let last_line_blank = self.blank
            && !matches!(
                node.kind,
                Kind::BlockQuote
                    | Kind::Heading { .. }
                    | Kind::ThematicBreak
                    | Kind::CodeBlock { fenced: true, .. }
            )
            && !(matches!(node.kind, Kind::Item(_))
                && node.children.is_empty()
                && node.start_line == self.line_number);
        self.nodes[container].last_line_blank = match &self.nodes[container].kind {
            // A table line is consumed whole, so the rest is blank. cmark-gfm
            // marks the row it made: the header row is the table's child
            // (the table is marked), a body row is the container (the table
            // is cleared as its parent). So a header-only table loosens the
            // list it ends an item of; one with body rows does not.
            Kind::Table { rows, .. } => rows.is_empty(),
            _ => last_line_blank,
        };
        let mut t = container;
        while t != ROOT {
            t = self.nodes[t].parent;
            self.nodes[t].last_line_blank = false;
        }

        if self.current != last_matched
            && container == last_matched
            && !self.blank
            && matches!(self.nodes[self.current].kind, Kind::Paragraph)
        {
            // Lazy continuation line.
            self.advance_offset(b, self.first_nonspace - self.offset, false);
            self.add_line(self.current, line);
            return;
        }

        while self.current != last_matched {
            self.current = self.finalize(self.current);
        }
        match self.nodes[container].kind {
            Kind::CodeBlock { .. } => self.add_line(container, line),
            Kind::HtmlBlock(t) => {
                self.add_line(container, line);
                if html_block_end(t, &b[self.first_nonspace..]) {
                    container = self.finalize(container);
                }
            }
            _ if self.blank => {}
            Kind::Heading { setext: false, .. } => {
                let text = chop_closing_hashes(&line[self.first_nonspace..]);
                if self.m.text(text.len()) {
                    self.nodes[container].content.push_str(text);
                }
            }
            Kind::Paragraph | Kind::Heading { .. } => {
                self.advance_offset(b, self.first_nonspace - self.offset, false);
                self.add_line(container, line);
            }
            _ => {
                container = self.add_child(container, Kind::Paragraph);
                self.advance_offset(b, self.first_nonspace - self.offset, false);
                self.add_line(container, line);
            }
        }
        self.current = container;
    }

    fn add_line(&mut self, n: usize, line: &str) {
        let mut to_tab = 0;
        if self.partially_consumed_tab {
            self.offset += 1;
            to_tab = TAB_STOP - self.column % TAB_STOP;
        }
        let rest = &line[self.offset..];
        // Charge the line's bytes, then reserve them, before they are
        // appended.
        if !self.m.text(to_tab + rest.len()) {
            return;
        }
        let content = &mut self.nodes[n].content;
        self.m.reserve_str(content, to_tab + rest.len());
        if self.m.failed() {
            return;
        }
        content.extend(std::iter::repeat_n(' ', to_tab));
        content.push_str(rest);
    }

    /// Appends a new block to `parent`, first closing blocks that cannot
    /// contain it.
    fn add_child(&mut self, mut parent: usize, kind: Kind) -> usize {
        // After a failure the caller may hand us a parent that cannot hold
        // the kind (its own child was never made): nothing more is made.
        if self.m.failed() {
            return parent;
        }
        while !can_contain(&self.nodes[parent].kind, &kind) {
            parent = self.finalize(parent);
        }
        // One node, charged before the arena grows. Past the budget the
        // block is not made and its parent stands in for it: the caller's
        // loop ends on the failed meter and the result is discarded.
        if !self.m.node() || self.nodes.try_reserve(1).is_err() {
            if !self.m.failed() {
                self.m.fail_allocation();
            }
            return parent;
        }
        let depth =
            self.nodes[parent].depth + u32::from(matches!(kind, Kind::BlockQuote | Kind::Item(_)));
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind,
            parent,
            children: Vec::new(),
            open: true,
            content: String::new(),
            last_line_blank: false,
            last_line_checked: false,
            start_line: self.line_number,
            depth,
            table_tried: false,
        });
        self.nodes[parent].children.push(id);
        id
    }

    /// Closes block `n`; returns its parent.
    fn finalize(&mut self, n: usize) -> usize {
        let parent = self.nodes[n].parent;
        self.nodes[n].open = false;
        match self.nodes[n].kind {
            Kind::Paragraph => {
                if !self.resolve_link_defs(n) {
                    let siblings = &mut self.nodes[parent].children;
                    if let Some(i) = siblings.iter().rposition(|&c| c == n) {
                        siblings.remove(i);
                    }
                }
            }
            Kind::CodeBlock { fenced: true, .. } => {
                let content = std::mem::take(&mut self.nodes[n].content);
                let eol = content.find('\n').unwrap_or(content.len());
                let info_line = content[..eol].trim_matches(|c: char| c.is_ascii_whitespace());
                // The info string is at most the line, already counted as
                // content; its unescaped copy is charged as it is kept.
                let info = if self.m.text(info_line.len()) {
                    unescape(info_line)
                } else {
                    String::new()
                };
                self.nodes[n].content = content[(eol + 1).min(content.len())..].to_string();
                if let Kind::CodeBlock { info: i, .. } = &mut self.nodes[n].kind {
                    *i = info;
                }
            }
            Kind::CodeBlock { fenced: false, .. } => {
                // Drop trailing blank lines; end with exactly one newline.
                let content = &mut self.nodes[n].content;
                match content.rfind(|c| !matches!(c, ' ' | '\t' | '\n' | '\r')) {
                    None => content.clear(),
                    Some(last) => {
                        let eol = content[last..]
                            .find('\n')
                            .map_or(content.len(), |i| last + i);
                        content.truncate(eol);
                    }
                }
                content.push('\n');
            }
            Kind::List { .. } => {
                let tight = self.list_is_tight(n);
                if let Kind::List { tight: t, .. } = &mut self.nodes[n].kind {
                    *t = tight;
                }
            }
            _ => {}
        }
        parent
    }

    fn list_is_tight(&mut self, list: usize) -> bool {
        let n_items = self.nodes[list].children.len();
        for i in 0..n_items {
            let item = self.nodes[list].children[i];
            let item_has_next = i + 1 < n_items;
            if self.nodes[item].last_line_blank && item_has_next {
                return false;
            }
            let n_subs = self.nodes[item].children.len();
            for j in 0..n_subs {
                let sub = self.nodes[item].children[j];
                if (item_has_next || j + 1 < n_subs) && self.ends_with_blank_line(sub) {
                    return false;
                }
            }
        }
        true
    }

    /// Whether the block ends with a blank line, looking through the last
    /// child of lists and items. Memoized, so list finalization stays linear.
    fn ends_with_blank_line(&mut self, mut n: usize) -> bool {
        loop {
            let node = &mut self.nodes[n];
            if node.last_line_checked {
                return node.last_line_blank;
            }
            node.last_line_checked = true;
            match (&node.kind, node.children.last()) {
                (Kind::List { .. } | Kind::Item(_), Some(&last)) => n = last,
                (Kind::List { .. } | Kind::Item(_), None) => return false,
                _ => return node.last_line_blank,
            }
        }
    }

    /// Takes link reference definitions off the front of paragraph `n`.
    /// Returns whether any content remains.
    fn resolve_link_defs(&mut self, n: usize) -> bool {
        let content = std::mem::take(&mut self.nodes[n].content);
        let mut pos = 0;
        while content[pos..].starts_with('[') {
            let Some((len, label, dest, title)) = parse_link_def(&content[pos..]) else {
                break;
            };
            let key = normalize_label(label);
            // A kept definition is one node plus its key, destination and
            // title (the first definition of a label wins; later ones are
            // dropped and cost nothing). Destination and title are charged
            // by their decoded length, before they are decoded.
            if !key.is_empty()
                && !self.refs.contains_key(&key)
                && self.m.node()
                && self.m.text(
                    key.len()
                        .saturating_add(decoded_len(dest, true))
                        .saturating_add(decoded_len(title, true)),
                )
            {
                let def = LinkDef {
                    dest: unescape(dest),
                    title: unescape(title),
                };
                self.refs.insert(key, def);
            }
            if self.m.failed() {
                break;
            }
            pos += len;
        }
        let rest = if pos == 0 {
            content
        } else {
            content[pos..].to_string()
        };
        let has_content = !is_blank(&rest);
        self.nodes[n].content = rest;
        has_content
    }

    /// Thematic break at `i`; on failure remembers how far the line cannot
    /// be one, so nested list markers do not rescan it.
    fn scan_thematic_break(&mut self, b: &[u8], i: usize) -> bool {
        let c = peek(b, i);
        if !matches!(c, b'*' | b'_' | b'-') {
            self.thematic_break_kill_pos = i;
            return false;
        }
        let mut count = 1;
        let mut j = i + 1;
        loop {
            let d = peek(b, j);
            if d == c {
                count += 1;
            } else if !is_space_or_tab(d) {
                if count >= 3 && is_line_end(d) {
                    return true;
                }
                self.thematic_break_kill_pos = j;
                return false;
            }
            j += 1;
        }
    }

    /// GFM: line `b` as a table line of `container`, as cmark-gfm's table
    /// extension reads it after every other block start failed: the
    /// delimiter row that turns paragraph `container` into a table, or a
    /// body row of table `container`. Whether it was one (then the line is
    /// consumed).
    fn table_line(&mut self, container: usize, b: &[u8]) -> bool {
        match self.nodes[container].kind {
            Kind::Paragraph => self.table_header(container, b),
            Kind::Table { .. } => !self.blank && self.table_row(container, b),
            _ => false,
        }
    }

    /// The line is a delimiter row and the last line of paragraph `n` a
    /// header row of as many cells: `n` becomes the table, and the lines
    /// before the header a paragraph of their own.
    fn table_header(&mut self, n: usize, b: &[u8]) -> bool {
        let line = &b[self.first_nonspace..];
        if self.nodes[n].table_tried || !scan_table_start(line) {
            return false;
        }
        let Some((_, cols)) = scan_row(line, |_, _| {}) else {
            return false;
        };
        let header = scan_row(self.nodes[n].content.as_bytes(), |_, _| {});
        let Some((start, _)) = header.filter(|&(_, h)| h == cols) else {
            self.nodes[n].table_tried = true;
            return false;
        };
        // The paragraph node becomes the table; the header row and its
        // cells are new nodes, and so is the paragraph before it.
        let prefix = usize::from(start > 0);
        if !self.m.nodes(1 + cols + prefix) {
            return false;
        }
        let mut align = Vec::new();
        let mut head = Vec::new();
        if align.try_reserve_exact(cols).is_err() || head.try_reserve_exact(cols).is_err() {
            self.m.fail_allocation();
            return false;
        }
        scan_row(line, |s, len| {
            let cell = line[s..s + len].trim_ascii();
            align.push(match (cell.first(), cell.last()) {
                (Some(b':'), Some(b':')) => Align::Center,
                (Some(b':'), _) => Align::Left,
                (_, Some(b':')) => Align::Right,
                _ => Align::None,
            });
        });
        let content = std::mem::take(&mut self.nodes[n].content);
        let m = &mut *self.m;
        scan_row(&content.as_bytes()[start..], |s, len| {
            head.push(own_cell(m, &content[start + s..start + s + len]));
        });
        if start > 0 {
            let text = unescape_pipes(m, content[..start].trim_matches(is_cmark_space));
            self.insert_paragraph_before(n, text);
        }
        if self.m.failed() {
            return false;
        }
        self.nodes[n].kind = Kind::Table {
            align,
            head,
            rows: Vec::new(),
            filled: cols,
        };
        self.advance_offset(b, b.len() - 1 - self.offset, false);
        true
    }

    /// A body row of table `n`: the header's number of cells, the missing
    /// ones empty, the extra ones dropped.
    fn table_row(&mut self, n: usize, b: &[u8]) -> bool {
        let line = &b[self.first_nonspace..];
        let Kind::Table {
            align,
            rows,
            filled,
            ..
        } = &self.nodes[n].kind
        else {
            return false;
        };
        let cols = align.len();
        let padded = cols.saturating_mul(rows.len() + 1).saturating_sub(*filled);
        if padded > MAX_PADDING_CELLS {
            return false;
        }
        let Some((_, found)) = scan_row(line, |_, _| {}) else {
            return false;
        };
        // The row and every cell, padding included, before the row exists.
        if !self.m.nodes(1 + cols) {
            return false;
        }
        let mut row = Vec::new();
        if row.try_reserve_exact(cols).is_err() {
            self.m.fail_allocation();
            return false;
        }
        let text = utf8(line);
        let m = &mut *self.m;
        scan_row(line, |s, len| {
            if row.len() < cols {
                row.push(own_cell(m, &text[s..s + len]));
            }
        });
        // Within the reserved capacity: no allocation.
        row.resize_with(cols, String::new);
        if self.m.failed() {
            return false;
        }
        let Kind::Table { rows, filled, .. } = &mut self.nodes[n].kind else {
            return false;
        };
        if rows.try_reserve(1).is_err() {
            self.m.fail_allocation();
            return false;
        }
        rows.push(row);
        *filled += found.min(cols);
        self.advance_offset(b, b.len() - 1 - self.offset, false);
        true
    }

    /// Inserts the closed paragraph `text` (its node already charged) just
    /// before block `n`. Like cmark-gfm's, it is never finalized: its link
    /// reference definitions stay text.
    fn insert_paragraph_before(&mut self, n: usize, text: String) {
        let parent = self.nodes[n].parent;
        if self.nodes.try_reserve(1).is_err() || self.nodes[parent].children.try_reserve(1).is_err()
        {
            self.m.fail_allocation();
            return;
        }
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind: Kind::Paragraph,
            parent,
            children: Vec::new(),
            open: false,
            content: text,
            last_line_blank: false,
            last_line_checked: false,
            start_line: self.nodes[n].start_line,
            depth: self.nodes[n].depth,
            table_tried: true,
        });
        let siblings = &mut self.nodes[parent].children;
        let at = siblings
            .iter()
            .rposition(|&c| c == n)
            .unwrap_or(siblings.len());
        siblings.insert(at, id);
    }

    /// The inlines of each table cell (empty cells need no parsing).
    fn cell_inlines(&mut self, cells: Vec<String>) -> Vec<Vec<Inline>> {
        let mut out = Vec::new();
        if out.try_reserve_exact(cells.len()).is_err() {
            self.m.fail_allocation();
            return out;
        }
        for c in cells {
            if self.m.failed() {
                break;
            }
            out.push(if c.is_empty() {
                Vec::new()
            } else {
                inlines(&c, &self.refs, self.dialect, self.m)
            });
        }
        out
    }

    fn convert_children(&mut self, n: usize) -> Vec<Block> {
        let children = std::mem::take(&mut self.nodes[n].children);
        let mut out = Vec::new();
        for c in children {
            if self.m.failed() {
                break;
            }
            let b = self.convert(c);
            self.m.push(&mut out, b);
        }
        out
    }

    fn convert(&mut self, n: usize) -> Block {
        let content = std::mem::take(&mut self.nodes[n].content);
        match std::mem::replace(&mut self.nodes[n].kind, Kind::Document) {
            Kind::Paragraph => {
                let raw = content.trim_end_matches([' ', '\t', '\n']);
                Block::Paragraph(inlines(raw, &self.refs, self.dialect, self.m))
            }
            Kind::Heading { level, .. } => {
                let raw = content.trim_matches([' ', '\t', '\n']);
                let content = if raw.is_empty() {
                    Vec::new()
                } else {
                    inlines(raw, &self.refs, self.dialect, self.m)
                };
                Block::Heading { level, content }
            }
            Kind::CodeBlock { info, .. } => Block::CodeBlock {
                info,
                text: content,
            },
            Kind::HtmlBlock(_) => Block::HtmlBlock(content),
            Kind::ThematicBreak => Block::ThematicBreak,
            Kind::Table {
                align, head, rows, ..
            } => {
                let head = self.cell_inlines(head);
                let mut out = Vec::new();
                for row in rows {
                    if self.m.failed() {
                        break;
                    }
                    let row = self.cell_inlines(row);
                    self.m.push(&mut out, row);
                }
                Block::Table {
                    align,
                    head,
                    rows: out,
                }
            }
            Kind::BlockQuote => Block::BlockQuote(self.convert_children(n)),
            Kind::List { data, tight } => {
                let items = std::mem::take(&mut self.nodes[n].children);
                Block::List {
                    start: match data.marker {
                        Marker::Bullet(_) => None,
                        Marker::Ordered(_) => Some(data.start),
                    },
                    tight,
                    items: {
                        let mut out = Vec::new();
                        for i in items {
                            if self.m.failed() {
                                break;
                            }
                            let blocks = self.convert_children(i);
                            self.m.push(
                                &mut out,
                                ListItem {
                                    checked: None,
                                    blocks,
                                },
                            );
                        }
                        out
                    },
                }
            }
            Kind::Document | Kind::Item(_) => {
                unreachable!("documents and items are converted by their parents")
            }
        }
    }
}

fn accepts_lines(k: &Kind) -> bool {
    matches!(
        k,
        Kind::Paragraph | Kind::Heading { .. } | Kind::CodeBlock { .. }
    )
}

fn can_contain(parent: &Kind, child: &Kind) -> bool {
    match parent {
        Kind::Document | Kind::BlockQuote | Kind::Item(_) => !matches!(child, Kind::Item(_)),
        Kind::List { .. } => matches!(child, Kind::Item(_)),
        _ => false,
    }
}

/// `#{1,6}` followed by space, tab or line end: (length incl. the spaces,
/// level).
fn scan_atx_start(b: &[u8], i: usize) -> Option<(usize, u8)> {
    let hashes = b[i..].iter().take_while(|&&c| c == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after = peek(b, i + hashes);
    if is_line_end(after) {
        return Some((hashes, hashes as u8));
    }
    if !is_space_or_tab(after) {
        return None;
    }
    let spaces = b[i + hashes..]
        .iter()
        .take_while(|&&c| is_space_or_tab(c))
        .count();
    Some((hashes + spaces, hashes as u8))
}

/// ATX heading text without the optional closing `#` sequence and the
/// surrounding whitespace.
fn chop_closing_hashes(s: &str) -> &str {
    let s = s.trim_end_matches(|c: char| c.is_ascii_whitespace());
    let t = s.trim_end_matches('#');
    if t.len() == s.len() {
        return s;
    }
    if t.is_empty() {
        return t;
    }
    if t.ends_with([' ', '\t']) {
        return t.trim_end_matches([' ', '\t']);
    }
    s
}

/// Opening code fence at `i`: its length.
fn scan_open_fence(b: &[u8], i: usize) -> Option<usize> {
    let c = peek(b, i);
    if c != b'`' && c != b'~' {
        return None;
    }
    let len = b[i..].iter().take_while(|&&d| d == c).count();
    if len < 3 {
        return None;
    }
    if c == b'`' && b[i + len..].contains(&b'`') {
        return None;
    }
    Some(len)
}

/// Closing code fence at `i`: its length, 0 if there is none.
fn scan_close_fence(b: &[u8], i: usize) -> usize {
    let c = peek(b, i);
    let len = b[i..].iter().take_while(|&&d| d == c).count();
    if len < 3 {
        return 0;
    }
    let rest = &b[i + len..];
    let ws = rest.iter().take_while(|&&d| is_space_or_tab(d)).count();
    if is_line_end(peek(rest, ws)) { len } else { 0 }
}

/// Setext underline at `i`: the heading level.
fn scan_setext(b: &[u8], i: usize) -> Option<u8> {
    let c = peek(b, i);
    let level = match c {
        b'=' => 1,
        b'-' => 2,
        _ => return None,
    };
    let rest = &b[i..];
    let run = rest.iter().take_while(|&&d| d == c).count();
    let ws = rest[run..]
        .iter()
        .take_while(|&&d| is_space_or_tab(d))
        .count();
    is_line_end(peek(rest, run + ws)).then_some(level)
}

/// List marker at `pos`: its length and list data (padding and offset are
/// filled in by the caller). A marker interrupting a paragraph must be a
/// bullet or `1`, with content after it.
fn parse_list_marker(
    b: &[u8],
    pos: usize,
    interrupts_paragraph: bool,
) -> Option<(usize, ListData)> {
    let c = peek(b, pos);
    let (end, marker, start) = if matches!(c, b'*' | b'-' | b'+') {
        (pos + 1, Marker::Bullet(c), 0)
    } else if c.is_ascii_digit() {
        let digits = b[pos..]
            .iter()
            .take(9)
            .take_while(|d| d.is_ascii_digit())
            .count();
        let start = b[pos..pos + digits]
            .iter()
            .fold(0u64, |n, d| n * 10 + u64::from(d - b'0'));
        if interrupts_paragraph && start != 1 {
            return None;
        }
        let delim = peek(b, pos + digits);
        if delim != b'.' && delim != b')' {
            return None;
        }
        (pos + digits + 1, Marker::Ordered(delim), start)
    } else {
        return None;
    };
    let next = peek(b, end);
    if !is_space_or_tab(next) && !is_line_end(next) {
        return None;
    }
    if interrupts_paragraph {
        let ws = b[end..].iter().take_while(|&&d| is_space_or_tab(d)).count();
        if is_line_end(peek(b, end + ws)) {
            return None;
        }
    }
    let data = ListData {
        marker,
        start,
        marker_offset: 0,
        padding: 0,
    };
    Some((end - pos, data))
}

/// cmark-gfm's `spacechar` in table rows.
fn is_table_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | 0x0b | 0x0c)
}

/// cmark's `cmark_isspace`, which trims cells and the paragraph before a
/// table.
fn is_cmark_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

fn table_spaces(b: &[u8], i: usize) -> usize {
    b[i.min(b.len())..]
        .iter()
        .take_while(|&&c| is_table_space(c))
        .count()
}

fn is_newline(b: &[u8], i: usize) -> bool {
    peek(b, i) == b'\n' || (peek(b, i) == b'\r' && peek(b, i + 1) == b'\n')
}

/// A GFM delimiter row (`b` from the first non-space to the line end):
/// `|? marker (| marker)* |? spaces newline`, a marker being
/// `spaces :? -+ :? spaces`.
fn scan_table_start(b: &[u8]) -> bool {
    let marker = |mut j: usize| -> Option<usize> {
        j += table_spaces(b, j);
        j += usize::from(peek(b, j) == b':');
        let dashes = b[j..].iter().take_while(|&&c| c == b'-').count();
        if dashes == 0 {
            return None;
        }
        j += dashes;
        j += usize::from(peek(b, j) == b':');
        Some(j + table_spaces(b, j))
    };
    let Some(mut i) = marker(usize::from(peek(b, 0) == b'|')) else {
        return false;
    };
    while peek(b, i) == b'|' {
        // A pipe before another marker, or the optional trailing one.
        if is_newline(b, i + 1 + table_spaces(b, i + 1)) {
            return true;
        }
        match marker(i + 1) {
            Some(j) => i = j,
            None => return false,
        }
    }
    is_newline(b, i)
}

/// Length of the table cell content at `i`: up to a pipe that is not
/// preceded by a backslash, or the line end (cmark-gfm's `table_cell`,
/// whose escapes include `\|`).
fn table_cell(b: &[u8], i: usize) -> usize {
    let mut j = i;
    while let Some(&c) = b.get(j) {
        match c {
            b'\r' | b'\n' => break,
            b'|' if j == i || b[j - 1] != b'\\' => break,
            _ => j += 1,
        }
    }
    j - i
}

/// A cell separator at `i`: the pipe and the spaces after it.
fn table_cell_end(b: &[u8], i: usize) -> usize {
    if peek(b, i) == b'|' {
        1 + table_spaces(b, i + 1)
    } else {
        0
    }
}

/// The end of a row at `i`: spaces and a line ending.
fn table_row_end(b: &[u8], i: usize) -> usize {
    let ws = table_spaces(b, i);
    match (peek(b, i + ws), peek(b, i + ws + 1)) {
        (b'\n', _) => ws + 1,
        (b'\r', b'\n') => ws + 2,
        _ => 0,
    }
}

/// cmark-gfm's `row_from_string`: `b` read as one table row, lines before
/// the last one being the paragraph above a header. Calls `cell(start,
/// len)` for every cell (with the earlier lines' cells too, so callers
/// that keep cells pass a single line). Returns the start of the row's
/// line and its number of cells, or `None` if it is no row: something
/// after the last cell, no cell, or more than [`MAX_COLUMNS`] cells on a
/// line. One pass over `b`.
fn scan_row(b: &[u8], mut cell: impl FnMut(usize, usize)) -> Option<(usize, usize)> {
    let len = b.len();
    let (mut start, mut n) = (0, 0);
    let mut offset = table_cell_end(b, 0);
    let mut more = true;
    while offset < len && more {
        let content = table_cell(b, offset);
        let pipe = table_cell_end(b, offset + content);
        if content > 0 || pipe > 0 {
            if n == MAX_COLUMNS {
                return None;
            }
            n += 1;
            cell(offset, content);
        }
        offset += content + pipe;
        if pipe == 0 {
            let end = table_row_end(b, offset);
            offset += end;
            if end > 0 && offset != len {
                // Not the last line: a paragraph line, the row starts over.
                start = offset;
                n = 0;
                offset += table_cell_end(b, offset);
            } else {
                more = false;
            }
        }
    }
    (offset == len && n > 0).then_some((start, n))
}

/// A cell's text: trimmed, pipe escapes taken off, charged before it is
/// kept.
fn own_cell(m: &mut Meter, raw: &str) -> String {
    unescape_pipes(m, raw.trim_matches(is_cmark_space))
}

/// `s` with every `\|` turned into `|` (left to right, as cmark-gfm's
/// `unescape_pipes`; also inside code spans, which is how a cell holds a
/// pipe there), charged before it is built.
fn unescape_pipes(m: &mut Meter, s: &str) -> String {
    let len = s.len() - s.matches("\\|").count();
    let mut out = String::new();
    if !m.text(len) {
        return out;
    }
    m.reserve_str(&mut out, len);
    if m.failed() {
        return out;
    }
    let mut rest = s;
    while let Some(i) = rest.find("\\|") {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
    }
    out.push_str(rest);
    out
}

const BLOCK_TAGS: &[&[u8]] = &[
    b"address",
    b"article",
    b"aside",
    b"base",
    b"basefont",
    b"blockquote",
    b"body",
    b"caption",
    b"center",
    b"col",
    b"colgroup",
    b"dd",
    b"details",
    b"dialog",
    b"dir",
    b"div",
    b"dl",
    b"dt",
    b"fieldset",
    b"figcaption",
    b"figure",
    b"footer",
    b"form",
    b"frame",
    b"frameset",
    b"h1",
    b"h2",
    b"h3",
    b"h4",
    b"h5",
    b"h6",
    b"head",
    b"header",
    b"hr",
    b"html",
    b"iframe",
    b"legend",
    b"li",
    b"link",
    b"main",
    b"menu",
    b"menuitem",
    b"nav",
    b"noframes",
    b"ol",
    b"optgroup",
    b"option",
    b"p",
    b"param",
    b"search",
    b"section",
    b"summary",
    b"table",
    b"tbody",
    b"td",
    b"tfoot",
    b"th",
    b"thead",
    b"title",
    b"tr",
    b"track",
    b"ul",
];

const RAW_TAGS: &[&[u8]] = &[b"pre", b"script", b"style", b"textarea"];

/// HTML block start condition (1-7) of line `s` (from the first non-space,
/// without the line ending). Type 7 cannot interrupt a paragraph.
fn html_block_start(s: &[u8], in_paragraph: bool) -> Option<u8> {
    if s.first() != Some(&b'<') {
        return None;
    }
    let rest = &s[1..];
    let name_len = rest
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric())
        .count();
    let name = &rest[..name_len];
    let after = rest.get(name_len).copied();
    if RAW_TAGS.iter().any(|t| t.eq_ignore_ascii_case(name))
        && matches!(after, None | Some(b' ' | b'\t' | b'>'))
    {
        return Some(1);
    }
    if rest.starts_with(b"!--") {
        return Some(2);
    }
    if rest.starts_with(b"?") {
        return Some(3);
    }
    if rest.starts_with(b"![CDATA[") {
        return Some(5);
    }
    if rest.first() == Some(&b'!') && rest.get(1).is_some_and(u8::is_ascii_alphabetic) {
        return Some(4);
    }
    let (name, after) = match rest.strip_prefix(b"/") {
        Some(r) => {
            let n = r.iter().take_while(|c| c.is_ascii_alphanumeric()).count();
            (&r[..n], &r[n..])
        }
        None => (name, &rest[name_len..]),
    };
    if BLOCK_TAGS.iter().any(|t| t.eq_ignore_ascii_case(name))
        && (after.is_empty() || matches!(after[0], b' ' | b'\t' | b'>') || after.starts_with(b"/>"))
    {
        return Some(6);
    }
    if in_paragraph {
        return None;
    }
    // Like cmark, any complete open or closing tag qualifies, including
    // `pre`/`script`/`style`/`textarea` forms that type 1 did not take
    // (such as `<pre/>` or `</pre>`).
    let len = if s.get(1) == Some(&b'/') {
        close_tag(s)?
    } else {
        open_tag(s)?
    };
    s[len..].iter().all(|&c| is_space_or_tab(c)).then_some(7)
}

/// Whether line `s` meets the end condition of HTML block type `t`.
fn html_block_end(t: u8, s: &[u8]) -> bool {
    match t {
        1 => [&b"</script>"[..], b"</pre>", b"</style>", b"</textarea>"]
            .iter()
            .any(|end| s.windows(end.len()).any(|w| w.eq_ignore_ascii_case(end))),
        2 => s.windows(3).any(|w| w == b"-->"),
        3 => s.windows(2).any(|w| w == b"?>"),
        4 => s.contains(&b'>'),
        5 => s.windows(3).any(|w| w == b"]]>"),
        _ => false,
    }
}

/// Link reference definition at the start of `s`: (consumed length
/// including the line ending, raw label, raw destination, raw title). The
/// caller decodes them once the budget has paid for the result.
fn parse_link_def(s: &str) -> Option<(usize, &str, &str, &str)> {
    let b = s.as_bytes();
    let (mut i, label) = link_label(b, 0)?;
    if is_blank(utf8(label)) || peek(b, i) != b':' {
        return None;
    }
    i = skip_spnl(b, i + 1);
    let (end, dest) = link_destination(b, i)?;
    i = end;
    let before_title = i;
    let mut title: &[u8] = b"";
    let j = skip_spnl(b, i);
    if j != before_title
        && let Some((end, t)) = link_title(b, j)
    {
        title = t;
        i = end;
    }
    let line_end = |i: usize| match peek(b, i) {
        b'\n' => Some(i + 1),
        0 => Some(i),
        _ => None,
    };
    let end = match line_end(skip_spaces(b, i)) {
        Some(e) => e,
        None if i != before_title => {
            // Trailing text after the title: the title is not part of the
            // definition, which must then end right after the destination.
            title = b"";
            line_end(skip_spaces(b, before_title))?
        }
        None => return None,
    };
    Some((end, utf8(label), utf8(dest), utf8(title)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Inline, html};
    use std::time::{Duration, Instant};

    fn parse(md: &str) -> Document {
        super::parse(
            md,
            Dialect::CommonMark,
            &mut Meter::new(crate::Limits::UNLIMITED),
        )
        .unwrap()
    }

    fn html(md: &str) -> String {
        html::write(&parse(md))
    }

    fn refs(md: &str) -> Refs {
        let mut m = Meter::new(crate::Limits::UNLIMITED);
        run(md, Dialect::CommonMark, &mut m).refs
    }

    fn timed(input: &str) -> (Document, Duration) {
        let t = Instant::now();
        let d = parse(input);
        (d, t.elapsed())
    }

    fn quote_depth(blocks: &[Block]) -> usize {
        let mut depth = 0;
        let mut cur = blocks;
        while let [Block::BlockQuote(inner)] = cur {
            depth += 1;
            cur = inner;
        }
        depth
    }

    #[test]
    fn deep_blockquote_no_overflow() {
        let input = ">".repeat(10_000);
        let t = Instant::now();
        let d = super::super::read(input.as_bytes()).unwrap();
        let took = t.elapsed();
        assert_eq!(quote_depth(&d.blocks), MAX_DEPTH as usize);
        assert!(took < Duration::from_millis(500), "{took:?}");
        // The writers recurse over the capped tree without overflow.
        let out = html::write(&d);
        assert!(out.contains(&"&gt;".repeat(10_000 - MAX_DEPTH as usize)));
        let _ = super::super::write(&d);
        let _ = crate::txt::write(&d);
    }

    #[test]
    fn nesting_cap_leaves_extra_markers_as_text() {
        let cap = MAX_DEPTH as usize;
        let d = parse(&format!("{}- x", "> ".repeat(cap)));
        let mut cur = &d.blocks[..];
        for _ in 0..cap {
            let [Block::BlockQuote(inner)] = cur else {
                panic!("expected a quote")
            };
            cur = inner;
        }
        assert_eq!(cur, [Block::Paragraph(vec![Inline::Text("- x".into())])]);

        let d = parse(&format!("{}x", "1. ".repeat(cap + 3)));
        let mut depth = 0;
        let mut cur = &d.blocks[..];
        while let [Block::List { items, .. }] = cur {
            depth += 1;
            cur = &items[0].blocks;
        }
        assert_eq!(depth, cap);
        let _ = html::write(&d);
        let _ = super::super::write(&d);
        let _ = crate::txt::write(&d);
    }

    #[test]
    fn long_inputs_are_linear() {
        let lines = "a line of text\n".repeat(100_000);
        let (_, a) = timed(&lines);
        let items = "- item\n\n".repeat(100_000);
        let (_, b) = timed(&items);
        let nested: String = (0..100_000)
            .map(|i| format!("{}- x\n", "  ".repeat(i % 200)))
            .collect();
        let (_, c) = timed(&nested);
        let quotes = format!("{}\n", "> ".repeat(300)).repeat(10_000);
        let (_, d) = timed(&quotes);
        let fences = format!("{}a\n", "`".repeat(3)).repeat(100_000);
        let (_, e) = timed(&fences);
        let breaks = format!("{}x\n", "- ".repeat(5_000)).repeat(20);
        let (_, f) = timed(&breaks);
        // Quadratic behaviour on these inputs would take minutes.
        let limit = Duration::from_secs(if cfg!(debug_assertions) { 10 } else { 1 });
        for t in [a, b, c, d, e, f] {
            assert!(t < limit, "{t:?}");
        }
    }

    #[test]
    fn line_endings_and_final_newline() {
        assert_eq!(html("a\r\nb\rc"), "<p>a\nb\nc</p>\n");
        assert_eq!(html("# h\r\n"), "<h1>h</h1>\n");
        assert_eq!(html(""), "");
        assert_eq!(html("\n\n"), "");
    }

    #[test]
    fn raw_paragraph_text_is_the_seam() {
        let d = parse("  a  \n   b *c*  \n");
        assert_eq!(
            d.blocks,
            [Block::Paragraph(vec![
                Inline::Text("a".into()),
                Inline::HardBreak,
                Inline::Text("b ".into()),
                Inline::Emph(vec![Inline::Text("c".into())]),
            ])]
        );
        let d = parse("Foo\n  bar\n===");
        assert_eq!(
            d.blocks,
            [Block::Heading {
                level: 1,
                content: vec![
                    Inline::Text("Foo".into()),
                    Inline::SoftBreak,
                    Inline::Text("bar".into())
                ]
            }]
        );
        let d = parse("# \n");
        assert_eq!(
            d.blocks,
            [Block::Heading {
                level: 1,
                content: vec![]
            }]
        );
    }

    #[test]
    fn link_defs_collected_and_normalized() {
        let r = refs(
            "[Foo  Bar]: /u \"t\"\n[foo bar]: /second\n[ẞ]:\n<a b>\n'x\\'y'\n[b]: /v\n'no' trailing\n",
        );
        let get = |k: &str| r.get(k).map(|d| (d.dest.as_str(), d.title.as_str()));
        assert_eq!(get(&normalize_label("FOO\tbar")), Some(("/u", "t")));
        assert_eq!(get(&normalize_label("SS")), Some(("a b", "x'y")));
        assert_eq!(get(&normalize_label("b")), Some(("/v", "")));
        assert_eq!(r.len(), 3);
    }

    /// The definitions of the spec's "Link reference definitions" examples
    /// whose HTML needs the inline parser (192-218).
    #[test]
    fn spec_link_defs_collected() {
        let one = |md: &str, label: &str| {
            let r = refs(md);
            let d = r.get(&normalize_label(label)).expect(md);
            (r.len(), d.dest.clone(), d.title.clone())
        };
        let def = |n: usize, dest: &str, title: &str| (n, dest.to_string(), title.to_string());
        assert_eq!(
            one(
                "   [foo]: \n      /url  \n           'the title'  \n\n[foo]\n",
                "foo"
            ),
            def(1, "/url", "the title")
        );
        assert_eq!(
            one(
                "[Foo*bar\\]]:my_(url) 'title (with parens)'\n",
                "foo*bar\\]"
            ),
            def(1, "my_(url)", "title (with parens)")
        );
        assert_eq!(
            one("[Foo bar]:\n<my url>\n'title'\n", "foo BAR"),
            def(1, "my url", "title")
        );
        assert_eq!(
            one("[foo]: /url '\ntitle\nline1\nline2\n'\n", "foo"),
            def(1, "/url", "\ntitle\nline1\nline2\n")
        );
        assert_eq!(one("[foo]:\n/url\n", "foo"), def(1, "/url", ""));
        assert_eq!(one("[foo]: <>\n", "foo"), def(1, "", ""));
        assert!(refs("[foo]: <bar>(baz)\n").is_empty());
        assert_eq!(
            one("[foo]: /url\\bar\\*baz \"foo\\\"bar\\baz\"\n", "foo"),
            def(1, "/url\\bar*baz", "foo\"bar\\baz")
        );
        assert_eq!(
            one("[foo]\n\n[foo]: first\n[foo]: second\n", "foo"),
            def(1, "first", "")
        );
        assert_eq!(one("[ΑΓΩ]: /φου\n", "αγω"), def(1, "/φου", ""));
        assert_eq!(
            one("# [Foo]\n[foo]: /url\n> bar\n", "foo"),
            def(1, "/url", "")
        );
        assert_eq!(one("[foo]: /url\nbar\n===\n", "foo"), def(1, "/url", ""));
        let r = refs(
            "[foo]: /foo-url \"foo\"\n[bar]: /bar-url\n  \"bar\"\n[baz]: /baz-url\n\n[foo],\n",
        );
        assert_eq!(r.len(), 3);
        assert_eq!(r["BAR"].title, "bar");
        assert_eq!(one("[foo]\n\n> [foo]: /url\n", "foo"), def(1, "/url", ""));
    }

    #[test]
    fn link_def_rejections_leave_a_paragraph() {
        for s in [
            "[]: /u",
            "[ ]: /u",
            "[a]:",
            "[a]: <b",
            "[a] : /u",
            "[a]: /u 'x' y",
            "[a[b]: /u",
            "[a]: (",
        ] {
            assert!(refs(s).is_empty(), "{s}");
            assert!(html(s).starts_with("<p>"), "{s}");
        }
        assert!(refs(&format!("[{}]: /u", "x".repeat(1000))).is_empty());
        assert_eq!(refs(&format!("[{}]: /u", "é".repeat(999))).len(), 1);
    }

    #[test]
    fn defs_only_paragraph_disappears() {
        assert_eq!(html("[a]: /u\n\nx"), "<p>x</p>\n");
        assert_eq!(html("[a]: /u\n==="), "<p>===</p>\n");
        assert_eq!(html("[a]: /u\nb\n==="), "<h1>b</h1>\n");
    }

    #[test]
    fn fence_info_backslash_unescaped() {
        assert_eq!(
            html("``` a\\*b c\nx\n```"),
            "<pre><code class=\"language-a*b\">x\n</code></pre>\n"
        );
    }

    #[test]
    fn html_block_starts() {
        assert_eq!(html_block_start(b"<x-y z>", false), Some(7));
        assert_eq!(html_block_start(b"<x-y z>", true), None);
        assert_eq!(html_block_start(b"</pre>", false), Some(7));
        assert_eq!(html_block_start(b"<pre/>", false), Some(7));
        assert_eq!(html_block_start(b"<textarea/>", true), None);
        assert_eq!(html_block_start(b"<pre x>", true), Some(1));
        assert_eq!(html_block_start(b"<DIV/>", true), Some(6));
        assert_eq!(html_block_start(b"<divx>", false), Some(7));
        assert_eq!(html_block_start(b"<a> b", false), None);
    }

    fn gfm_html(md: &str) -> String {
        let mut m = Meter::new(crate::Limits::UNLIMITED);
        html::write(&super::parse(md, Dialect::Gfm, &mut m).unwrap())
    }

    /// `<table>` HTML of a header row and body rows, no alignment.
    fn table_html(head: &[&str], rows: &[&[&str]]) -> String {
        let row = |tag: &str, cells: &[&str]| {
            let cells: String = cells
                .iter()
                .map(|c| format!("<{tag}>{c}</{tag}>\n"))
                .collect();
            format!("<tr>\n{cells}</tr>\n")
        };
        let mut out = format!("<table>\n<thead>\n{}</thead>\n", row("th", head));
        if !rows.is_empty() {
            out.push_str("<tbody>\n");
            for r in rows {
                out.push_str(&row("td", r));
            }
            out.push_str("</tbody>\n");
        }
        out.push_str("</table>\n");
        out
    }

    #[test]
    fn pipes_without_delimiter_row_stay_a_paragraph() {
        for md in [
            "a | b\nc | d\n",
            "| a | b |\n| c | d |\n",
            "| a |\n|\n",
            "a|b\n\n| - |\n",
        ] {
            assert_eq!(gfm_html(md), html(md), "{md:?}");
            assert!(!gfm_html(md).contains("<table>"), "{md:?}");
        }
    }

    #[test]
    fn delimiter_row_column_mismatch_is_not_a_table() {
        assert_eq!(
            gfm_html("| a | b |\n| - |\n| c |\n"),
            "<p>| a | b |\n| - |\n| c |</p>\n"
        );
        assert_eq!(gfm_html("| a |\n|-|-|\n"), "<p>| a |\n|-|-|</p>\n");
        // A paragraph is checked once, at its first delimiter-like line (as
        // cmark-gfm does): the third line would match the second one, but
        // the paragraph stays one. Rescanning it on every line would also be
        // quadratic.
        assert_eq!(gfm_html("a\n-:|-:\n-:|-:\n"), "<p>a\n-:|-:\n-:|-:</p>\n");
    }

    #[test]
    fn table_after_multiline_paragraph() {
        assert_eq!(
            gfm_html("x\ny\n| a | b |\n| - | - |\n| c | d |\n"),
            format!("<p>x\ny</p>\n{}", table_html(&["a", "b"], &[&["c", "d"]]))
        );
        // As in cmark-gfm, the lines before the header become a paragraph
        // as they are: pipe escapes are taken off and a link reference
        // definition among them is not one.
        assert_eq!(
            gfm_html("[r]: /u\n`p\\|q`\na\n| h |\n| - |\n\n[r]\n"),
            format!(
                "<p>[r]: /u\n<code>p|q</code>\na</p>\n{}<p>[r]</p>\n",
                table_html(&["h"], &[])
            )
        );
    }

    #[test]
    fn setext_underline_is_not_a_delimiter_row() {
        assert_eq!(gfm_html("| a |\n---\n"), "<h2>| a |</h2>\n");
        assert_eq!(gfm_html("a\n===\n"), "<h1>a</h1>\n");
        // Not a setext underline: a one-column table.
        assert_eq!(
            gfm_html("a\n:--\nb\n"),
            "<table>\n<thead>\n<tr>\n<th align=\"left\">a</th>\n</tr>\n</thead>\n\
<tbody>\n<tr>\n<td align=\"left\">b</td>\n</tr>\n</tbody>\n</table>\n"
        );
        // A list item and a thematic break come before a delimiter row.
        assert_eq!(
            gfm_html("a\n- | -\n"),
            "<p>a</p>\n<ul>\n<li>| -</li>\n</ul>\n"
        );
        assert_eq!(gfm_html("a\n***\n"), "<p>a</p>\n<hr />\n");
    }

    #[test]
    fn escaped_pipe_in_code_span() {
        assert_eq!(
            gfm_html("| `a\\|b` | c\\|d | e\\\\|f |\n|-|-|-|\n"),
            table_html(&["<code>a|b</code>", "c|d", "e|f"], &[])
        );
        // Outside a table the code span keeps its backslash.
        assert_eq!(gfm_html("`a\\|b`\n"), "<p><code>a\\|b</code></p>\n");
    }

    #[test]
    fn table_ends_at_blank_line_or_block_start() {
        let t = table_html(&["a"], &[&["b"]]);
        let head = "| a |\n| - |\n| b |\n";
        for (next, after) in [
            ("\nc\n", "<p>c</p>\n"),
            ("> c\n", "<blockquote>\n<p>c</p>\n</blockquote>\n"),
            ("# c\n", "<h1>c</h1>\n"),
            ("```\nc\n```\n", "<pre><code>c\n</code></pre>\n"),
            ("- c\n", "<ul>\n<li>c</li>\n</ul>\n"),
            ("***\n", "<hr />\n"),
            ("    c\n", "<pre><code>c\n</code></pre>\n"),
            ("<div>\n", "<div>\n"),
        ] {
            assert_eq!(
                gfm_html(&format!("{head}{next}")),
                format!("{t}{after}"),
                "{next:?}"
            );
        }
        // Any other line is a row; short rows are padded, long ones cut.
        assert_eq!(
            gfm_html("| a | b |\n|-|-|\nc\n| d | e | f |\n"),
            table_html(&["a", "b"], &[&["c", ""], &["d", "e"]])
        );
    }

    #[test]
    fn table_in_list_and_quote() {
        let t = table_html(&["a"], &[&["b"]]);
        assert_eq!(
            gfm_html("- | a |\n  | - |\n  | b |\n- c\n"),
            format!("<ul>\n<li>\n{t}</li>\n<li>c</li>\n</ul>\n")
        );
        assert_eq!(
            gfm_html("> | a |\n> | - |\n> | b |\nc\n"),
            format!("<blockquote>\n{t}</blockquote>\n<p>c</p>\n")
        );
        // A delimiter row cannot be a lazy continuation line.
        assert_eq!(
            gfm_html("> | a |\n| - |\n"),
            "<blockquote>\n<p>| a |\n| - |</p>\n</blockquote>\n"
        );
        // As in cmark-gfm, a header-only table is followed by its own
        // (blank) rest of line, so it loosens the list it is in.
        assert_eq!(
            gfm_html("- | a |\n  | - |\n- c\n"),
            format!(
                "<ul>\n<li>\n{}</li>\n<li>\n<p>c</p>\n</li>\n</ul>\n",
                table_html(&["a"], &[])
            )
        );
    }

    #[test]
    fn cm_dialect_reads_no_tables() {
        let md = "| a | b |\n| - | - |\n| c | d |\n";
        let d = parse(md);
        assert_eq!(d, super::super::read(md.as_bytes()).unwrap());
        assert_eq!(html::write(&d), "<p>| a | b |\n| - | - |\n| c | d |</p>\n");
    }

    #[test]
    fn table_nodes_charge_the_budget() {
        let read = |md: &str, nodes: usize| {
            let limits = crate::Limits {
                nodes,
                ..crate::Limits::DEFAULT
            };
            super::super::read_limited_with(md.as_bytes(), limits, Dialect::Gfm)
        };
        // The least node budget that reads `md`; one less fails on nodes.
        let need = |md: &str| {
            let n = (1..).find(|&n| read(md, n).is_ok()).unwrap();
            assert_eq!(
                read(md, n - 1),
                Err(crate::ReadError::Limit(crate::LimitError::Exceeded(
                    "nodes"
                )))
            );
            n
        };
        let head = format!("{}|\n", "|a".repeat(100));
        let delim = format!("{}|\n", "|-".repeat(100));
        // The table, its header row and 100 cells, each body row and its
        // 100 cells (padding included), and one text node per non-empty
        // cell.
        let md = format!("{head}{delim}x\n");
        assert_eq!(need(&md), 1 + (1 + 100) + (1 + 100) + 101);
        assert_eq!(need(&format!("{md}y\n")), need(&md) + 1 + 100 + 1);
        // A paragraph before the table is one more node.
        assert_eq!(need(&format!("p\n{md}")), need(&md) + 2);
    }

    #[test]
    fn table_cells_are_bounded() {
        let d = |md: &str| {
            let mut m = Meter::new(crate::Limits::UNLIMITED);
            super::parse(md, Dialect::Gfm, &mut m).unwrap()
        };
        let table = |b: &Block| match b {
            Block::Table { head, rows, .. } => Some((head.len(), rows.len())),
            _ => None,
        };
        // Padding a short row is capped as in cmark-gfm: a header of 1000
        // columns over 1000 one-cell rows would be a million cells.
        let md = format!(
            "{}|\n{}|\n{}",
            "|a".repeat(1000),
            "|-".repeat(1000),
            "x\n".repeat(1000)
        );
        let doc = d(&md);
        let (cols, rows) = table(&doc.blocks[0]).unwrap();
        assert_eq!(cols, 1000);
        // Rows are taken while at most MAX_PADDING_CELLS cells were padded.
        assert_eq!(rows, MAX_PADDING_CELLS / 999 + 1);
        assert!(matches!(doc.blocks[1], Block::Paragraph(_)));
        // A row of more cells than a u16 counts is not a row.
        let wide = |n: usize| d(&format!("{}\n{}\n", "a|".repeat(n), "-|".repeat(n)));
        assert_eq!(table(&wide(MAX_COLUMNS + 1).blocks[0]), None);
        assert_eq!(table(&wide(MAX_COLUMNS).blocks[0]), Some((MAX_COLUMNS, 0)));
    }
}
