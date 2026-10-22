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
use super::{LinkDef, Refs, inlines, normalize_label, unescape};
use crate::{Block, Document};

const TAB_STOP: usize = 4;
const CODE_INDENT: usize = 4;
/// Maximum number of nested block quotes and list items.
pub(super) const MAX_DEPTH: u32 = 256;
const ROOT: usize = 0;

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
}

/// Parses `text` (valid UTF-8, NUL already replaced) into a document.
pub(super) fn parse(text: &str) -> Document {
    let mut p = run(text);
    let blocks = p.convert_children(ROOT);
    Document { blocks }
}

/// The block phase alone: every block closed, definitions collected.
fn run(text: &str) -> Parser {
    let mut p = Parser::new();
    let mut line = String::new();
    let mut rest = text;
    while !rest.is_empty() {
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

struct Parser {
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

impl Parser {
    fn new() -> Self {
        Parser {
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
        while !matches!(
            self.nodes[*container].kind,
            Kind::CodeBlock { .. } | Kind::HtmlBlock(_)
        ) {
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
        self.nodes[container].last_line_blank = last_line_blank;
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
                self.nodes[container].content.push_str(text);
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
        let content = &mut self.nodes[n].content;
        if self.partially_consumed_tab {
            self.offset += 1;
            let to_tab = TAB_STOP - self.column % TAB_STOP;
            content.extend(std::iter::repeat_n(' ', to_tab));
        }
        content.push_str(&line[self.offset..]);
    }

    /// Appends a new block to `parent`, first closing blocks that cannot
    /// contain it.
    fn add_child(&mut self, mut parent: usize, kind: Kind) -> usize {
        while !can_contain(&self.nodes[parent].kind, &kind) {
            parent = self.finalize(parent);
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
                let info = unescape(info_line);
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
            let Some((len, label, def)) = parse_link_def(&content[pos..]) else {
                break;
            };
            let key = normalize_label(label);
            if !key.is_empty() {
                self.refs.entry(key).or_insert(def);
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

    fn convert_children(&mut self, n: usize) -> Vec<Block> {
        let children = std::mem::take(&mut self.nodes[n].children);
        children.into_iter().map(|c| self.convert(c)).collect()
    }

    fn convert(&mut self, n: usize) -> Block {
        let content = std::mem::take(&mut self.nodes[n].content);
        match std::mem::replace(&mut self.nodes[n].kind, Kind::Document) {
            Kind::Paragraph => {
                let raw = content.trim_end_matches([' ', '\t', '\n']);
                Block::Paragraph(inlines(raw, &self.refs))
            }
            Kind::Heading { level, .. } => {
                let raw = content.trim_matches([' ', '\t', '\n']);
                let content = if raw.is_empty() {
                    Vec::new()
                } else {
                    inlines(raw, &self.refs)
                };
                Block::Heading { level, content }
            }
            Kind::CodeBlock { info, .. } => Block::CodeBlock {
                info,
                text: content,
            },
            Kind::HtmlBlock(_) => Block::HtmlBlock(content),
            Kind::ThematicBreak => Block::ThematicBreak,
            Kind::BlockQuote => Block::BlockQuote(self.convert_children(n)),
            Kind::List { data, tight } => {
                let items = std::mem::take(&mut self.nodes[n].children);
                Block::List {
                    start: match data.marker {
                        Marker::Bullet(_) => None,
                        Marker::Ordered(_) => Some(data.start),
                    },
                    tight,
                    items: items
                        .into_iter()
                        .map(|i| self.convert_children(i))
                        .collect(),
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
/// including the line ending, raw label, definition).
fn parse_link_def(s: &str) -> Option<(usize, &str, LinkDef)> {
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
    let def = LinkDef {
        dest: unescape(utf8(dest)),
        title: unescape(utf8(title)),
    };
    Some((end, utf8(label), def))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Inline, html};
    use std::time::{Duration, Instant};

    fn html(md: &str) -> String {
        html::write(&parse(md))
    }

    fn refs(md: &str) -> Refs {
        run(md).refs
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
            cur = &items[0];
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
}
