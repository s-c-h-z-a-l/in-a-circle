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
//! In the GFM dialect a `~~` run is a delimiter too (strikethrough), with
//! the emphasis flanking rules and its own search bounds; one tilde or three
//! or more stay text.
//!
//! GFM also reads extended autolinks: `www.`, `http://`, `https://` and
//! `ftp://` links are found during the pass (at a `w` or the scheme's `:`),
//! bare email addresses in the finished text, outside links and images.
//! Each scan is bounded: the domain run is memoized for every start inside
//! it; whether the trailing-punctuation rule trims into the domain is
//! decided before the path is read, by a scan that stops at the first byte
//! the trim keeps, so only a link that is taken reads its path (and counts
//! its parentheses), once; and an address scan never passes the next `@`.
//!
//! The rules follow the GFM spec text. One reading follows cmark-gfm where
//! the text is open to two: the `www` of `www.` counts as a segment, so
//! `www.x` (no period after `www.`) is a link, as the reference renders it.
//!
//! Nesting (emphasis, strikethrough, links, images) deeper than
//! [`MAX_DEPTH`] is flattened when the arena becomes the [`Inline`] tree:
//! those wrappers are dropped and their content is kept. This bounds the recursion of every writer.

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
    Parser::new(raw, refs, dialect, m).parse()
}

enum Kind {
    Text(String),
    Code(String),
    Html(String),
    SoftBreak,
    HardBreak,
    Emph,
    Strong,
    Strikethrough,
    /// `auto`: a GFM extended autolink.
    Link {
        dest: String,
        title: String,
        auto: bool,
    },
    Image {
        dest: String,
        title: String,
    },
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

/// A `*` or `_` run that can open or close emphasis, or (GFM) a `~~` run
/// that can open or close strikethrough. Delimiters live in an
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
    /// Which syntax is read: GFM adds the `~~` strikethrough delimiter.
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
    /// The last domain run scanned for an extended autolink.
    domain: Option<DomainRun>,
    /// Opener candidates looked at by `process_emphasis`, so a test can
    /// bound the search without timing it.
    #[cfg(test)]
    steps: usize,
    /// Bytes the extended-autolink scans look at, for the same reason.
    #[cfg(test)]
    auto_steps: usize,
}

/// Length of the only tilde run that opens or closes strikethrough: GFM
/// 0.29 wraps strikethrough "in two tildes"; one tilde ("~5 min") and three
/// or more stay text. (cmark-gfm also takes one tilde unless its
/// double-tilde option is set; the spec text is the authority here.)
const STRIKE_RUN: usize = 2;

/// Slots of `process_emphasis`'s `openers_bottom`: per delimiter char
/// (`*`, `_`, `~`), whether the closer can also open (2), and the closer's
/// run length mod 3 (3).
const OPENER_KEYS: usize = 3 * 2 * 3;

fn is_special(c: u8) -> bool {
    matches!(
        c,
        b'\n' | b'\\' | b'`' | b'&' | b'<' | b'[' | b']' | b'!' | b'*' | b'_'
    )
}

/// Unicode whitespace as CommonMark defines it: Zs plus tab, LF, FF, CR.
pub(super) fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\x0c' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// Unicode punctuation as CommonMark defines it: general categories P and S.
pub(super) fn is_punct(c: char) -> bool {
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

/// Schemes of a GFM extended URL autolink (spec: "`http://`, `https://`,
/// or `ftp://`"); matched without regard to case, as URI schemes are.
const SCHEMES: [&[u8]; 3] = [b"http", b"https", b"ftp"];

/// Whitespace as the GFM spec (and cmark-gfm's `cmark_isspace`) means it:
/// space, tab, LF, VT, FF, CR. It ends an extended autolink's path.
fn is_ascii_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// An extended autolink may follow the start of the text, whitespace, or
/// one of the delimiting characters `*`, `_`, `~`, `(` (GFM spec).
fn autolink_boundary(b: &[u8], i: usize) -> bool {
    i == 0 || is_ascii_space(b[i - 1]) || matches!(b[i - 1], b'*' | b'_' | b'~' | b'(')
}

/// A character of a domain segment: alphanumeric (Unicode, so an
/// internationalized name is one), `_` or `-`.
fn is_domain_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// A character of an email address's local part (GFM spec).
fn is_local_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_' | b'+')
}

/// A character of an email address's domain segment (GFM spec).
fn is_email_domain_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_'
}

/// Where the periods and underscores of a domain run fall, as far as the
/// last segments need them.
#[derive(Clone, Copy, Default)]
struct Segments {
    /// The last period followed by a segment character (a period with a
    /// segment after it).
    good_dot: Option<usize>,
    last_dot: Option<usize>,
    prev_dot: Option<usize>,
    /// An underscore after `last_dot` (from the run's start without one).
    us_last: bool,
    /// An underscore between `prev_dot` (or the run's start) and `last_dot`.
    us_prev: bool,
}

/// A maximal run of domain characters and periods, scanned once from
/// `start`. Every later start inside it ends at the same `end`, sees the
/// same last segments and gets the same trimmed link end, so it answers
/// them all without a rescan.
#[derive(Clone, Copy)]
struct DomainRun {
    start: usize,
    end: usize,
    /// The segments of the whole run.
    full: Segments,
    /// Just after the run's last character that is not `.` or `_`: where
    /// the trailing-punctuation rule stops when it eats into the run.
    cut: usize,
    /// The segments of `start..cut`: the domain when the link is trimmed
    /// into the run (`_www.a.b_` links `www.a.b`).
    at_cut: Segments,
    /// Whether the trailing-punctuation rule removes the whole path after
    /// the run ([`path_trims_away`]), once computed. It does not depend on
    /// where the link starts: the run holds no parenthesis.
    trims: Option<bool>,
}

/// Whether the domain of `run` from `d`, with the segments `g` (of the
/// whole run or of its trimmed part), is valid (GFM spec: segments of
/// alphanumerics, `_` and `-` separated by periods, at least one period,
/// no underscore in the last two segments). A period needs a segment after
/// it to count: `www.` alone is no domain.
fn valid(run: &DomainRun, g: &Segments, d: usize) -> bool {
    if d >= run.end || g.good_dot.is_none_or(|p| p < d) {
        return false;
    }
    // The segment before the last period starts at `prev_dot` or at `d`.
    // A later start than the run's with no `prev_dot` after it is a `www.`
    // whose period is the last one: its own segment `www` has no
    // underscore.
    let us_prev = match g.prev_dot {
        Some(p) if p >= d => g.us_prev,
        _ if run.start == d => g.us_prev,
        _ => {
            debug_assert_eq!(g.last_dot, Some(d + 3));
            false
        }
    };
    !g.us_last && !us_prev
}

/// The length of an extended autolink of `link` after the GFM path
/// validation: trailing `?!.,:*_~` go, a trailing `&` + alphanumerics + `;`
/// (an entity look-alike) goes, and a trailing `)` goes while the link has
/// more `)` than `(`. `opens`/`closes` are the link's parentheses, counted
/// once by the caller (no `(` is ever removed, so only `closes` changes);
/// each step removes at least one byte, so the loop is linear in what it
/// removes.
fn trim_link(link: &[u8], opens: usize, mut closes: usize, steps: &mut usize) -> usize {
    let mut n = link.len();
    while n > 0 {
        *steps += 1;
        match link[n - 1] {
            b'?' | b'!' | b'.' | b',' | b':' | b'*' | b'_' | b'~' => n -= 1,
            b';' => {
                let k = link[..n - 1]
                    .iter()
                    .rev()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .count();
                *steps += k;
                // `&` sits at `n - 2 - k`.
                if k > 0 && n >= k + 2 && link[n - 2 - k] == b'&' {
                    n -= k + 2;
                } else {
                    break;
                }
            }
            b')' if closes > opens => {
                closes -= 1;
                n -= 1;
            }
            _ => break,
        }
    }
    n
}

/// Whether [`trim_link`] removes the whole path from `i` (the end of a
/// domain run) to the next whitespace, `<` or the end, so that the trim
/// reaches into the run. It does when every byte of the path is one of
/// `?!.,:*_~`, a `)` (with no `(` in the path, every `)` is one too many:
/// a domain holds none), or part of an entity look-alike `&` alnum+ `;`;
/// those are exactly what the trim removes, in any order. Stops at the
/// first other byte, so it never reads into a later candidate: a `www.` or
/// scheme there needs a letter outside an entity look-alike after a
/// boundary, and the scan stops at that letter.
fn path_trims_away(b: &[u8], mut i: usize, steps: &mut usize) -> bool {
    loop {
        *steps += 1;
        match b.get(i) {
            None => return true,
            Some(&c) if is_ascii_space(c) || c == b'<' => return true,
            Some(b'?' | b'!' | b'.' | b',' | b':' | b'*' | b'_' | b'~' | b')') => i += 1,
            Some(b'&') => {
                let k = b[i + 1..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .count();
                *steps += k;
                if k == 0 || b.get(i + 1 + k) != Some(&b';') {
                    return false;
                }
                i += k + 2;
            }
            _ => return false,
        }
    }
}

/// The domain of an email address from `j` (after the `@`): segments of
/// [`is_email_domain_char`] separated by single periods, at least one
/// period, ending in an alphanumeric. A second `@` right after spoils it
/// (cmark-gfm: one `@` per address). Its end, or `None`.
fn email_domain(b: &[u8], j: usize, steps: &mut usize) -> Option<usize> {
    let (mut k, mut dots) = (j, 0);
    loop {
        *steps += 1;
        match b.get(k) {
            Some(&c) if is_email_domain_char(c) => k += 1,
            Some(b'.') if k > j && b.get(k + 1).is_some_and(|&c| is_email_domain_char(c)) => {
                dots += 1;
                k += 1;
            }
            _ => break,
        }
    }
    let ok = dots > 0 && b[k - 1].is_ascii_alphanumeric() && b.get(k) != Some(&b'@');
    ok.then_some(k)
}

impl<'a> Parser<'a> {
    fn new(raw: &'a str, refs: &'a Refs, dialect: Dialect, m: &'a mut Meter) -> Self {
        Parser {
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
            domain: None,
            #[cfg(test)]
            steps: 0,
            #[cfg(test)]
            auto_steps: 0,
        }
    }

    fn parse(&mut self) -> Vec<Inline> {
        self.run();
        if self.m.failed() {
            return Vec::new();
        }
        self.process_emphasis(0);
        let mut out = Vec::new();
        let first = self.nodes[HEAD].next;
        self.convert(first, 0, false, &mut out);
        out
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
                b'~' if self.dialect == Dialect::Gfm => self.delim_run(c),
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
        let gfm = self.dialect == Dialect::Gfm;
        // GFM: an extended www or URL autolink, never inside a link's text
        // (any open bracket holds them off, as in cmark-gfm). Both start in
        // a text run: a `w`, or the scheme's letters before a `:`.
        let auto = gfm && self.brackets.is_empty();
        let mut end = start;
        loop {
            if auto && let Some((from, to)) = self.extended_at(start, end) {
                if from > start {
                    self.text(&self.s[start..from]);
                }
                self.pos = to;
                self.extended_link(from, to, self.b[from] == b'w');
                return;
            }
            end += 1;
            match self.b.get(end) {
                Some(&c)
                    if !((is_special(c) || (gfm && c == b'~'))
                        && !(c == b'!' && self.b.get(end + 1) != Some(&b'['))) => {}
                _ => break,
            }
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

    /// An extended www or URL autolink at `i` (a `w`, or a `:`) in the text
    /// run from `start`: the link's start and end. The scheme's letters lie
    /// in the run: the byte before a run is never a letter.
    fn extended_at(&mut self, start: usize, i: usize) -> Option<(usize, usize)> {
        let b = self.b;
        let (from, d) = match b[i] {
            b'w' if b[i..].starts_with(b"www.") => (i, i),
            b':' if b[i + 1..].starts_with(b"//") => {
                let from = SCHEMES.iter().find_map(|sch| {
                    let f = i.checked_sub(sch.len()).filter(|&f| f >= start)?;
                    b[f..i].eq_ignore_ascii_case(sch).then_some(f)
                })?;
                (from, i + 3)
            }
            _ => return None,
        };
        if !autolink_boundary(b, from) {
            return None;
        }
        let mut run = match self.domain {
            Some(r) if r.start <= d && d < r.end => r,
            _ => self.scan_domain(d),
        };
        if b.get(d) == Some(&b'.') || !(valid(&run, &run.full, d) || valid(&run, &run.at_cut, d)) {
            self.domain = Some(run);
            return None;
        }
        // Which domain counts (the whole run, or the run trimmed to `cut`)
        // is decided before the path is scanned, so a candidate that fails
        // here never scans the path, and a later candidate in that path
        // never makes it scan again.
        let trims = match run.trims {
            Some(t) => t,
            None => {
                let mut steps = 0;
                let t = path_trims_away(b, run.end, &mut steps);
                self.count(steps);
                run.trims = Some(t);
                t
            }
        };
        self.domain = Some(run);
        if trims {
            // The trim takes the path, then the run's trailing `.` and
            // `_`, and stops at `cut`.
            return valid(&run, &run.at_cut, d).then_some((from, run.cut));
        }
        if !valid(&run, &run.full, d) {
            return None;
        }
        let tail = self.link_tail(d, run.end);
        debug_assert!(tail > run.end);
        Some((from, tail))
    }

    /// The end of a link whose domain starts at `d` and ends at `end`: the
    /// path runs to whitespace or `<` (spec: "`<` immediately ends an
    /// autolink"), then [`trim_link`]. Its parentheses are counted on the
    /// way for the trailing-`)` rule (a domain holds none).
    fn link_tail(&mut self, d: usize, mut end: usize) -> usize {
        let b = self.b;
        let (mut opens, mut closes, mut steps) = (0, 0, 0);
        while let Some(&c) = b.get(end).filter(|&&c| !is_ascii_space(c) && c != b'<') {
            opens += usize::from(c == b'(');
            closes += usize::from(c == b')');
            end += 1;
            steps += 1;
        }
        let n = trim_link(&b[d..end], opens, closes, &mut steps);
        self.count(steps);
        d + n
    }

    fn scan_domain(&mut self, start: usize) -> DomainRun {
        let mut r = DomainRun {
            start,
            end: self.b.len(),
            full: Segments::default(),
            cut: start,
            at_cut: Segments::default(),
            trims: None,
        };
        let mut after_dot = false;
        let mut steps = 0;
        let g = &mut r.full;
        for (off, c) in self.s[start..].char_indices() {
            steps += 1;
            let i = start + off;
            if c == '.' {
                g.prev_dot = g.last_dot;
                g.last_dot = Some(i);
                g.us_prev = g.us_last;
                g.us_last = false;
                after_dot = true;
                continue;
            }
            if !is_domain_char(c) {
                r.end = i;
                break;
            }
            if after_dot {
                g.good_dot = g.last_dot;
                after_dot = false;
            }
            g.us_last |= c == '_';
            if c != '_' {
                r.cut = i + c.len_utf8();
                r.at_cut = *g;
            }
        }
        self.count(steps);
        r
    }

    /// Records the bytes an autolink scan looked at (tests only).
    #[cfg_attr(not(test), allow(unused_variables))]
    fn count(&mut self, steps: usize) {
        #[cfg(test)]
        {
            self.auto_steps += steps;
        }
    }

    /// The extended www or URL autolink `from..to`: its text as written,
    /// its destination the same with `http://` before a `www.`.
    fn extended_link(&mut self, from: usize, to: usize, www: bool) {
        let text = &self.s[from..to];
        let prefix = if www { "http://" } else { "" };
        // Two nodes (link, its text) and two strings, charged first.
        if !self.m.nodes(2)
            || !self
                .m
                .text(text.len().saturating_mul(2).saturating_add(prefix.len()))
        {
            return;
        }
        let dest = format!("{prefix}{text}");
        self.push_link(dest, text.to_string(), true);
    }

    /// Appends a link holding one text node; both are already charged.
    fn push_link(&mut self, dest: String, text: String, auto: bool) {
        if self.nodes.try_reserve(2).is_err() {
            self.m.fail_allocation();
            return;
        }
        let link = self.nodes.len();
        let mut node = Node::new(Kind::Link {
            dest,
            title: String::new(),
            auto,
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
        // Backslashes are literal in autolinks; references are decoded.
        let text = decode(&self.s[i..i + n], false);
        let dest = if email {
            format!("mailto:{text}")
        } else {
            text.clone()
        };
        self.push_link(dest, text, false);
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
        if (can_open || can_close) && (ch != b'~' || n == STRIKE_RUN) {
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
        let mut openers_bottom = [bottom; OPENER_KEYS];
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
            let ch = match c.ch {
                b'*' => 0,
                b'_' => 1,
                _ => 2,
            };
            let key = ch * 6 + usize::from(c.can_open) * 3 + c.len % 3;
            let mut opener = c.prev;
            let mut found = false;
            while opener != NONE && opener >= openers_bottom[key] {
                #[cfg(test)]
                {
                    self.steps += 1;
                }
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

    /// Wraps the nodes between `opener` and `closer` in emphasis (or, for
    /// `~~`, strikethrough: both runs are exactly two, so both go whole);
    /// returns the next closer to look at.
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
        let kind = match (self.delims[opener].ch, use_) {
            (b'~', _) => Kind::Strikethrough,
            (_, 1) => Kind::Emph,
            _ => Kind::Strong,
        };
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
            Kind::Link {
                dest,
                title,
                auto: false,
            }
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

    /// Converts the sibling list from `n` into the empty `out` as inlines,
    /// nested at `depth`. Adjacent text is merged; in GFM, outside a link
    /// or image (`in_link`), the merged text is then searched for email
    /// addresses.
    fn convert(&mut self, mut n: usize, depth: usize, in_link: bool, out: &mut Vec<Inline>) {
        while n != NONE && !self.m.failed() {
            let next = self.nodes[n].next;
            let first = self.nodes[n].first;
            let kind = std::mem::replace(&mut self.nodes[n].kind, Kind::SoftBreak);
            let children = |p: &mut Self, in_link: bool| {
                let mut c = Vec::new();
                p.convert(first, depth + 1, in_link, &mut c);
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
                Kind::Emph => Inline::Emph(children(self, in_link)),
                Kind::Strong => Inline::Strong(children(self, in_link)),
                Kind::Strikethrough => Inline::Strikethrough(children(self, in_link)),
                Kind::Link { dest, title, auto } => Inline::Link {
                    dest,
                    title,
                    content: children(self, true),
                    auto,
                },
                Kind::Image { dest, title } => Inline::Image {
                    dest,
                    title,
                    alt: children(self, true),
                },
            };
            self.m.push(out, inline);
            n = next;
        }
        if self.dialect == Dialect::Gfm && !in_link {
            self.emails(out);
        }
    }

    /// GFM extended email autolinks: every text of `out` that holds an `@`
    /// is split around the addresses in it. Linear: a local part is found
    /// by going back from its `@`, never past the previous `@` or the last
    /// address, and a domain scan stops at the next `@`.
    fn emails(&mut self, out: &mut Vec<Inline>) {
        if !out
            .iter()
            .any(|i| matches!(i, Inline::Text(t) if t.contains('@')))
        {
            return;
        }
        let items = std::mem::take(out);
        for item in items {
            match item {
                Inline::Text(t) if t.contains('@') => self.split_emails(&t, out),
                other => self.m.push(out, other),
            }
        }
    }

    fn split_emails(&mut self, t: &str, out: &mut Vec<Inline>) {
        let b = t.as_bytes();
        // `floor`: the start of the text not yet appended.
        let (mut floor, mut i, mut steps) = (0, 0, 0);
        while let Some(off) = b[i..].iter().position(|&c| c == b'@') {
            let at = i + off;
            steps += off + 1;
            let mut from = at;
            while from > floor && is_local_char(b[from - 1]) {
                from -= 1;
                steps += 1;
            }
            i = at + 1;
            let Some(end) = email_domain(b, at + 1, &mut steps).filter(|_| from < at) else {
                continue;
            };
            let email = &t[from..end];
            // Three nodes (the link, its text, the text after it) and the
            // destination, a second copy of the address with `mailto:`.
            // The pieces of `t` (before, address, rest) are not charged
            // again: together they are `t`'s bytes, which are charged, and
            // `t` is dropped when the caller's item goes, so the copy
            // briefly holds at most one charged text node twice.
            if !self.m.nodes(3) || !self.m.text("mailto:".len() + email.len()) {
                return;
            }
            push_text(out, &t[floor..from], self.m);
            let mut content = Vec::new();
            push_text(&mut content, email, self.m);
            let link = Inline::Link {
                dest: format!("mailto:{email}"),
                title: String::new(),
                content,
                auto: true,
            };
            self.m.push(out, link);
            (floor, i) = (end, end);
        }
        push_text(out, &t[floor..], self.m);
        self.count(steps);
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
                Kind::Emph
                | Kind::Strong
                | Kind::Strikethrough
                | Kind::Link { .. }
                | Kind::Image { .. } => {
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
                Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => {
                    1 + inline_depth(c)
                }
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
    fn deep_strikethrough_is_capped() {
        let n = 1000;
        let p = format!("{}b{}", "~~a ".repeat(n), " a~~".repeat(n));
        let quoted = format!("{}{p}", ">".repeat(300));
        for p in [p, quoted] {
            let d = md::read_with(p.as_bytes(), Dialect::Gfm).unwrap();
            assert_eq!(para_depth(&d), MAX_DEPTH);
            // The flattened wrappers keep their text: every `a` and the `b`.
            assert_eq!(txt::write(&d).0.matches('a').count(), 2 * n);
            html::write(&d);
            md::write(&d);
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

    fn gfm(md: &str) -> String {
        html::write(&md::read_with(md.as_bytes(), Dialect::Gfm).unwrap())
    }

    #[test]
    fn single_tilde_is_text() {
        assert_eq!(gfm("~a~"), "<p>~a~</p>\n");
        assert_eq!(gfm("~5 min"), "<p>~5 min</p>\n");
        // A single tilde neither opens nor closes against a double one.
        assert_eq!(gfm("~a~~"), "<p>~a~~</p>\n");
        assert_eq!(gfm("~~a~"), "<p>~~a~</p>\n");
        assert_eq!(gfm("~~a~~"), "<p><del>a</del></p>\n");
    }

    #[test]
    fn triple_tilde_is_text() {
        // (Text first: three tildes at a line start open a code fence.)
        assert_eq!(gfm("x ~~~a~~~"), "<p>x ~~~a~~~</p>\n");
        assert_eq!(gfm("x ~~~~a~~~~"), "<p>x ~~~~a~~~~</p>\n");
        // A longer run is not split into a pair and a rest.
        assert_eq!(gfm("x ~~~a~~"), "<p>x ~~~a~~</p>\n");
        assert_eq!(gfm("~~a~~~"), "<p>~~a~~~</p>\n");
    }

    #[test]
    fn tilde_follows_the_flanking_rules() {
        assert_eq!(gfm("~~ a~~"), "<p>~~ a~~</p>\n");
        assert_eq!(gfm("~~a ~~"), "<p>~~a ~~</p>\n");
        assert_eq!(gfm("a~~b~~c"), "<p>a<del>b</del>c</p>\n");
        assert_eq!(gfm("\\~~a~~"), "<p>~~a~~</p>\n");
        // Unclosed openers stay text; the first closer takes the nearest.
        assert_eq!(gfm("~~a ~~b~~"), "<p>~~a <del>b</del></p>\n");
    }

    #[test]
    fn tilde_and_emphasis_nest() {
        assert_eq!(gfm("~~*a*~~"), "<p><del><em>a</em></del></p>\n");
        assert_eq!(gfm("*~~a~~*"), "<p><em><del>a</del></em></p>\n");
        assert_eq!(
            gfm("**a ~~b~~**"),
            "<p><strong>a <del>b</del></strong></p>\n"
        );
        assert_eq!(gfm("~~[a](b)~~"), "<p><del><a href=\"b\">a</a></del></p>\n");
        assert_eq!(gfm("[~~a](b)~~"), "<p><a href=\"b\">~~a</a>~~</p>\n");
        // Crossing runs: the first closer wins, the other stays text.
        assert_eq!(gfm("~~a *b~~ c*"), "<p><del>a *b</del> c*</p>\n");
        // A failed `~~` closer does not hide `*` openers below it.
        // (The `**` closer has the `~~` closer's run length and flags, so
        // a search bound shared between the two would stop at the `~~`.)
        assert_eq!(gfm("*a b~~ c*"), "<p><em>a b~~ c</em></p>\n");
        assert_eq!(gfm("**a b~~ c**"), "<p><strong>a b~~ c</strong></p>\n");
    }

    #[test]
    fn tilde_in_code_span_is_text() {
        assert_eq!(gfm("`~~a~~`"), "<p><code>~~a~~</code></p>\n");
        assert_eq!(
            gfm("~~a `b~~` c~~"),
            "<p><del>a <code>b~~</code> c</del></p>\n"
        );
        assert_eq!(gfm("<~~a~~>"), "<p>&lt;<del>a</del>&gt;</p>\n");
        assert_eq!(
            gfm("x <a title=\"~~a~~\">"),
            "<p>x <a title=\"~~a~~\"></p>\n"
        );
    }

    #[test]
    fn cm_dialect_reads_tildes_as_text() {
        for md in ["~~a~~", "a ~~b~~ c", "~~*a*~~"] {
            let cm = md::read_with(md.as_bytes(), Dialect::CommonMark).unwrap();
            assert_eq!(cm, md::read(md.as_bytes()).unwrap());
            assert!(!html::write(&cm).contains("<del>"), "{md}");
        }
        assert_eq!(
            md::read(b"~~a~~").unwrap().blocks,
            vec![Block::Paragraph(vec![Inline::Text("~~a~~".into())])]
        );
    }

    #[test]
    fn strikethrough_is_a_charged_node() {
        let read = |nodes: usize| {
            let limits = crate::Limits {
                nodes,
                ..crate::Limits::DEFAULT
            };
            md::read_limited_with(b"~~a~~", limits, Dialect::Gfm)
        };
        let need = (1..).find(|&n| read(n).is_ok()).unwrap();
        assert_eq!(
            read(need - 1),
            Err(crate::ReadError::Limit(crate::LimitError::Exceeded(
                "nodes"
            )))
        );
        let plain = |nodes: usize| {
            let limits = crate::Limits {
                nodes,
                ..crate::Limits::DEFAULT
            };
            md::read_limited_with(b"~~a~~", limits, Dialect::CommonMark).is_ok()
        };
        // GFM: the paragraph, two `~~` text nodes, `a`, and the wrapper;
        // CommonMark reads `~~a~~` as one text node.
        assert_eq!(need, (1..).find(|&n| plain(n)).unwrap() + 3);
    }

    /// The paragraph's inlines, GFM.
    fn gfm_inlines(md: &str) -> Vec<Inline> {
        match md::read_with(md.as_bytes(), Dialect::Gfm)
            .unwrap()
            .blocks
            .as_slice()
        {
            [Block::Paragraph(p)] => p.clone(),
            b => panic!("not one paragraph: {b:?}"),
        }
    }

    fn auto_link(dest: &str, text: &str) -> Inline {
        Inline::Link {
            dest: dest.into(),
            title: String::new(),
            content: vec![Inline::Text(text.into())],
            auto: true,
        }
    }

    #[test]
    fn www_gets_http_scheme() {
        assert_eq!(
            gfm_inlines("www.commonmark.org"),
            [auto_link("http://www.commonmark.org", "www.commonmark.org")]
        );
        assert_eq!(
            gfm_inlines("a https://x.y/z b"),
            [
                Inline::Text("a ".into()),
                auto_link("https://x.y/z", "https://x.y/z"),
                Inline::Text(" b".into()),
            ]
        );
        // Schemes ignore case; the text and destination stay as written.
        assert_eq!(
            gfm("HTTP://x.y FTP://x.y"),
            "<p><a href=\"HTTP://x.y\">HTTP://x.y</a> <a href=\"FTP://x.y\">FTP://x.y</a></p>\n"
        );
        // Only at the start of a line, after whitespace or `*_~(`.
        assert_eq!(gfm("xwww.a.b"), "<p>xwww.a.b</p>\n");
        assert_eq!(gfm("xhttp://a.b"), "<p>xhttp://a.b</p>\n");
        assert_eq!(gfm("1https://a.b"), "<p>1https://a.b</p>\n");
        assert_eq!(gfm("<www.a.b"), "<p>&lt;www.a.b</p>\n");
        assert_eq!(
            gfm("a\n*www.a.b* ~~http://a.b~~ _www.a.b_"),
            "<p>a\n<em><a href=\"http://www.a.b\">www.a.b</a></em> \
             <del><a href=\"http://a.b\">http://a.b</a></del> \
             <em><a href=\"http://www.a.b\">www.a.b</a></em></p>\n"
        );
        // A valid domain: a period with a segment after it, no underscore
        // in the last two segments; `http://` needs one too.
        for no in [
            "www.",
            "www..",
            "www.a_b",
            "www.a_b.c",
            "http://a",
            "http://.a.b",
            "http://_a.b",
            "http://",
            "ftp://a_b.c",
        ] {
            assert_eq!(gfm(no), format!("<p>{no}</p>\n"), "{no:?}");
        }
        // The `www` of `www.` is a segment (cmark-gfm's reading; the spec
        // text is open to two): `www.x` is a link.
        assert_eq!(
            gfm("www.localhost"),
            "<p><a href=\"http://www.localhost\">www.localhost</a></p>\n"
        );
        assert_eq!(
            gfm("www.a_b.c.d"),
            "<p><a href=\"http://www.a_b.c.d\">www.a_b.c.d</a></p>\n"
        );
        assert_eq!(
            gfm("www.bücher.de"),
            "<p><a href=\"http://www.b%C3%BCcher.de\">www.bücher.de</a></p>\n"
        );
        // The path runs to whitespace or `<`, over any other syntax.
        assert_eq!(
            gfm("www.a.b/`c`*d*[e]\\f"),
            "<p><a href=\"http://www.a.b/%60c%60*d*%5Be%5D%5Cf\">www.a.b/`c`*d*[e]\\f</a></p>\n"
        );
    }

    #[test]
    fn trailing_punctuation_excluded() {
        assert_eq!(
            gfm("www.a.b?!.,:*_~ x"),
            "<p><a href=\"http://www.a.b\">www.a.b</a>?!.,:*_~ x</p>\n"
        );
        assert_eq!(
            gfm("http://a.b/c.d."),
            "<p><a href=\"http://a.b/c.d\">http://a.b/c.d</a>.</p>\n"
        );
        // `&` + alphanumerics + `;` looks like an entity and is left out;
        // any other trailing `;` stays in the link.
        assert_eq!(
            gfm("www.a.b/?q&h1;"),
            "<p><a href=\"http://www.a.b/?q\">www.a.b/?q</a>&amp;h1;</p>\n"
        );
        assert_eq!(
            gfm("www.a.b/c;"),
            "<p><a href=\"http://www.a.b/c;\">www.a.b/c;</a></p>\n"
        );
        assert_eq!(
            gfm("www.a.b/&;"),
            "<p><a href=\"http://www.a.b/&amp;;\">www.a.b/&amp;;</a></p>\n"
        );
        assert_eq!(
            gfm("www.a.b&x;&y;."),
            "<p><a href=\"http://www.a.b\">www.a.b</a>&amp;x;&amp;y;.</p>\n"
        );
        // When the trim takes the whole path, it goes on into the domain
        // and the trimmed domain decides; otherwise the whole one does.
        for (md, want) in [
            ("www.a.b_)", "<a href=\"http://www.a.b\">www.a.b</a>_)"),
            (
                "www.a.b_&x;.",
                "<a href=\"http://www.a.b\">www.a.b</a>_&amp;x;.",
            ),
            ("www.a.b_?/", "www.a.b_?/"),
            ("www.a.b_&x", "www.a.b_&amp;x"),
            ("www.a.b_;", "www.a.b_;"),
            ("www.a_b.c.?", "www.a_b.c.?"),
            (
                "www.a_b.c./",
                "<a href=\"http://www.a_b.c./\">www.a_b.c./</a>",
            ),
        ] {
            assert_eq!(gfm(md), format!("<p>{want}</p>\n"), "{md:?}");
        }
    }

    #[test]
    fn parens_balanced() {
        assert_eq!(
            gfm("www.a.b/(c)"),
            "<p><a href=\"http://www.a.b/(c)\">www.a.b/(c)</a></p>\n"
        );
        assert_eq!(
            gfm("(www.a.b/c))."),
            "<p>(<a href=\"http://www.a.b/c\">www.a.b/c</a>)).</p>\n"
        );
        assert_eq!(
            gfm("www.a.b/((c)"),
            "<p><a href=\"http://www.a.b/((c)\">www.a.b/((c)</a></p>\n"
        );
        // Only a trailing `)` is checked.
        assert_eq!(
            gfm("www.a.b/c)d"),
            "<p><a href=\"http://www.a.b/c)d\">www.a.b/c)d</a></p>\n"
        );
        // Punctuation and parentheses come off in any order.
        assert_eq!(
            gfm("(www.a.b/c.)"),
            "<p>(<a href=\"http://www.a.b/c\">www.a.b/c</a>.)</p>\n"
        );
    }

    #[test]
    fn email_autolink() {
        assert_eq!(
            gfm_inlines("x foo.bar+1@ex-a_m.ple.org."),
            [
                Inline::Text("x ".into()),
                auto_link(
                    "mailto:foo.bar+1@ex-a_m.ple.org",
                    "foo.bar+1@ex-a_m.ple.org"
                ),
                Inline::Text(".".into()),
            ]
        );
        for no in ["a@b", "@b.c", "a@b.c-", "a@b.c_", "a@.b.c", "a@b+c.d"] {
            assert_eq!(gfm(no), format!("<p>{no}</p>\n"), "{no:?}");
        }
        assert_eq!(
            gfm("a@b.c1"),
            "<p><a href=\"mailto:a@b.c1\">a@b.c1</a></p>\n"
        );
        // A second `@` spoils the first address; the second can still be one.
        assert_eq!(
            gfm("a@b.c@d.e"),
            "<p>a@<a href=\"mailto:b.c@d.e\">b.c@d.e</a></p>\n"
        );
        // Recognised in any text: across emphasis delimiters and entities
        // that stayed text, after any character.
        assert_eq!(
            gfm("a_b@c.d x/e&#64;f.g"),
            "<p><a href=\"mailto:a_b@c.d\">a_b@c.d</a> x/<a href=\"mailto:e@f.g\">e@f.g</a></p>\n"
        );
        assert_eq!(
            gfm("*a@b.c* a@b.c a@b.c"),
            "<p><em><a href=\"mailto:a@b.c\">a@b.c</a></em> \
             <a href=\"mailto:a@b.c\">a@b.c</a> <a href=\"mailto:a@b.c\">a@b.c</a></p>\n"
        );
    }

    #[test]
    fn not_inside_code_or_link() {
        assert_eq!(
            gfm("`www.a.b a@b.c`"),
            "<p><code>www.a.b a@b.c</code></p>\n"
        );
        assert_eq!(
            gfm("[www.a.b a@b.c http://a.b](u)"),
            "<p><a href=\"u\">www.a.b a@b.c http://a.b</a></p>\n"
        );
        assert_eq!(gfm("[x](www.a.b)"), "<p><a href=\"www.a.b\">x</a></p>\n");
        assert_eq!(
            gfm("![www.a.b a@b.c](u)"),
            "<p><img src=\"u\" alt=\"www.a.b a@b.c\" /></p>\n"
        );
        // An open bracket that never closes still holds www and URL links
        // off (as cmark-gfm does); a bare email is found in its text.
        assert_eq!(gfm("[www.a.b"), "<p>[www.a.b</p>\n");
        assert_eq!(
            gfm("<a title=\"www.a.b\"> <x@y.z>"),
            "<p><a title=\"www.a.b\"> <a href=\"mailto:x@y.z\">x@y.z</a></p>\n"
        );
        // A CommonMark `<...>` autolink stays one, not an extended one.
        assert_eq!(
            gfm_inlines("<http://a.b>"),
            [Inline::Link {
                dest: "http://a.b".into(),
                title: String::new(),
                content: vec![Inline::Text("http://a.b".into())],
                auto: false,
            }]
        );
        // CommonMark reads none of them.
        let cm = md::read_with(b"www.a.b http://a.b a@b.c", Dialect::CommonMark).unwrap();
        assert_eq!(html::write(&cm), "<p>www.a.b http://a.b a@b.c</p>\n");
    }

    #[test]
    fn extended_autolink_is_charged() {
        for input in [&b"www.a.b"[..], b"http://a.b", b"a@b.c"] {
            let read = |nodes: usize, dialect| {
                let limits = crate::Limits {
                    nodes,
                    ..crate::Limits::DEFAULT
                };
                md::read_limited_with(input, limits, dialect)
            };
            let need = (1..).find(|&n| read(n, Dialect::Gfm).is_ok()).unwrap();
            assert_eq!(
                read(need - 1, Dialect::Gfm),
                Err(crate::ReadError::Limit(crate::LimitError::Exceeded(
                    "nodes"
                )))
            );
            let plain = (1..)
                .find(|&n| read(n, Dialect::CommonMark).is_ok())
                .unwrap();
            assert!(need > plain, "{input:?}: {need} <= {plain}");
            // The destination is a second copy of the text: charged too.
            let text = |text_bytes: usize| {
                let limits = crate::Limits {
                    text_bytes,
                    ..crate::Limits::DEFAULT
                };
                md::read_limited_with(input, limits, Dialect::Gfm).is_ok()
            };
            let need = (0..).find(|&n| text(n)).unwrap();
            assert!(need >= 2 * input.len(), "{input:?}: {need}");
        }
    }

    /// Bytes the extended-autolink scans look at for `raw` (GFM).
    fn autolink_steps(raw: &str) -> usize {
        let mut m = Meter::new(crate::Limits::UNLIMITED);
        let refs = Refs::new();
        let mut p = Parser::new(raw, &refs, Dialect::Gfm, &mut m);
        p.parse();
        assert!(!p.m.failed());
        p.auto_steps
    }

    #[test]
    fn extended_autolinks_stay_linear() {
        // Step counts, not time: a scan that restarted per candidate (the
        // domain from each `www.`, the parentheses per trailing `)`, the
        // address from each `@`) would look at ~n^2/2 bytes here.
        let n = 2_000;
        let inputs = [
            "www.a_".repeat(n),
            format!("x{}", "_www.a_b".repeat(n)),
            "www.".repeat(n),
            format!("www.{}", "a.".repeat(n)),
            format!("www.a.b/{}", ")".repeat(n)),
            format!("{}www.a.b/{}", "(".repeat(n), ")".repeat(n)),
            format!("www.a.b{}", ".,".repeat(n)),
            format!("www.a.b{}", "&a;".repeat(n)),
            "http://a.b<".repeat(n),
            "http://a_b.c_d:".repeat(n),
            "a@".repeat(n),
            "a@b.c".repeat(n),
            format!("{}@", "a.".repeat(n)),
            format!("a@{}", "b.".repeat(n)),
            "a@b.c@".repeat(n),
            "(www.a.b) ".repeat(n),
        ];
        for raw in &inputs {
            assert_linear_steps(raw);
        }
    }

    /// Fails unless the autolink scans of `raw` look at no more than four
    /// times its bytes.
    fn assert_linear_steps(raw: &str) {
        let steps = autolink_steps(raw);
        assert!(
            steps <= 4 * raw.len(),
            "{} steps for {} bytes of {:?}...",
            steps,
            raw.len(),
            &raw[..12]
        );
    }

    #[test]
    fn rejected_paths_are_not_rescanned() {
        // A domain that is valid only when trimmed (or only in full) with a
        // path that decides against it: a path scan per candidate, each
        // to the next whitespace, would look at ~n^2/2 bytes. Checked up
        // to the 8 MiB the default limits accept, where that is hours.
        let shapes: [fn(usize) -> String; 9] = [
            |k| "www.a.b_/(".repeat(k),
            |k| "(http://a.b_/".repeat(k),
            |k| "www.a.b_*".repeat(k),
            |k| "www.a.b_~".repeat(k),
            |k| "www.a.b_)(".repeat(k),
            |k| "_www.a.b_&a".repeat(k),
            |k| "(www.a.b_;".repeat(k),
            |k| "www.a_b.c.*".repeat(k),
            |k| "(ftp://a.b_)x".repeat(k),
        ];
        for size in [16 << 10, 8 << 20] {
            for shape in &shapes {
                let raw = shape(size / shape(1).len());
                assert_linear_steps(&raw);
            }
        }
    }

    /// Opener candidates `process_emphasis` looks at for `raw` in GFM.
    fn emphasis_steps(raw: &str) -> usize {
        let mut m = Meter::new(crate::Limits::UNLIMITED);
        let refs = Refs::new();
        let mut p = Parser::new(raw, &refs, Dialect::Gfm, &mut m);
        p.parse();
        p.steps
    }

    #[test]
    fn tilde_runs_stay_linear() {
        // Step counts, not time: a search that restarted from the top per
        // closer would look at ~n^2/2 candidates here.
        let n = 2_000;
        let inputs = [
            "~~a ".repeat(n),
            "a~~ ".repeat(n),
            "~~".repeat(n),
            "~".repeat(n),
            "~~~ ".repeat(n),
            format!("{}{}", "*a ".repeat(n), "a~~ ".repeat(n)),
            format!("{}{}", "~~a ".repeat(n), "a* ".repeat(n)),
            format!("{}{}", "~~a ".repeat(n), "a~~".repeat(n)),
            "~~a*".repeat(n),
            "[~~a".repeat(n),
        ];
        for raw in &inputs {
            let steps = emphasis_steps(raw);
            assert!(steps <= 4 * n, "{} steps for {:?}...", steps, &raw[..12]);
        }
    }

    #[test]
    fn punct_table_is_sorted() {
        assert!(PUNCT.windows(2).all(|w| w[0].1 < w[1].0));
        assert!(PUNCT.iter().all(|&(lo, hi)| lo <= hi && !lo.is_ascii()));
        assert!(is_punct('€') && is_punct('¡') && is_punct('—') && is_punct('$'));
        assert!(!is_punct('a') && !is_punct('é') && !is_punct(' '));
    }

    #[test]
    fn email_charge_is_the_destination_plus_the_address() {
        // Reading as GFM costs, over CommonMark, exactly the destination
        // (`mailto:` and the address) per address: 12 for `a@b.c`.
        for (input, addresses, bytes) in
            [("a@b.c", 1, 5), ("x a@b.cc y", 1, 6), ("a@b.c a@b.c", 2, 5)]
        {
            let need = |dialect| {
                (0..)
                    .find(|&text_bytes| {
                        let limits = crate::Limits {
                            text_bytes,
                            ..crate::Limits::DEFAULT
                        };
                        md::read_limited_with(input.as_bytes(), limits, dialect).is_ok()
                    })
                    .unwrap()
            };
            assert_eq!(
                need(Dialect::Gfm) - need(Dialect::CommonMark),
                addresses * ("mailto:".len() + bytes),
                "{input:?}"
            );
        }
    }

    #[test]
    fn autolink_steps_count_what_is_scanned() {
        // The bound in `extended_autolinks_stay_linear` is only worth
        // something if the counter counts: pin it on small inputs, each
        // reaching one of the places that add to it.
        // The domain run (7 bytes) and one look at the empty path.
        assert_eq!(autolink_steps("www.a.b"), 8);
        // The run and the stop byte, then an entity look-alike (`&`, its
        // three letters) in the path scan.
        assert_eq!(autolink_steps("www.a.b&amp;"), 13);
        // A path that stays: scanned once, then trimmed from the end,
        // an entity look-alike counting its letters.
        assert_eq!(autolink_steps("www.a.b/x),."), 18);
        assert_eq!(autolink_steps("www.a.b/x&amp;"), 21);
        assert_eq!(autolink_steps("http://a.b/p?q=1)."), 16);
        // Addresses: the bytes up to each `@`, its local part going back,
        // the domain going forward.
        assert_eq!(autolink_steps("a@b.c"), 7);
        assert_eq!(autolink_steps("x a.b@c.de y"), 14);
        assert_eq!(autolink_steps("a@b@c"), 10);
        // One opener candidate per closer in emphasis.
        assert_eq!(emphasis_steps("*a* **b**"), 2);
    }

    #[test]
    fn extended_autolink_domain_edges() {
        let link = |t: &str| format!("<a href=\"http://{t}\">{t}</a>");
        // A `www.` with no segment after its period is no domain, even
        // when an earlier period in the same run has one.
        assert_eq!(gfm("x.y_www."), "<p>x.y_www.</p>\n");
        assert_eq!(gfm("www.x_www."), "<p>www.x_www.</p>\n");
        // A second candidate inside a run: its own segment `www` holds no
        // underscore, whatever the segments before it hold.
        assert_eq!(
            gfm("www.x_www.c"),
            format!("<p>www.x_{}</p>\n", link("www.c"))
        );
        assert_eq!(gfm("x.y_www.c"), format!("<p>x.y_{}</p>\n", link("www.c")));
        // `www.` must be the whole start of a candidate, and `://` must
        // follow a scheme: neither a bare `w` nor a bare `:` is one.
        assert_eq!(gfm("w.a.b"), "<p>w.a.b</p>\n");
        assert_eq!(gfm("http:abc.d.e"), "<p>http:abc.d.e</p>\n");
        assert_eq!(gfm("ww.a.b.c"), "<p>ww.a.b.c</p>\n");
        // A trailing entity look-alike of any length goes, the link ending
        // where its `&` begins.
        for (entity, tail) in [
            ("&a;", "&amp;a;"),
            ("&ab;", "&amp;ab;"),
            ("&amp;", "&amp;"),
            ("&abcdefgh;", "&amp;abcdefgh;"),
        ] {
            assert_eq!(
                gfm(&format!("www.a.b/x{entity}")),
                format!("<p>{}{tail}</p>\n", link("www.a.b/x")),
                "{entity}"
            );
        }
        // An entity look-alike longer than half of what precedes it still
        // goes from the end of the link.
        assert_eq!(
            gfm("www.a.b&aaaaaaaaaaaa;"),
            format!("<p>{}&amp;aaaaaaaaaaaa;</p>\n", link("www.a.b"))
        );
    }

    #[test]
    fn closers_of_different_length_mod_3_keep_their_own_search_floor() {
        // `**` is both-flanking and its run length is 2 mod 3, the closing
        // `*` is 1 mod 3: the search floors kept per class must not be
        // shared, or the `*` would give up before it reaches the opener.
        let want = "<p><em>a**a</em>a</p>\n";
        assert_eq!(html("*a**a*a"), want);
        assert_eq!(gfm("*a**a*a"), want);
    }

    #[test]
    fn email_autolink_needs_a_clean_domain() {
        // A character that is neither a letter, digit, hyphen, period nor
        // the closing bracket ends the scan: no address.
        assert_eq!(html("<a@b$c>"), "<p>&lt;a@b$c&gt;</p>\n");
        assert_eq!(
            html("<a@b.c>"),
            "<p><a href=\"mailto:a@b.c\">a@b.c</a></p>\n"
        );
    }
}
