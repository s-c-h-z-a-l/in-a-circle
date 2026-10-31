//! CommonMark 0.31.2 inlines: the second phase of the spec's "Appendix: A
//! parsing strategy", following cmark's `inlines.c` so the results match the
//! reference implementation.
//!
//! One left-to-right pass builds a list of nodes in an arena. Emphasis
//! delimiter runs and link brackets are kept on stacks; a `]` that closes a
//! link or image moves the nodes after its opener into the new node, and
//! emphasis is resolved by the spec's "process emphasis" with the
//! `openers_bottom` optimization. Every step is amortized O(1) per input
//! byte, scans that could repeat (backtick closers, HTML comment/PI/CDATA/
//! declaration ends) are memoized, and inactive link openers are tracked by
//! a counter instead of being walked, so the pass is linear.
//!
//! Nesting (emphasis, links, images) deeper than [`MAX_DEPTH`] is flattened
//! when the arena becomes the [`Inline`] tree: those wrappers are dropped
//! and their content is kept. This bounds the recursion of every writer.

use super::scan::{close_tag, link_destination, link_label, link_title, open_tag, utf8};
use super::{Refs, char_ref, decode, decoded_len, normalize_label, punct::PUNCT, unescape};
use crate::budget::Meter;
use crate::{Dialect, Inline};
use std::collections::HashMap;

/// Deepest inline nesting kept in the tree.
pub(super) const MAX_DEPTH: usize = 256;

/// Longest link label, in characters (spec: link label).
const MAX_LABEL: usize = 999;

const NONE: usize = usize::MAX;

/// The arena's first node: the head of the top-level list, never output.
const HEAD: usize = 0;

/// Parses a paragraph's or heading's raw content (see `md::inlines`).
///
/// Every node, every owned string and every copy of a reference's
/// destination and title is charged to `m` before it is made; once `m`
/// has failed the pass stops and the (partial) result is discarded.
pub(super) fn parse(raw: &str, refs: &Refs, dialect: Dialect, m: &mut Meter) -> Vec<Inline> {
    let mut p = Parser {
        m,
        dialect,
        s: raw,
        b: raw.as_bytes(),
        pos: 0,
        refs,
        nodes: vec![Node::new(Kind::Text(String::new()))],
        tail: HEAD,
        delims: Vec::new(),
        last_delim: NONE,
        brackets: Vec::new(),
        brackets_made: 0,
        link_floor: 0,
        backticks: None,
        ends: [Memo::default(); 4],
    };
    p.run();
    if p.m.failed() {
        return Vec::new();
    }
    p.process_emphasis(0);
    let mut out = Vec::new();
    let first = p.nodes[HEAD].next;
    p.convert(first, 0, &mut out);
    out
}

enum Kind {
    Text(String),
    Code(String),
    Html(String),
    SoftBreak,
    HardBreak,
    Emph,
    Strong,
    Link { dest: String, title: String },
    Image { dest: String, title: String },
}

/// Arena node. Siblings form a doubly linked list; containers (emphasis,
/// links, images) own the sublist `first..=last` of their children.
struct Node {
    kind: Kind,
    prev: usize,
    next: usize,
    first: usize,
    last: usize,
}

impl Node {
    fn new(kind: Kind) -> Self {
        Node {
            kind,
            prev: NONE,
            next: NONE,
            first: NONE,
            last: NONE,
        }
    }
}

/// A `*` or `_` run that can open or close emphasis. Delimiters live in an
/// arena (index = creation order, the spec's stack position) and the live
/// ones form a doubly linked list.
struct Delim {
    /// The run's text node; its length is the number of delimiters left.
    node: usize,
    ch: u8,
    /// Length of the original run (for the rule of 3).
    len: usize,
    can_open: bool,
    can_close: bool,
    prev: usize,
    next: usize,
}

/// A `[` or `![` that may start a link or image.
struct Bracket {
    /// The `[`/`![` text node.
    node: usize,
    /// Byte offset just after the bracket (start of the link text).
    text_start: usize,
    image: bool,
    /// Creation number; non-image openers below `link_floor` are inactive.
    id: usize,
    /// Another bracket was opened after this one (the link text then
    /// cannot be a reference label).
    bracket_after: bool,
    /// Delimiters at or above this arena index belong to the link text.
    delim_bottom: usize,
}

/// Memoized forward search for a fixed terminator: the first occurrence at
/// or after `from` (or none at all), valid for every later start up to it.
#[derive(Clone, Copy, Default)]
struct Memo {
    searched: Option<(usize, Option<usize>)>,
}

/// Terminators of raw inline HTML: comment, processing instruction, CDATA,
/// declaration.
const ENDS: [&[u8]; 4] = [b"-->", b"?>", b"]]>", b">"];

struct Parser<'a> {
    m: &'a mut Meter,
    /// Which syntax is read.
    #[allow(dead_code)] // read once the GFM extensions are parsed
    dialect: Dialect,
    s: &'a str,
    b: &'a [u8],
    pos: usize,
    refs: &'a Refs,
    nodes: Vec<Node>,
    /// Last node of the top-level list.
    tail: usize,
    delims: Vec<Delim>,
    last_delim: usize,
    brackets: Vec<Bracket>,
    brackets_made: usize,
    link_floor: usize,
    /// Maximal backtick runs: length -> sorted start offsets. Built on the
    /// first backtick.
    backticks: Option<HashMap<usize, Vec<usize>>>,
    ends: [Memo; 4],
}

fn is_special(c: u8) -> bool {
    matches!(
        c,
        b'\n' | b'\\' | b'`' | b'&' | b'<' | b'[' | b']' | b'!' | b'*' | b'_'
    )
}

/// Unicode whitespace as CommonMark defines it: Zs plus tab, LF, FF, CR.
fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\x0c' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// Unicode punctuation as CommonMark defines it: general categories P and S.
fn is_punct(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_punctuation();
    }
    PUNCT
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                std::cmp::Ordering::Less
            } else if lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Scheme `:` then no spaces, controls or `<>`, then `>` (after the `<`
/// at `i - 1`): the URI's length without the `>`.
fn scan_uri(b: &[u8], i: usize) -> Option<usize> {
    let scheme = b[i..]
        .iter()
        .take(33)
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'.' | b'-'))
        .count();
    if !(2..=32).contains(&scheme)
        || !b[i].is_ascii_alphabetic()
        || b.get(i + scheme) != Some(&b':')
    {
        return None;
    }
    let rest = b[i + scheme + 1..]
        .iter()
        .take_while(|&&c| c > b' ' && c != b'<' && c != b'>' && c != 0x7f)
        .count();
    let end = i + scheme + 1 + rest;
    (b.get(end) == Some(&b'>')).then_some(end - i)
}

/// Email address then `>`: the address's length.
fn scan_email(b: &[u8], i: usize) -> Option<usize> {
    let local = b[i..]
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric() || b".!#$%&'*+/=?^_`{|}~-".contains(c))
        .count();
    if local == 0 || b.get(i + local) != Some(&b'@') {
        return None;
    }
    let mut j = i + local + 1;
    loop {
        let n = b[j..]
            .iter()
            .take(64)
            .take_while(|c| c.is_ascii_alphanumeric() || **c == b'-')
            .count();
        if n == 0 || n > 63 || b[j] == b'-' || b[j + n - 1] == b'-' {
            return None;
        }
        j += n;
        match b.get(j) {
            Some(b'.') => j += 1,
            Some(b'>') => return Some(j - i),
            _ => return None,
        }
    }
}

impl Parser<'_> {
    fn run(&mut self) {
        while self.pos < self.b.len() && !self.m.failed() {
            let c = self.b[self.pos];
            match c {
                b'\n' => self.newline(),
                b'\\' => self.backslash(),
                b'`' => self.code_span(),
                b'&' => self.entity(),
                b'<' => self.pointy(),
                b'[' => {
                    self.pos += 1;
                    self.push_bracket(false);
                }
                b'!' if self.b.get(self.pos + 1) == Some(&b'[') => {
                    self.pos += 2;
                    self.push_bracket(true);
                }
                b']' => self.close_bracket(),
                b'*' | b'_' => self.delim_run(c),
                _ => self.text_run(),
            }
        }
    }

    // ---- node list ----

    /// Appends a node. One node is charged and the arena reserved first;
    /// without room the node is not made and the tail stands in for it
    /// (the pass ends at its next step on the failed meter).
    fn add(&mut self, kind: Kind) -> usize {
        if !self.room() {
            return self.tail;
        }
        let n = self.nodes.len();
        let mut node = Node::new(kind);
        node.prev = self.tail;
        self.nodes.push(node);
        self.nodes[self.tail].next = n;
        self.tail = n;
        n
    }

    /// Charges one node and reserves its arena slot.
    fn room(&mut self) -> bool {
        if !self.m.node() {
            return false;
        }
        if self.nodes.try_reserve(1).is_err() {
            self.m.fail_allocation();
            return false;
        }
        true
    }

    fn text(&mut self, s: &str) -> usize {
        let t = self.m.own(s);
        self.add(Kind::Text(t))
    }

    /// A text node of an already built string (small: one character, an
    /// entity's replacement); its bytes are charged.
    fn text_owned(&mut self, t: String) -> usize {
        if !self.m.text(t.len()) {
            return self.tail;
        }
        self.add(Kind::Text(t))
    }

    fn unlink(&mut self, n: usize) {
        let (p, x) = (self.nodes[n].prev, self.nodes[n].next);
        self.nodes[p].next = x;
        if x == NONE {
            self.tail = p;
        } else {
            self.nodes[x].prev = p;
        }
    }

    /// Makes the nodes strictly between `a` and `b` (or after `a` to the
    /// end when `b == NONE`) the children of a new `kind` node placed there.
    fn wrap(&mut self, a: usize, b: usize, kind: Kind) -> usize {
        if !self.room() {
            return a;
        }
        let first = self.nodes[a].next;
        let last = if b == NONE {
            self.tail
        } else {
            self.nodes[b].prev
        };
        let w = self.nodes.len();
        let mut node = Node::new(kind);
        if first != b {
            node.first = first;
            node.last = last;
            self.nodes[first].prev = NONE;
            self.nodes[last].next = NONE;
        }
        node.prev = a;
        node.next = b;
        self.nodes.push(node);
        self.nodes[a].next = w;
        if b == NONE {
            self.tail = w;
        } else {
            self.nodes[b].prev = w;
        }
        w
    }

    // ---- simple inlines ----

    fn text_run(&mut self) {
        let start = self.pos;
        let mut end = start + 1;
        while let Some(&c) = self.b.get(end) {
            if is_special(c) && !(c == b'!' && self.b.get(end + 1) != Some(&b'[')) {
                break;
            }
            end += 1;
        }
        self.pos = end;
        let mut t = &self.s[start..end];
        if self.b.get(end) == Some(&b'\n') {
            t = t.trim_end_matches([' ', '\t']);
        }
        if !t.is_empty() {
            self.text(t);
        }
    }

    fn newline(&mut self) {
        let nl = self.pos;
        self.pos += 1;
        self.skip_spaces();
        let hard = nl >= 2 && self.b[nl - 1] == b' ' && self.b[nl - 2] == b' ';
        self.add(if hard {
            Kind::HardBreak
        } else {
            Kind::SoftBreak
        });
    }

    fn skip_spaces(&mut self) {
        while matches!(self.b.get(self.pos), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
    }

    fn backslash(&mut self) {
        match self.b.get(self.pos + 1) {
            Some(&c) if c.is_ascii_punctuation() => {
                self.pos += 2;
                self.text_owned(char::from(c).to_string());
            }
            Some(b'\n') => {
                self.pos += 2;
                self.skip_spaces();
                self.add(Kind::HardBreak);
            }
            _ => {
                self.pos += 1;
                self.text("\\");
            }
        }
    }

    fn entity(&mut self) {
        let mut out = String::new();
        match char_ref(self.b, self.pos, &mut out) {
            Some(end) => {
                self.pos = end;
                self.text_owned(out);
            }
            None => {
                self.pos += 1;
                self.text("&");
            }
        }
    }

    fn code_span(&mut self) {
        let start = self.pos;
        let n = self.b[start..].iter().take_while(|&&c| c == b'`').count();
        let after = start + n;
        let closer = self.backtick_run(n, after);
        let Some(close) = closer else {
            self.pos = after;
            self.text(&self.s[start..after]);
            return;
        };
        self.pos = close + n;
        // The span's copy is charged before it is made.
        if !self.m.text(close - after) {
            return;
        }
        let mut code = self.s[after..close].replace('\n', " ");
        if code.len() >= 2
            && code.starts_with(' ')
            && code.ends_with(' ')
            && code.bytes().any(|c| c != b' ')
        {
            code.pop();
            code.remove(0);
        }
        self.add(Kind::Code(code));
    }

    /// Start of the first maximal backtick run of length `n` at or after
    /// `from`.
    fn backtick_run(&mut self, n: usize, from: usize) -> Option<usize> {
        let b = self.b;
        if self.backticks.is_none() {
            // The index holds one offset per maximal run: counted and
            // charged (8 bytes each) before it is built.
            let mut count = 0usize;
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'`' {
                    i += b[i..].iter().take_while(|&&c| c == b'`').count();
                    count += 1;
                } else {
                    i += 1;
                }
            }
            if !self
                .m
                .text(count.saturating_mul(std::mem::size_of::<usize>()))
            {
                return None;
            }
        }
        let runs = self.backticks.get_or_insert_with(|| {
            let mut runs: HashMap<usize, Vec<usize>> = HashMap::new();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'`' {
                    let len = b[i..].iter().take_while(|&&c| c == b'`').count();
                    runs.entry(len).or_default().push(i);
                    i += len;
                } else {
                    i += 1;
                }
            }
            runs
        });
        let starts = runs.get(&n)?;
        starts.get(starts.partition_point(|&p| p < from)).copied()
    }

    /// First occurrence of `ENDS[k]` at or after `from`, memoized.
    fn find_end(&mut self, k: usize, from: usize) -> Option<usize> {
        if let Some((searched, found)) = self.ends[k].searched
            && searched <= from
            && found.is_none_or(|f| f >= from)
        {
            return found;
        }
        let needle = ENDS[k];
        let found = self.b[from..]
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|p| from + p);
        self.ends[k].searched = Some((from, found));
        found
    }

    /// `<`: autolink, raw HTML, or a literal `<`.
    fn pointy(&mut self) {
        let start = self.pos;
        let i = start + 1;
        if let Some(n) = scan_uri(self.b, i) {
            self.autolink(i, n, false);
            return;
        }
        if let Some(n) = scan_email(self.b, i) {
            self.autolink(i, n, true);
            return;
        }
        if let Some(end) = self.raw_html(start) {
            self.pos = end;
            let h = self.m.own(&self.s[start..end]);
            self.add(Kind::Html(h));
            return;
        }
        self.pos = i;
        self.text("<");
    }

    fn autolink(&mut self, i: usize, n: usize, email: bool) {
        self.pos = i + n + 1;
        // Two nodes (link, its text); the destination (with `mailto:`) and
        // the link text are two strings of the decoded length each (a
        // reference can decode to more bytes than it takes), charged
        // before they are made.
        let d = decoded_len(&self.s[i..i + n], false);
        if !self.m.nodes(2)
            || !self
                .m
                .text(d.saturating_mul(2).saturating_add("mailto:".len()))
        {
            return;
        }
        if self.nodes.try_reserve(2).is_err() {
            self.m.fail_allocation();
            return;
        }
        // Backslashes are literal in autolinks; references are decoded.
        let text = decode(&self.s[i..i + n], false);
        let dest = if email {
            format!("mailto:{text}")
        } else {
            text.clone()
        };
        // The two nodes are already charged and reserved.
        let link = self.nodes.len();
        let mut node = Node::new(Kind::Link {
            dest,
            title: String::new(),
        });
        node.prev = self.tail;
        self.nodes.push(node);
        self.nodes[self.tail].next = link;
        self.tail = link;
        let t = self.nodes.len();
        self.nodes.push(Node::new(Kind::Text(text)));
        self.nodes[link].first = t;
        self.nodes[link].last = t;
    }

    /// Raw HTML at `start` (a `<`): its end.
    fn raw_html(&mut self, start: usize) -> Option<usize> {
        let b = &self.b[start..];
        let end_of = |p: &mut Self, k: usize, from: usize| {
            p.find_end(k, start + from).map(|e| e + ENDS[k].len())
        };
        if b.starts_with(b"<!--") {
            if b[4..].starts_with(b">") {
                return Some(start + 5);
            }
            if b[4..].starts_with(b"->") {
                return Some(start + 6);
            }
            return end_of(self, 0, 4);
        }
        if b.starts_with(b"<?") {
            return end_of(self, 1, 2);
        }
        if b.starts_with(b"<![CDATA[") {
            return end_of(self, 2, 9);
        }
        if b.starts_with(b"<!") && b.get(2).is_some_and(u8::is_ascii_alphabetic) {
            return end_of(self, 3, 2);
        }
        if b.starts_with(b"</") {
            return close_tag(b).map(|n| start + n);
        }
        open_tag(b).map(|n| start + n)
    }

    // ---- emphasis ----

    fn delim_run(&mut self, ch: u8) {
        let start = self.pos;
        let n = self.b[start..].iter().take_while(|&&c| c == ch).count();
        self.pos = start + n;
        let before = self.s[..start].chars().next_back().unwrap_or('\n');
        let after = self.s[self.pos..].chars().next().unwrap_or('\n');
        let left = !is_space(after) && (!is_punct(after) || is_space(before) || is_punct(before));
        let right = !is_space(before) && (!is_punct(before) || is_space(after) || is_punct(after));
        let (can_open, can_close) = if ch == b'_' {
            (
                left && (!right || is_punct(before)),
                right && (!left || is_punct(after)),
            )
        } else {
            (left, right)
        };
        let node = self.text(&self.s[start..self.pos]);
        if can_open || can_close {
            let d = self.delims.len();
            self.delims.push(Delim {
                node,
                ch,
                len: n,
                can_open,
                can_close,
                prev: self.last_delim,
                next: NONE,
            });
            if self.last_delim != NONE {
                self.delims[self.last_delim].next = d;
            }
            self.last_delim = d;
        }
    }

    fn remove_delim(&mut self, d: usize) {
        let (p, x) = (self.delims[d].prev, self.delims[d].next);
        if p != NONE {
            self.delims[p].next = x;
        }
        if x == NONE {
            self.last_delim = p;
        } else {
            self.delims[x].prev = p;
        }
    }

    fn delim_count(&self, d: usize) -> usize {
        match &self.nodes[self.delims[d].node].kind {
            Kind::Text(t) => t.len(),
            _ => 0,
        }
    }

    fn take_delims(&mut self, d: usize, n: usize) -> usize {
        match &mut self.nodes[self.delims[d].node].kind {
            Kind::Text(t) => {
                t.truncate(t.len() - n);
                t.len()
            }
            _ => 0,
        }
    }

    /// The spec's "process emphasis" over the delimiters at or above arena
    /// index `bottom`; they are all removed afterwards.
    fn process_emphasis(&mut self, bottom: usize) {
        // Keyed by delimiter char, whether the closer can also open, and
        // the closer's run length mod 3.
        let mut openers_bottom = [bottom; 12];
        let mut closer = NONE;
        let mut d = self.last_delim;
        while d != NONE && d >= bottom {
            closer = d;
            d = self.delims[d].prev;
        }
        while closer != NONE && !self.m.failed() {
            let c = &self.delims[closer];
            if !c.can_close {
                closer = c.next;
                continue;
            }
            let key = usize::from(c.ch == b'_') * 6 + usize::from(c.can_open) * 3 + c.len % 3;
            let mut opener = c.prev;
            let mut found = false;
            while opener != NONE && opener >= openers_bottom[key] {
                let o = &self.delims[opener];
                if o.can_open
                    && o.ch == c.ch
                    && (!(c.can_open || o.can_close)
                        || c.len.is_multiple_of(3)
                        || !(o.len + c.len).is_multiple_of(3))
                {
                    found = true;
                    break;
                }
                opener = o.prev;
            }
            let old = closer;
            if found {
                closer = self.insert_emph(opener, closer);
            } else {
                closer = self.delims[closer].next;
                openers_bottom[key] = old;
                if !self.delims[old].can_open {
                    self.remove_delim(old);
                }
            }
        }
        while self.last_delim != NONE && self.last_delim >= bottom {
            self.remove_delim(self.last_delim);
        }
    }

    /// Wraps the nodes between `opener` and `closer` in emphasis; returns
    /// the next closer to look at.
    fn insert_emph(&mut self, opener: usize, closer: usize) -> usize {
        let use_ = if self.delim_count(opener) >= 2 && self.delim_count(closer) >= 2 {
            2
        } else {
            1
        };
        let left_o = self.take_delims(opener, use_);
        let left_c = self.take_delims(closer, use_);
        let mut d = self.delims[closer].prev;
        while d != opener {
            let p = self.delims[d].prev;
            self.remove_delim(d);
            d = p;
        }
        let (o_node, c_node) = (self.delims[opener].node, self.delims[closer].node);
        let kind = if use_ == 1 { Kind::Emph } else { Kind::Strong };
        self.wrap(o_node, c_node, kind);
        if left_o == 0 {
            self.unlink(o_node);
            self.remove_delim(opener);
        }
        if left_c == 0 {
            self.unlink(c_node);
            let next = self.delims[closer].next;
            self.remove_delim(closer);
            return next;
        }
        closer
    }

    // ---- links and images ----

    fn push_bracket(&mut self, image: bool) {
        let node = self.text(if image { "![" } else { "[" });
        if let Some(last) = self.brackets.last_mut() {
            last.bracket_after = true;
        }
        self.brackets.push(Bracket {
            node,
            text_start: self.pos,
            image,
            id: self.brackets_made,
            bracket_after: false,
            delim_bottom: self.delims.len(),
        });
        self.brackets_made += 1;
    }

    fn close_bracket(&mut self) {
        let close = self.pos;
        self.pos += 1;
        let Some(opener) = self.brackets.last() else {
            self.text("]");
            return;
        };
        if !opener.image && opener.id < self.link_floor {
            self.brackets.pop();
            self.text("]");
            return;
        }
        let (image, text_start, bracket_after) =
            (opener.image, opener.text_start, opener.bracket_after);
        let target = self
            .inline_target()
            .or_else(|| self.reference_target(text_start, close, bracket_after));
        let Some((dest, title)) = target else {
            self.brackets.pop();
            self.pos = close + 1;
            self.text("]");
            return;
        };
        let Some(opener) = self.brackets.pop() else {
            return;
        };
        self.process_emphasis(opener.delim_bottom);
        let kind = if image {
            Kind::Image { dest, title }
        } else {
            Kind::Link { dest, title }
        };
        let prev = self.nodes[opener.node].prev;
        self.unlink(opener.node);
        self.wrap(prev, NONE, kind);
        if !image {
            self.link_floor = self.brackets_made;
        }
    }

    /// `(destination "title")` right after the `]`: (dest, title), with
    /// `pos` moved past the `)`.
    fn inline_target(&mut self) -> Option<(String, String)> {
        let b = self.b;
        if b.get(self.pos) != Some(&b'(') {
            return None;
        }
        let ws = |i: usize| {
            i + b[i..]
                .iter()
                .take_while(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r'))
                .count()
        };
        let i = ws(self.pos + 1);
        let (dest_end, dest) = match b.get(i) {
            Some(b')') => (i, &b[i..i]),
            _ => link_destination(b, i)?,
        };
        let title_start = ws(dest_end);
        let (title_end, title) = match link_title(b, title_start) {
            Some(t) if title_start > dest_end => t,
            _ => (title_start, &b[title_start..title_start]),
        };
        let end = ws(title_end);
        if b.get(end) != Some(&b')') {
            return None;
        }
        self.pos = end + 1;
        // Charged by the decoded length (a reference can decode to more
        // bytes than its source), before the copies are made.
        let (dest, title) = (utf8(dest), utf8(title));
        let n = decoded_len(dest, true).saturating_add(decoded_len(title, true));
        if !self.m.text(n) {
            return None;
        }
        Some((unescape(dest), unescape(title)))
    }

    /// Full (`[label]`), collapsed (`[]`) or shortcut reference for the
    /// link text `text_start..close`: (dest, title), with `pos` past it.
    fn reference_target(
        &mut self,
        text_start: usize,
        close: usize,
        bracket_after: bool,
    ) -> Option<(String, String)> {
        let after = close + 1;
        let label = match link_label(self.b, after) {
            Some((end, label)) if !label.is_empty() => {
                self.pos = end;
                label
            }
            found => {
                if bracket_after {
                    return None;
                }
                self.pos = found.map_or(after, |(end, _)| end);
                &self.b[text_start..close]
            }
        };
        // Bound the work: a label longer than 999 characters never matches.
        if label.len() > MAX_LABEL * 4 || utf8(label).chars().count() > MAX_LABEL {
            return None;
        }
        let def = self.refs.get(&normalize_label(utf8(label)))?;
        // Each use of a reference copies its destination and title: this
        // is the expansion the owned-text budget stops.
        if !self.m.text(def.dest.len() + def.title.len()) {
            return None;
        }
        Some((def.dest.clone(), def.title.clone()))
    }

    // ---- output ----

    /// Appends the sibling list from `n` to `out` as inlines, nested at
    /// `depth`. Adjacent text is merged.
    fn convert(&mut self, mut n: usize, depth: usize, out: &mut Vec<Inline>) {
        while n != NONE && !self.m.failed() {
            let next = self.nodes[n].next;
            let first = self.nodes[n].first;
            let kind = std::mem::replace(&mut self.nodes[n].kind, Kind::SoftBreak);
            let children = |p: &mut Self| {
                let mut c = Vec::new();
                p.convert(first, depth + 1, &mut c);
                c
            };
            let inline = match kind {
                Kind::Text(t) => {
                    push_text(out, &t, self.m);
                    n = next;
                    continue;
                }
                Kind::Code(t) => Inline::Code(t),
                Kind::Html(t) => Inline::Html(t),
                Kind::SoftBreak => Inline::SoftBreak,
                Kind::HardBreak => Inline::HardBreak,
                _ if depth >= MAX_DEPTH => {
                    self.flatten(first, out);
                    n = next;
                    continue;
                }
                Kind::Emph => Inline::Emph(children(self)),
                Kind::Strong => Inline::Strong(children(self)),
                Kind::Link { dest, title } => Inline::Link {
                    dest,
                    title,
                    content: children(self),
                    auto: false,
                },
                Kind::Image { dest, title } => Inline::Image {
                    dest,
                    title,
                    alt: children(self),
                },
            };
            self.m.push(out, inline);
            n = next;
        }
    }

    /// Appends the leaves under the sibling list from `n`, without their
    /// containers. Iterative, so depth costs no stack.
    fn flatten(&mut self, n: usize, out: &mut Vec<Inline>) {
        let mut stack = vec![n];
        while let Some(n) = stack.pop() {
            if n == NONE {
                continue;
            }
            stack.push(self.nodes[n].next);
            let kind = std::mem::replace(&mut self.nodes[n].kind, Kind::SoftBreak);
            match kind {
                Kind::Text(t) => push_text(out, &t, self.m),
                Kind::Code(t) => self.m.push(out, Inline::Code(t)),
                Kind::Html(t) => self.m.push(out, Inline::Html(t)),
                Kind::SoftBreak => self.m.push(out, Inline::SoftBreak),
                Kind::HardBreak => self.m.push(out, Inline::HardBreak),
                Kind::Emph | Kind::Strong | Kind::Link { .. } | Kind::Image { .. } => {
                    stack.push(self.nodes[n].first);
                }
            }
        }
    }
}

/// Appends text, merged into a preceding text. The arena's string is
/// already charged and is dropped right after, so a merge or a copy here
/// is transient and only needs a fallible reservation.
fn push_text(out: &mut Vec<Inline>, t: &str, m: &mut Meter) {
    if t.is_empty() {
        return;
    }
    if let Some(Inline::Text(last)) = out.last_mut() {
        m.reserve_str(last, t.len());
        if !m.failed() {
            last.push_str(t);
        }
    } else {
        let mut copy = String::new();
        m.reserve_str(&mut copy, t.len());
        if !m.failed() {
            copy.push_str(t);
            m.push(out, Inline::Text(copy));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Block, Document, html, md, txt};
    use std::time::{Duration, Instant};

    fn html(md: &str) -> String {
        html::write(&md::read(md.as_bytes()).unwrap())
    }

    /// Generous: quadratic behaviour on these inputs takes minutes.
    fn limit() -> Duration {
        Duration::from_secs(if cfg!(debug_assertions) { 10 } else { 1 })
    }

    fn timed(input: &str) -> (Document, Duration) {
        let t = Instant::now();
        let d = md::read(input.as_bytes()).unwrap();
        (d, t.elapsed())
    }

    fn inline_depth(is: &[Inline]) -> usize {
        is.iter()
            .map(|i| match i {
                Inline::Emph(c) | Inline::Strong(c) => 1 + inline_depth(c),
                Inline::Link { content: c, .. } | Inline::Image { alt: c, .. } => {
                    1 + inline_depth(c)
                }
                _ => 0,
            })
            .max()
            .unwrap_or(0)
    }

    fn para_depth(d: &Document) -> usize {
        let mut blocks = &d.blocks;
        loop {
            match blocks.as_slice() {
                [Block::BlockQuote(inner)] => blocks = inner,
                [Block::Paragraph(is)] => return inline_depth(is),
                other => panic!("unexpected blocks: {}", other.len()),
            }
        }
    }

    #[test]
    fn deep_brackets_no_quadratic() {
        let n = 10_000;
        let inputs = [
            "[".repeat(n),
            "![".repeat(n),
            format!("{}{}", "[".repeat(n), "]".repeat(n)),
            format!("{}a{}", "[".repeat(n), "](b)".repeat(n)),
            format!("{}a{}", "![".repeat(n), "](b)".repeat(n)),
            "[a](".repeat(n),
            "[a](<b".repeat(n),
            "[a](b \"c".repeat(n),
            "[a](()".repeat(n),
            "[a][".repeat(n),
            "[a]".repeat(n),
            "*".repeat(n),
            "_".repeat(n),
            "*a ".repeat(n),
            "a**b*".repeat(n),
            "_a __b ".repeat(n),
            format!("{}a{}", "*a ".repeat(n), " a*".repeat(n)),
            (1..=200).map(|k| "`".repeat(k) + "a").collect::<String>(),
            "`a``".repeat(n),
            "<!--".repeat(n),
            "<?".repeat(n),
            "<![CDATA[".repeat(n),
            "<!a".repeat(n),
            "<a b=\"".repeat(n),
            "&#".repeat(n),
            "a\\".repeat(n),
        ];
        for input in &inputs {
            let (d, t) = timed(input);
            assert!(t < limit(), "{t:?} for {:?}...", &input[..20]);
            let t = Instant::now();
            html::write(&d);
            md::write(&d);
            txt::write(&d);
            assert!(t.elapsed() < limit());
        }
    }

    #[test]
    fn nesting_is_capped_and_writers_survive() {
        let n = 1000;
        let emph = format!("{}b{}", "*a ".repeat(n), " a*".repeat(n));
        let images = format!("{}x{}", "![".repeat(n), "](u)".repeat(n));
        let mixed = format!("{}x{}", "*![".repeat(n), "](u)*".repeat(n));
        for p in [emph, images, mixed] {
            let d = md::read(p.as_bytes()).unwrap();
            assert_eq!(para_depth(&d), MAX_DEPTH, "{:?}", &p[..10]);
            // Inside the deepest block quotes: both caps at once.
            let quoted = format!("{}{p}", ">".repeat(300));
            let d = md::read(quoted.as_bytes()).unwrap();
            assert_eq!(para_depth(&d), MAX_DEPTH);
            html::write(&d);
            md::write(&d);
            txt::write(&d);
        }
    }

    #[test]
    fn flattening_keeps_content() {
        let deep = format!("{}`c`{}", "*a ".repeat(300), " a*".repeat(300));
        let d = md::read(deep.as_bytes()).unwrap();
        let text = txt::write(&d).0;
        assert_eq!(text.matches('a').count(), 600);
        assert!(text.contains('c'));
        assert!(!text.contains('*'));
    }

    #[test]
    fn link_openers_deactivated_by_counter() {
        assert_eq!(
            html("[a [b](c) [d] e](f)"),
            "<p>[a <a href=\"c\">b</a> [d] e](f)</p>\n"
        );
        assert_eq!(
            html("![a [b](c)](d)"),
            "<p><img src=\"d\" alt=\"a b\" /></p>\n"
        );
    }

    #[test]
    fn breaks_and_spaces() {
        assert_eq!(html("a  \nb"), "<p>a<br />\nb</p>\n");
        assert_eq!(html("a \t\nb"), "<p>a\nb</p>\n");
        assert_eq!(html("a\\\nb"), "<p>a<br />\nb</p>\n");
        assert_eq!(html("a\\"), "<p>a\\</p>\n");
        assert_eq!(html("*a*  \nb"), "<p><em>a</em><br />\nb</p>\n");
    }

    #[test]
    fn code_spans() {
        assert_eq!(html("`` a ``"), "<p><code>a</code></p>\n");
        assert_eq!(html("`  `"), "<p><code>  </code></p>\n");
        assert_eq!(html("`a\nb`"), "<p><code>a b</code></p>\n");
        assert_eq!(html("```a``"), "<p>```a``</p>\n");
        assert_eq!(html("`a``b`"), "<p><code>a``b</code></p>\n");
    }

    #[test]
    fn autolinks_and_raw_html() {
        assert_eq!(
            html("<http://a?b&amp;c\\d>"),
            "<p><a href=\"http://a?b&amp;c%5Cd\">http://a?b&amp;c\\d</a></p>\n"
        );
        assert_eq!(
            html("<a.b@c-d.e>"),
            "<p><a href=\"mailto:a.b@c-d.e\">a.b@c-d.e</a></p>\n"
        );
        assert_eq!(html("<a@-b.c>"), "<p>&lt;a@-b.c&gt;</p>\n");
        assert_eq!(html("<x:y>"), "<p>&lt;x:y&gt;</p>\n");
        assert_eq!(html("a <!-- b\nc --> d"), "<p>a <!-- b\nc --> d</p>\n");
        assert_eq!(
            html("a <!--> <!---> <?x?>"),
            "<p>a <!--> <!---> <?x?></p>\n"
        );
        assert_eq!(html("x <!-- a <!-- b -->"), "<p>x <!-- a <!-- b --></p>\n");
        assert_eq!(html("x <!-- a"), "<p>x &lt;!-- a</p>\n");
    }

    #[test]
    fn memoized_end_search_respects_start() {
        // The `?>` found for the first `<?` is behind the second one.
        assert_eq!(html("x <?a?> <?b"), "<p>x <?a?> &lt;?b</p>\n");
        assert_eq!(html("x <?a <?b?>"), "<p>x <?a <?b?></p>\n");
    }

    #[test]
    fn unicode_flanking() {
        assert_eq!(html("a*€*b"), "<p>a*€*b</p>\n");
        assert_eq!(html("€*a*€"), "<p>€<em>a</em>€</p>\n");
        assert_eq!(html("*a*\u{3000}b"), "<p><em>a</em>\u{3000}b</p>\n");
        assert_eq!(html("a\u{a0}*\u{a0}b*"), "<p>a\u{a0}*\u{a0}b*</p>\n");
    }

    #[test]
    fn long_labels_never_match() {
        let label = "a".repeat(1000);
        let md = format!("[{label}]\n\n[{}]: /u", "a".repeat(999));
        assert!(!html(&md).contains("href"));
        let md = format!("[{}]\n\n[{}]: /u", "a".repeat(999), "a".repeat(999));
        assert!(html(&md).contains("href=\"/u\""));
    }

    #[test]
    fn punct_table_is_sorted() {
        assert!(PUNCT.windows(2).all(|w| w[0].1 < w[1].0));
        assert!(PUNCT.iter().all(|&(lo, hi)| lo <= hi && !lo.is_ascii()));
        assert!(is_punct('€') && is_punct('¡') && is_punct('—') && is_punct('$'));
        assert!(!is_punct('a') && !is_punct('é') && !is_punct(' '));
    }
}
