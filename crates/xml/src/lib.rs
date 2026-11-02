//! A non-validating XML 1.0 (fifth edition) pull parser with Namespaces in
//! XML 1.0, for documents that are foreign input (book chapters, package
//! files, subtitles). No dependencies, no `unsafe`, no I/O.
//!
//! - **No external resource is ever loaded.** A `<!DOCTYPE>` is read but its
//!   external identifier is only checked for syntax; the parser has no file
//!   or network access at all.
//! - **No entity is expanded from a DTD.** The internal subset is skipped
//!   lexically (literals, comments and processing instructions are honoured,
//!   so a `]>` inside one does not end it); a reference to an entity it
//!   declares is `ErrorKind::EntityDeclarations`, so entity bombs cannot
//!   exist. Known references: the five predefined entities, numeric
//!   character references (to characters XML 1.0 allows), and, only with
//!   `Options::html_entities`, the HTML5 named characters (`entity`).
//! - **Strict:** anything not well-formed (and not namespace-well-formed)
//!   is an `Error` with the line where it was found; there is no recovery.
//!   Attribute defaults declared in the internal subset are not applied.
//! - **Bounded:** element nesting is limited by `Options::max_depth`; the
//!   work is linear in the input length (`Reader::steps` counts every byte
//!   examined, copied, hashed or compared; a namespace name is hashed when
//!   its declaration comes into and goes out of scope, and the
//!   duplicate-attribute check compares its id, never the name); names and
//!   values borrow from the input, and memory is allocated only for a text
//!   or attribute value that a reference or a line-end normalization
//!   changes, the attribute list of an element, the open-element stack and
//!   the namespace bindings in scope.
//!
//! `decode` turns the bytes into text (UTF-8, or UTF-16 with a byte order
//! mark) and checks the declared encoding; `Reader` reads the text.

mod entities;
#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Default element nesting limit. 256 is the Markdown reader's nesting
/// limit (`MAX_DEPTH` in the doc crate) so that a document converts
/// between the formats without one of them refusing a depth the other
/// accepted; real XHTML chapters nest a few dozen levels at most. Past it
/// the document is `ErrorKind::TooDeep`.
pub const MAX_DEPTH: u32 = 256;

/// The namespace the `xml` prefix is bound to by definition.
pub const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
/// The namespace of the `xmlns` attributes; it may never be declared.
pub const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

/// Attribute count up to which duplicates are found by comparing every
/// pair: at most 8 * 7 / 2 = 28 comparisons, cheaper than a hash set for
/// the few attributes real elements carry. Above it a hash set keeps the
/// check linear in the attribute count.
const PAIRWISE_ATTRS: usize = 8;

/// Comparisons one binary search of the HTML entity table makes at most:
/// the bit length of the table size (2,125 names -> 12).
const ENTITY_LOOKUP_STEPS: u64 = (usize::BITS - entities::ENTITIES.len().leading_zeros()) as u64;

/// Upper bound of `Reader::steps` per input byte, for the linearity tests
/// and the fuzz target. Each byte is counted by the character check (1)
/// and by the lexer as it moves past it (1). A name byte is counted again
/// when the name is resolved, matched against an end tag or unbound (1
/// each, at most 2 of them per name), and by the duplicate check: hashed
/// once, or compared with each of at most `PAIRWISE_ATTRS - 1` other names,
/// each comparison walking at most the shorter name, at most 3.5 per byte
/// summed over the pairs. A value byte is normalized and copied (2); a
/// namespace name is hashed and compared once when bound and again when
/// unbound (4). A reference adds at most `ENTITY_LOOKUP_STEPS` comparisons
/// for at least 3 bytes (`&x;`), 4 per byte. That is at most 7.5 per byte;
/// 16 leaves room for constant per-construct overheads, while a quadratic
/// path at the tests' sizes (64,000 units, or a 1 MiB namespace name in
/// 8,192 elements) exceeds it by orders of magnitude.
#[doc(hidden)]
pub const STEPS_PER_BYTE: u64 = 16;

/// How the reader treats what XML leaves open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Most elements open at once; one more is `ErrorKind::TooDeep`.
    pub max_depth: u32,
    /// Accept the HTML5 named character references (`&nbsp;`, `&eacute;`)
    /// as XHTML readers do. Without it they are undefined entities.
    pub html_entities: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_depth: MAX_DEPTH,
            html_entities: false,
        }
    }
}

/// Why a document was refused, and the line (1-based) where it was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub line: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Not well-formed (or not namespace-well-formed); the text says what.
    NotWellFormed(&'static str),
    /// More elements open at once than `Options::max_depth`.
    TooDeep,
    /// The declared encoding is neither UTF-8 nor UTF-16.
    UnsupportedEncoding(String),
    /// A reference to an entity that a DTD declares (or may declare):
    /// declarations are never expanded.
    EntityDeclarations,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ErrorKind::NotWellFormed(what) => {
                write!(f, "not well-formed XML (line {}): {what}", self.line)
            }
            ErrorKind::TooDeep => write!(f, "elements nested too deeply (line {})", self.line),
            ErrorKind::UnsupportedEncoding(name) => write!(f, "unsupported encoding {name}"),
            ErrorKind::EntityDeclarations => write!(f, "entity declarations are not supported"),
        }
    }
}

impl std::error::Error for Error {}

/// An element or attribute name: the namespace URI its prefix (or the
/// default namespace, for elements) is bound to, and the local part.
/// The URI is a slice of the declaration, as long as the input made it:
/// compare it with a known constant (`Element::attr` does), not with
/// another name's URI once per name, which walks a long URI each time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct QName<'a> {
    pub ns: Option<&'a str>,
    pub local: &'a str,
}

/// A start tag. Namespace declarations (`xmlns`, `xmlns:p`) are applied,
/// not listed among the attributes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element<'a> {
    pub name: QName<'a>,
    attrs: Vec<(QName<'a>, Cow<'a, str>)>,
}

impl<'a> Element<'a> {
    /// The attributes in document order, values normalized.
    pub fn attrs(&self) -> impl Iterator<Item = (QName<'a>, &Cow<'a, str>)> {
        self.attrs.iter().map(|(n, v)| (*n, v))
    }

    /// The value of the attribute with this namespace and local name.
    pub fn attr(&self, ns: Option<&str>, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| n.ns == ns && n.local == local)
            .map(|(_, v)| v.as_ref())
    }
}

/// What the reader found next. An empty element is a `Start` and an `End`;
/// CDATA sections are text; adjacent text, references and CDATA sections
/// (with comments and processing instructions between them skipped) are one
/// `Text`. Comments, processing instructions, the declaration, the DOCTYPE
/// and whitespace outside the root element produce no event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event<'a> {
    Start(Element<'a>),
    End(QName<'a>),
    Text(Cow<'a, str>),
}

/// Turns the bytes of a document into text: UTF-8 (with or without a byte
/// order mark; borrowed) or UTF-16 with a byte order mark (copied). A
/// declared encoding other than UTF-8 or UTF-16 is
/// `ErrorKind::UnsupportedEncoding`; a declaration that contradicts the
/// bytes, invalid UTF-8 or UTF-16, or UTF-16 without a byte order mark is
/// not well-formed.
pub fn decode(bytes: &[u8]) -> Result<Cow<'_, str>, Error> {
    let (text, utf16): (Cow<'_, str>, Option<&str>) =
        if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
            (Cow::Borrowed(utf8(rest)?), None)
        } else if let Some(rest) = bytes.strip_prefix(b"\xFF\xFE") {
            (
                Cow::Owned(utf16(rest, u16::from_le_bytes)?),
                Some("UTF-16LE"),
            )
        } else if let Some(rest) = bytes.strip_prefix(b"\xFE\xFF") {
            (
                Cow::Owned(utf16(rest, u16::from_be_bytes)?),
                Some("UTF-16BE"),
            )
        } else if bytes.starts_with(b"\0<") || bytes.starts_with(b"<\0") {
            return Err(Error {
                kind: ErrorKind::NotWellFormed("UTF-16 without a byte order mark"),
                line: 1,
            });
        } else {
            (Cow::Borrowed(utf8(bytes)?), None)
        };
    let decl = xml_decl(&text).map_err(|(what, at)| Error {
        kind: ErrorKind::NotWellFormed(what),
        line: line_of(&text, at),
    })?;
    if let Some(enc) = decl.and_then(|d| d.encoding) {
        let ok = match utf16 {
            None => enc.eq_ignore_ascii_case("UTF-8"),
            Some(form) => enc.eq_ignore_ascii_case("UTF-16") || enc.eq_ignore_ascii_case(form),
        };
        if !ok {
            let unicode = ["UTF-8", "UTF-16", "UTF-16LE", "UTF-16BE"]
                .iter()
                .any(|u| enc.eq_ignore_ascii_case(u));
            let kind = if unicode {
                ErrorKind::NotWellFormed("the declared encoding does not match the bytes")
            } else {
                ErrorKind::UnsupportedEncoding(enc.to_string())
            };
            return Err(Error { kind, line: 1 });
        }
    }
    Ok(text)
}

fn utf8(b: &[u8]) -> Result<&str, Error> {
    std::str::from_utf8(b).map_err(|e| {
        let good = &b[..e.valid_up_to()];
        Error {
            kind: ErrorKind::NotWellFormed("invalid UTF-8"),
            // Valid by construction: the prefix before the error.
            line: line_of(std::str::from_utf8(good).unwrap_or(""), good.len()),
        }
    })
}

fn utf16(b: &[u8], unit: fn([u8; 2]) -> u16) -> Result<String, Error> {
    let bad = |s: &str| Error {
        kind: ErrorKind::NotWellFormed("invalid UTF-16"),
        line: line_of(s, s.len()),
    };
    // At most 3 UTF-8 bytes per 2-byte unit; the input size is a sound
    // first guess that grows at most once or twice.
    let mut out = String::with_capacity(b.len());
    if !b.len().is_multiple_of(2) {
        return Err(bad(""));
    }
    let units = b.as_chunks::<2>().0.iter().map(|&c| unit(c));
    for c in char::decode_utf16(units) {
        match c {
            Ok(c) => out.push(c),
            Err(_) => return Err(bad(&out)),
        }
    }
    Ok(out)
}

/// 1-based line of byte offset `at` in `s`: line ends are `\n`, `\r\n`
/// and a lone `\r`, as XML normalizes them.
fn line_of(s: &str, at: usize) -> u32 {
    let b = &s.as_bytes()[..at.min(s.len())];
    let mut n: u32 = 1;
    for (i, &c) in b.iter().enumerate() {
        if c == b'\n' || (c == b'\r' && s.as_bytes().get(i + 1) != Some(&b'\n')) {
            n = n.saturating_add(1);
        }
    }
    n
}

/// The XML declaration's fields, and the byte offset just past it.
struct Decl<'a> {
    encoding: Option<&'a str>,
    standalone: bool,
    end: usize,
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_space(b: &[u8], mut i: usize) -> usize {
    while b.get(i).copied().is_some_and(is_space) {
        i += 1;
    }
    i
}

/// Reads `<?xml version="1.x" encoding="..." standalone="..."?>` at the
/// very start of `s`. `Ok(None)`: there is none (a processing instruction
/// such as `<?xml-stylesheet` is not one). Errors carry the byte offset.
fn xml_decl(s: &str) -> Result<Option<Decl<'_>>, (&'static str, usize)> {
    let b = s.as_bytes();
    if !s.starts_with("<?xml") {
        return Ok(None);
    }
    match s[5..].chars().next() {
        Some(c) if c.is_ascii() && is_space(c as u8) => {}
        Some('?') => return Err(("an XML declaration without a version", 5)),
        Some(c) if is_name_char(c) => return Ok(None),
        _ => return Err(("a malformed XML declaration", 5)),
    }
    // One pseudo-attribute `name = "value"` after required whitespace;
    // `Ok(None)` (nothing consumed) when the next one is another name.
    let pseudo = |i: &mut usize, name: &str| -> Result<Option<&str>, (&'static str, usize)> {
        let j = skip_space(b, *i);
        if j == *i || !s[j..].starts_with(name) {
            return Ok(None);
        }
        let mut k = skip_space(b, j + name.len());
        if b.get(k) != Some(&b'=') {
            return Err(("a malformed XML declaration", k));
        }
        k = skip_space(b, k + 1);
        let q = match b.get(k) {
            Some(&q @ (b'"' | b'\'')) => q,
            _ => return Err(("a malformed XML declaration", k)),
        };
        let v0 = k + 1;
        let Some(len) = b[v0..].iter().position(|&c| c == q) else {
            return Err(("a malformed XML declaration", k));
        };
        *i = v0 + len + 1;
        Ok(Some(&s[v0..v0 + len]))
    };
    let mut i = 5;
    let Some(version) = pseudo(&mut i, "version")? else {
        return Err(("an XML declaration without a version", i));
    };
    let vb = version.as_bytes();
    if !(vb.len() > 2 && vb.starts_with(b"1.") && vb[2..].iter().all(u8::is_ascii_digit)) {
        return Err(("an XML version other than 1.x", i));
    }
    let encoding = pseudo(&mut i, "encoding")?;
    if let Some(e) = encoding {
        let eb = e.as_bytes();
        let ok = eb.first().is_some_and(u8::is_ascii_alphabetic)
            && eb
                .iter()
                .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'));
        if !ok {
            return Err(("a malformed encoding name", i));
        }
    }
    let standalone = match pseudo(&mut i, "standalone")? {
        None => false,
        Some("yes") => true,
        Some("no") => false,
        Some(_) => return Err(("standalone other than yes or no", i)),
    };
    let i = skip_space(b, i);
    if !s[i..].starts_with("?>") {
        return Err(("a malformed XML declaration", i));
    }
    Ok(Some(Decl {
        encoding,
        standalone,
        end: i + 2,
    }))
}

/// XML 1.0 fifth edition `NameStartChar`.
fn is_name_start(c: char) -> bool {
    matches!(c,
        ':' | 'A'..='Z' | '_' | 'a'..='z'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}' | '\u{200C}'..='\u{200D}'
        | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}' | '\u{10000}'..='\u{EFFFF}')
}

/// XML 1.0 fifth edition `NameChar`.
fn is_name_char(c: char) -> bool {
    is_name_start(c)
        || matches!(c,
            '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}

/// XML 1.0 `Char`, the characters a document (or a character reference)
/// may hold. Surrogates cannot occur in a `char`.
fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{FFFD}' | '\u{10000}'..)
}

/// The HTML5 named character reference `&name;` (without `&` and `;`),
/// e.g. `entity("eacute") == Some("é")`; some names stand for two
/// characters. Binary search of the WHATWG table.
pub fn entity(name: &str) -> Option<&'static str> {
    entities::ENTITIES
        .binary_search_by(|(e, _)| (*e).cmp(name))
        .ok()
        .map(|k| entities::ENTITIES[k].1)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing read yet: the character check and the declaration.
    Start,
    /// Before the root element.
    Prolog,
    /// Inside the root element.
    Content,
    /// After the root element: only comments, PIs and whitespace.
    Epilog,
    Done,
}

/// An open element: its name as written (the end tag must repeat it), as
/// resolved, and how many namespace bindings it declared.
struct Open<'a> {
    raw: &'a str,
    name: QName<'a>,
    bindings: usize,
}

/// An attribute as written; `at` is the byte offset of its value.
#[derive(Clone, Copy)]
struct RawAttr<'a> {
    name: &'a str,
    name_at: usize,
    value: &'a str,
    at: usize,
    /// The value holds `&`, `\t`, `\n` or `\r`: normalization changes it.
    special: bool,
}

/// What a reference stands for.
enum Ref {
    Char(char),
    Str(&'static str),
}

/// The duplicate-attribute key: namespace declarations by prefix (`""` for
/// the default), other attributes by expanded name with the namespace
/// name as its id (`Reader::uris`), so a check never walks a namespace
/// name: a long one would be walked once per attribute in its scope.
#[derive(PartialEq, Eq, Hash)]
enum Key<'a> {
    Decl(&'a str),
    Attr(Option<usize>, &'a str),
}

impl Key<'_> {
    /// The bytes hashing or comparing the key walks at most, plus one for
    /// the discriminant and the id.
    fn cost(&self) -> u64 {
        (match self {
            Key::Decl(p) => p.len(),
            Key::Attr(_, l) => l.len(),
        } + 1) as u64
    }
}

/// The id of the namespace the `xml` prefix is bound to by definition;
/// declared namespace names get ids from `FIRST_ID` on.
const XML_ID: usize = 0;
const FIRST_ID: usize = 1;

/// A namespace name in scope: how many bindings name it, and its id.
struct Uri {
    id: usize,
    bindings: usize,
}

/// Text being gathered for one `Event::Text`: borrowed while it is one
/// unchanged slice of the input, owned once anything is added or changed.
enum Acc<'a> {
    Empty,
    Borrowed(&'a str),
    Owned(String),
}

type Res<T> = Result<T, Error>;

/// The pull parser. `next` returns events until `Ok(None)` (the document
/// ended well-formed) or an error; after an error every call returns it
/// again.
pub struct Reader<'a> {
    s: &'a str,
    pos: usize,
    opts: Options,
    phase: Phase,
    stack: Vec<Open<'a>>,
    /// The last start tag was empty (`<a/>`): its end comes next.
    close_empty: bool,
    /// Prefix (`""` = default namespace) -> the URIs bound to it, innermost
    /// last, each with its id; `""` as a default namespace URI means "no
    /// namespace".
    ns: HashMap<&'a str, Vec<(&'a str, usize)>>,
    /// The namespace names bound in scope, each hashed once per binding:
    /// equal names (even bound to different prefixes) share one id, so
    /// the duplicate check compares ids. A name leaves when its last
    /// binding goes out of scope, so this holds no more than the bindings
    /// in scope.
    uris: HashMap<&'a str, Uri>,
    /// The next id for a namespace name; ids are not reused, so one never
    /// names two different names.
    next_id: usize,
    /// The prefixes bound by the open elements, in binding order.
    undo: Vec<&'a str>,
    /// Scratch lists of the current start tag's attributes and their
    /// duplicate-check keys (with the byte offset of the attribute's
    /// name), kept to reuse their memory.
    raw: Vec<RawAttr<'a>>,
    keys: Vec<(Key<'a>, usize)>,
    /// General entities the internal subset declares (never expanded).
    declared: HashSet<&'a str>,
    /// Entity declarations may exist that the reader did not see (an
    /// external subset or parameter entity references, and not
    /// `standalone="yes"`).
    unseen: bool,
    standalone: bool,
    seen_doctype: bool,
    steps: u64,
    /// Byte offset where the last event began (for `line`).
    event_at: usize,
    /// Line-number cache: (byte offset, line there). Offsets asked for
    /// only grow, so counting from the cached point keeps it linear.
    line_cache: Cell<(usize, u32)>,
    err: Option<Error>,
}

impl<'a> Reader<'a> {
    pub fn new(text: &'a str, opts: Options) -> Reader<'a> {
        Reader {
            s: text,
            pos: 0,
            opts,
            phase: Phase::Start,
            stack: Vec::new(),
            close_empty: false,
            ns: HashMap::new(),
            uris: HashMap::new(),
            next_id: FIRST_ID,
            undo: Vec::new(),
            raw: Vec::new(),
            keys: Vec::new(),
            declared: HashSet::new(),
            unseen: false,
            standalone: false,
            seen_doctype: false,
            steps: 0,
            event_at: 0,
            line_cache: Cell::new((0, 1)),
            err: None,
        }
    }

    /// The next event, `Ok(None)` at the well-formed end of the document.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Event<'a>>, Error> {
        if let Some(e) = &self.err {
            return Err(e.clone());
        }
        let r = self.step();
        if let Err(e) = &r {
            self.err = Some(e.clone());
            self.phase = Phase::Done;
        }
        r
    }

    /// The line (1-based) where the last event began.
    pub fn line(&self) -> u32 {
        self.line_at(self.event_at)
    }

    /// Work done so far, in units of one byte examined, copied or hashed,
    /// or one comparison: the linearity tests bound it by the input length.
    #[doc(hidden)]
    pub fn steps(&self) -> u64 {
        self.steps
    }

    fn line_at(&self, at: usize) -> u32 {
        let (mut p, mut n) = self.line_cache.get();
        if at < p {
            (p, n) = (0, 1);
        }
        let b = self.s.as_bytes();
        let at = at.min(b.len());
        for i in p..at {
            if b[i] == b'\n' || (b[i] == b'\r' && b.get(i + 1) != Some(&b'\n')) {
                n = n.saturating_add(1);
            }
        }
        self.line_cache.set((at, n));
        n
    }

    fn fail(&self, what: &'static str, at: usize) -> Error {
        Error {
            kind: ErrorKind::NotWellFormed(what),
            line: self.line_at(at),
        }
    }

    fn err<T>(&self, what: &'static str, at: usize) -> Res<T> {
        Err(self.fail(what, at))
    }

    fn kind<T>(&self, kind: ErrorKind, at: usize) -> Res<T> {
        Err(Error {
            kind,
            line: self.line_at(at),
        })
    }

    fn b(&self) -> &'a [u8] {
        self.s.as_bytes()
    }

    fn at(&self, lit: &str) -> bool {
        self.s.as_bytes()[self.pos..].starts_with(lit.as_bytes())
    }

    fn peek(&self) -> Option<u8> {
        self.b().get(self.pos).copied()
    }

    /// Skips whitespace; true if there was any.
    fn space(&mut self) -> bool {
        let p = skip_space(self.b(), self.pos);
        self.steps += (p - self.pos) as u64;
        let any = p > self.pos;
        self.pos = p;
        any
    }

    fn expect(&mut self, lit: &str, what: &'static str) -> Res<()> {
        if !self.at(lit) {
            return self.err(what, self.pos);
        }
        self.pos += lit.len();
        self.steps += lit.len() as u64;
        Ok(())
    }

    /// Moves past the first `lit` at or after the current position.
    fn past(&mut self, lit: &str, what: &'static str, at: usize) -> Res<usize> {
        match self.s[self.pos..].find(lit) {
            Some(k) => {
                let start = self.pos + k;
                self.steps += (k + lit.len()) as u64;
                self.pos = start + lit.len();
                Ok(start)
            }
            None => {
                self.steps += (self.s.len() - self.pos) as u64;
                self.err(what, at)
            }
        }
    }

    /// A `Name` at the current position.
    fn name(&mut self) -> Res<&'a str> {
        let start = self.pos;
        let mut chars = self.s[start..].chars();
        match chars.next() {
            Some(c) if is_name_start(c) => self.pos += c.len_utf8(),
            _ => return self.err("a name was expected", start),
        }
        for c in chars {
            if !is_name_char(c) {
                break;
            }
            self.pos += c.len_utf8();
        }
        self.steps += (self.pos - start) as u64;
        Ok(&self.s[start..self.pos])
    }

    fn step(&mut self) -> Res<Option<Event<'a>>> {
        loop {
            match self.phase {
                Phase::Start => self.start()?,
                Phase::Prolog | Phase::Epilog => {
                    self.space();
                    let prolog = self.phase == Phase::Prolog;
                    self.event_at = self.pos;
                    if self.pos == self.s.len() {
                        if prolog {
                            return self.err("no root element", self.pos);
                        }
                        self.phase = Phase::Done;
                        return Ok(None);
                    }
                    if self.at("<?") {
                        self.pi()?;
                    } else if self.at("<!--") {
                        self.comment()?;
                    } else if prolog && self.at("<!DOCTYPE") {
                        if self.seen_doctype {
                            return self.err("a second DOCTYPE", self.pos);
                        }
                        self.doctype()?;
                    } else if prolog
                        && self.peek() == Some(b'<')
                        && !matches!(self.b().get(self.pos + 1), Some(b'!' | b'/'))
                    {
                        self.phase = Phase::Content;
                        return self.start_tag().map(Some);
                    } else if prolog {
                        return self.err("content before the root element", self.pos);
                    } else {
                        return self.err("content after the root element", self.pos);
                    }
                }
                Phase::Content => {
                    if self.close_empty {
                        self.close_empty = false;
                        return Ok(Some(self.pop()));
                    }
                    self.event_at = self.pos;
                    if let Some(t) = self.text()? {
                        return Ok(Some(Event::Text(t)));
                    }
                    self.event_at = self.pos;
                    if self.pos == self.s.len() {
                        return self.err("the document ends inside an element", self.pos);
                    }
                    if self.at("</") {
                        return self.end_tag().map(Some);
                    }
                    if self.at("<!") {
                        return self.err("unexpected markup in content", self.pos);
                    }
                    return self.start_tag().map(Some);
                }
                Phase::Done => return Ok(None),
            }
        }
    }

    /// The character check over the whole text, the byte order mark and
    /// the XML declaration.
    fn start(&mut self) -> Res<()> {
        let b = self.b();
        self.steps += b.len() as u64;
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            let bad = (c < 0x20 && !matches!(c, b'\t' | b'\n' | b'\r'))
                // U+FFFE and U+FFFF: EF BF BE, EF BF BF.
                || (c == 0xEF && b.get(i + 1) == Some(&0xBF) && matches!(b.get(i + 2), Some(0xBE | 0xBF)));
            if bad {
                return self.err("a character XML does not allow", i);
            }
            i += 1;
        }
        if self.s.starts_with('\u{FEFF}') {
            self.pos = '\u{FEFF}'.len_utf8();
        }
        match xml_decl(&self.s[self.pos..]) {
            Ok(Some(d)) => {
                self.standalone = d.standalone;
                self.pos += d.end;
                self.steps += d.end as u64;
            }
            Ok(None) => {}
            Err((what, at)) => return self.err(what, self.pos + at),
        }
        self.phase = Phase::Prolog;
        Ok(())
    }

    /// `<!--` ... `-->`; `--` may not occur inside.
    fn comment(&mut self) -> Res<()> {
        let at = self.pos;
        self.pos += 4;
        let dashes = self.past("--", "an unclosed comment", at)?;
        if self.peek() != Some(b'>') {
            return self.err("-- inside a comment", dashes);
        }
        self.pos += 1;
        Ok(())
    }

    /// `<?target ...?>`; the target may not be `xml` (in any case) or hold
    /// a colon.
    fn pi(&mut self) -> Res<()> {
        let at = self.pos;
        self.pos += 2;
        let target = self.name()?;
        if target.eq_ignore_ascii_case("xml") {
            return self.err("an XML declaration not at the start", at);
        }
        if target.contains(':') {
            return self.err("a colon in a processing instruction target", at);
        }
        if self.at("?>") {
            self.pos += 2;
            return Ok(());
        }
        if !self.space() {
            return self.err("no space after a processing instruction target", self.pos);
        }
        self.past("?>", "an unclosed processing instruction", at)?;
        Ok(())
    }

    /// A quoted literal; returns its content.
    fn literal(&mut self, what: &'static str) -> Res<&'a str> {
        let q = match self.peek() {
            Some(q @ (b'"' | b'\'')) => q,
            _ => return self.err(what, self.pos),
        };
        let at = self.pos;
        self.pos += 1;
        let start = self.pos;
        match self.b()[start..].iter().position(|&c| c == q) {
            Some(n) => {
                self.steps += (n + 2) as u64;
                self.pos = start + n + 1;
                Ok(&self.s[start..start + n])
            }
            None => self.err(what, at),
        }
    }

    /// `<!DOCTYPE name ExternalID? [internal subset]? >`. The external
    /// identifier is only parsed: nothing is ever loaded.
    fn doctype(&mut self) -> Res<()> {
        self.seen_doctype = true;
        self.pos += "<!DOCTYPE".len();
        if !self.space() {
            return self.err("no space after DOCTYPE", self.pos);
        }
        self.name()?;
        let sp = self.space();
        let mut external = false;
        if self.at("SYSTEM") || self.at("PUBLIC") {
            if !sp {
                return self.err("no space before an external identifier", self.pos);
            }
            let public = self.at("PUBLIC");
            self.pos += 6;
            if !self.space() {
                return self.err("no space in an external identifier", self.pos);
            }
            if public {
                let at = self.pos;
                let id = self.literal("a malformed public identifier")?;
                let ok = id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b" \r\n-'()+,./:=?;!*#@$_%".contains(&c));
                if !ok {
                    return self.err("a character not allowed in a public identifier", at);
                }
                if !self.space() {
                    return self.err("no space after a public identifier", self.pos);
                }
            }
            self.literal("a malformed system identifier")?;
            external = true;
            self.space();
        }
        let mut pe_refs = false;
        if self.peek() == Some(b'[') {
            self.pos += 1;
            pe_refs = self.internal_subset()?;
            self.space();
        }
        self.expect(">", "an unclosed DOCTYPE")?;
        self.unseen = (external || pe_refs) && !self.standalone;
        Ok(())
    }

    /// Skips the internal subset up to and including its `]`, lexically:
    /// markup declarations (with their quoted literals), comments,
    /// processing instructions, parameter entity references and
    /// whitespace. Records the names of declared general entities.
    /// Returns whether a parameter entity reference occurred.
    fn internal_subset(&mut self) -> Res<bool> {
        let mut pe_refs = false;
        loop {
            self.space();
            if self.pos == self.s.len() {
                return self.err("an unclosed internal subset", self.pos);
            }
            if self.peek() == Some(b']') {
                self.pos += 1;
                return Ok(pe_refs);
            }
            if self.peek() == Some(b'%') {
                self.pos += 1;
                self.name()?;
                self.expect(";", "a malformed parameter entity reference")?;
                pe_refs = true;
            } else if self.at("<!--") {
                self.comment()?;
            } else if self.at("<?") {
                self.pi()?;
            } else if self.at("<!ENTITY") {
                self.pos += "<!ENTITY".len();
                if !self.space() {
                    return self.err("no space after ENTITY", self.pos);
                }
                let general = self.peek() != Some(b'%');
                if !general {
                    self.pos += 1;
                    if !self.space() {
                        return self.err("no space after %", self.pos);
                    }
                }
                let name = self.name()?;
                if general {
                    self.steps += name.len() as u64;
                    self.declared.insert(name);
                }
                self.declaration()?;
            } else if self.at("<!ELEMENT") || self.at("<!ATTLIST") || self.at("<!NOTATION") {
                self.pos += 2;
                self.name()?;
                self.declaration()?;
            } else {
                return self.err("unexpected content in the internal subset", self.pos);
            }
        }
    }

    /// The rest of a markup declaration up to its `>`, skipping quoted
    /// literals (which may hold `>`).
    fn declaration(&mut self) -> Res<()> {
        let at = self.pos;
        loop {
            match self.peek() {
                None => return self.err("an unclosed markup declaration", at),
                Some(b'>') => {
                    self.pos += 1;
                    self.steps += 1;
                    return Ok(());
                }
                Some(b'"' | b'\'') => {
                    self.literal("an unclosed literal")?;
                }
                Some(b'<') => return self.err("< inside a markup declaration", self.pos),
                Some(_) => {
                    self.pos += 1;
                    self.steps += 1;
                }
            }
        }
    }

    /// Text up to the next tag: character data, references, CDATA
    /// sections, with comments and PIs skipped. `None` if there is none.
    fn text(&mut self) -> Res<Option<Cow<'a, str>>> {
        let mut acc = Acc::Empty;
        let b = self.b();
        loop {
            let mut run = self.pos;
            while let Some(&c) = b.get(self.pos) {
                match c {
                    b'<' => break,
                    b'&' => {
                        self.push(&mut acc, &self.s[run..self.pos]);
                        let (r, end) = self.reference(self.pos)?;
                        match r {
                            Ref::Char(c) => self.push_owned(&mut acc, c.encode_utf8(&mut [0; 4])),
                            Ref::Str(s) => self.push_owned(&mut acc, s),
                        }
                        self.pos = end;
                        run = end;
                    }
                    b'\r' => {
                        self.push(&mut acc, &self.s[run..self.pos]);
                        self.push_owned(&mut acc, "\n");
                        self.pos += if b.get(self.pos + 1) == Some(&b'\n') {
                            2
                        } else {
                            1
                        };
                        self.steps += 1;
                        run = self.pos;
                    }
                    b']' if b[self.pos..].starts_with(b"]]>") => {
                        return self.err("]]> in text", self.pos);
                    }
                    _ => {
                        self.pos += 1;
                        self.steps += 1;
                    }
                }
            }
            self.push(&mut acc, &self.s[run..self.pos]);
            if self.at("<![CDATA[") {
                let at = self.pos;
                self.pos += "<![CDATA[".len();
                let start = self.pos;
                let end = self.past("]]>", "an unclosed CDATA section", at)?;
                // Line ends normalize to \n here too; the body was found
                // by `past`, this is its one more pass.
                let mut run = start;
                let mut i = start;
                while i < end {
                    self.steps += 1;
                    if b[i] == b'\r' {
                        self.push(&mut acc, &self.s[run..i]);
                        self.push_owned(&mut acc, "\n");
                        i += if b.get(i + 1) == Some(&b'\n') { 2 } else { 1 };
                        run = i;
                    } else {
                        i += 1;
                    }
                }
                self.push(&mut acc, &self.s[run..end]);
            } else if self.at("<!--") {
                self.comment()?;
            } else if self.at("<?") {
                self.pi()?;
            } else {
                break;
            }
        }
        Ok(match acc {
            Acc::Empty => None,
            Acc::Borrowed(s) => Some(Cow::Borrowed(s)),
            Acc::Owned(s) => Some(Cow::Owned(s)),
        })
    }

    /// Appends an unchanged slice of the input.
    fn push(&mut self, acc: &mut Acc<'a>, piece: &'a str) {
        if piece.is_empty() {
            return;
        }
        match acc {
            Acc::Empty => *acc = Acc::Borrowed(piece),
            _ => self.push_owned(acc, piece),
        }
    }

    /// Appends text that is not (or not only) a slice of the input.
    fn push_owned(&mut self, acc: &mut Acc<'a>, piece: &str) {
        self.steps += piece.len() as u64;
        match acc {
            Acc::Empty => *acc = Acc::Owned(piece.to_string()),
            Acc::Borrowed(prev) => {
                self.steps += prev.len() as u64;
                let mut s = String::with_capacity(prev.len() + piece.len());
                s.push_str(prev);
                s.push_str(piece);
                *acc = Acc::Owned(s);
            }
            Acc::Owned(s) => s.push_str(piece),
        }
    }

    /// The reference at byte offset `at` (an `&`); returns what it stands
    /// for and the offset past its `;`.
    fn reference(&mut self, at: usize) -> Res<(Ref, usize)> {
        let b = self.b();
        let mut i = at + 1;
        if b.get(i) == Some(&b'#') {
            i += 1;
            let hex = b.get(i) == Some(&b'x');
            if hex {
                i += 1;
            }
            let radix = if hex { 16 } else { 10 };
            let d0 = i;
            // Past U+10FFFF the exact value no longer matters: it is
            // refused either way, so it saturates instead of overflowing.
            let mut v: u32 = 0;
            while let Some(d) = b.get(i).and_then(|&c| (c as char).to_digit(radix)) {
                v = v.saturating_mul(radix).saturating_add(d).min(0x11_0000);
                i += 1;
            }
            self.steps += (i - at) as u64;
            if i == d0 || b.get(i) != Some(&b';') {
                return self.err("a malformed character reference", at);
            }
            return match char::from_u32(v).filter(|&c| is_xml_char(c)) {
                Some(c) => Ok((Ref::Char(c), i + 1)),
                None => self.err("a reference to a character XML does not allow", at),
            };
        }
        let save = self.pos;
        self.pos = i;
        let name = self.name();
        let end = self.pos;
        self.pos = save;
        let name = name?;
        if b.get(end) != Some(&b';') {
            return self.err("a malformed entity reference", at);
        }
        let predefined = match name {
            "lt" => Some("<"),
            "gt" => Some(">"),
            "amp" => Some("&"),
            "apos" => Some("'"),
            "quot" => Some("\""),
            _ => None,
        };
        if let Some(s) = predefined {
            return Ok((Ref::Str(s), end + 1));
        }
        self.steps += name.len() as u64;
        if self.declared.contains(name) {
            return self.kind(ErrorKind::EntityDeclarations, at);
        }
        if self.opts.html_entities {
            self.steps += ENTITY_LOOKUP_STEPS;
            if let Some(s) = entity(name) {
                return Ok((Ref::Str(s), end + 1));
            }
        }
        if self.unseen {
            return self.kind(ErrorKind::EntityDeclarations, at);
        }
        self.err("an undefined entity", at)
    }

    /// A start tag at `<`.
    fn start_tag(&mut self) -> Res<Event<'a>> {
        let at = self.pos;
        self.pos += 1;
        let raw_name = self.name()?;
        let mut raw = std::mem::take(&mut self.raw);
        raw.clear();
        let r = self.start_tag_rest(at, raw_name, &mut raw);
        self.raw = raw;
        r
    }

    fn start_tag_rest(
        &mut self,
        at: usize,
        raw_name: &'a str,
        raw: &mut Vec<RawAttr<'a>>,
    ) -> Res<Event<'a>> {
        let empty;
        loop {
            let sp = self.space();
            match self.peek() {
                Some(b'>') => {
                    self.pos += 1;
                    empty = false;
                    break;
                }
                Some(b'/') => {
                    self.expect("/>", "a malformed start tag")?;
                    empty = true;
                    break;
                }
                None => return self.err("an unclosed start tag", at),
                Some(_) => {
                    if !sp {
                        return self.err("no space before an attribute", self.pos);
                    }
                    raw.push(self.attribute()?);
                }
            }
        }
        if self.stack.len() as u64 >= u64::from(self.opts.max_depth) {
            return self.kind(ErrorKind::TooDeep, at);
        }
        // Namespace declarations first: they apply to the element's own
        // name and attributes. An error ends the reader for good (`next`
        // returns it again), so no error path needs to undo a binding.
        let mut bindings = 0;
        let mut plain = 0;
        for a in raw.iter() {
            let prefix = if a.name == "xmlns" {
                ""
            } else if let Some(p) = a.name.strip_prefix("xmlns:") {
                if p.is_empty() {
                    return self.err("a namespace prefix that is not a name", a.at);
                }
                p
            } else {
                plain += 1;
                continue;
            };
            self.bind(prefix, a)?;
            bindings += 1;
        }
        let (name, _) = self.resolve(raw_name, true, at)?;
        let mut attrs = Vec::with_capacity(plain);
        let mut keys = std::mem::take(&mut self.keys);
        keys.clear();
        for a in raw.iter() {
            if let Some(p) = a.name.strip_prefix("xmlns")
                && (p.is_empty() || p.starts_with(':'))
            {
                keys.push((Key::Decl(p.strip_prefix(':').unwrap_or("")), a.name_at));
                continue;
            }
            let (q, id) = self.resolve(a.name, false, a.at)?;
            let v = self.attr_value(a)?;
            keys.push((Key::Attr(id, q.local), a.name_at));
            attrs.push((q, v));
        }
        let unique = self.unique(&keys);
        self.keys = keys;
        unique?;
        self.stack.push(Open {
            raw: raw_name,
            name,
            bindings,
        });
        self.close_empty = empty;
        Ok(Event::Start(Element { name, attrs }))
    }

    /// `name = "value"` at the current position.
    fn attribute(&mut self) -> Res<RawAttr<'a>> {
        let name_at = self.pos;
        let name = self.name()?;
        self.space();
        self.expect("=", "an attribute without a value")?;
        self.space();
        let q = match self.peek() {
            Some(q @ (b'"' | b'\'')) => q,
            _ => return self.err("an unquoted attribute value", self.pos),
        };
        let b = self.b();
        let open = self.pos;
        self.pos += 1;
        let start = self.pos;
        let mut special = false;
        loop {
            match b.get(self.pos) {
                None => return self.err("an unclosed attribute value", open),
                Some(&c) if c == q => break,
                Some(b'<') => return self.err("< in an attribute value", self.pos),
                Some(b'&' | b'\t' | b'\n' | b'\r') => special = true,
                Some(_) => {}
            }
            self.pos += 1;
            self.steps += 1;
        }
        let value = &self.s[start..self.pos];
        self.pos += 1;
        Ok(RawAttr {
            name,
            name_at,
            value,
            at: start,
            special,
        })
    }

    /// Binds `prefix` (`""` = default) to the declaration's value, after
    /// the Namespaces in XML 1.0 constraints.
    fn bind(&mut self, prefix: &'a str, a: &RawAttr<'a>) -> Res<()> {
        let uri = a.value;
        if a.special {
            // The URI must borrow from the input; a reference or a line
            // break (which normalization would change) is refused rather
            // than copied. No real namespace name holds either.
            return self.err("a reference or line break in a namespace name", a.at);
        }
        if !prefix.is_empty() {
            let ncname = !prefix.contains(':') && prefix.chars().next().is_some_and(is_name_start);
            if !ncname {
                return self.err("a namespace prefix that is not a name", a.at);
            }
            if prefix == "xmlns" {
                return self.err("the xmlns prefix declared", a.at);
            }
            if uri.is_empty() {
                return self.err("a prefix bound to the empty namespace name", a.at);
            }
            if (prefix == "xml") != (uri == XML_NS) {
                return self.err("the xml prefix and its namespace bound apart", a.at);
            }
        } else if uri == XML_NS {
            return self.err("the xml namespace as the default", a.at);
        }
        if uri == XMLNS_NS {
            return self.err("the xmlns namespace declared", a.at);
        }
        self.steps += (prefix.len() + 1) as u64;
        // One hash of the name and one comparison with an equal one.
        self.steps += 2 * uri.len() as u64 + 1;
        let next = self.next_id;
        let u = self.uris.entry(uri).or_insert(Uri {
            id: next,
            bindings: 0,
        });
        if u.id == next {
            self.next_id += 1;
        }
        u.bindings += 1;
        let id = u.id;
        self.ns.entry(prefix).or_default().push((uri, id));
        self.undo.push(prefix);
        Ok(())
    }

    /// Removes the last `n` bindings.
    fn unbind(&mut self, n: usize) {
        for _ in 0..n {
            let Some(p) = self.undo.pop() else { return };
            self.steps += (p.len() + 1) as u64;
            let Some((uri, _)) = self.ns.get_mut(p).and_then(Vec::pop) else {
                continue;
            };
            self.steps += 2 * uri.len() as u64 + 1;
            if let Some(u) = self.uris.get_mut(uri) {
                u.bindings -= 1;
                if u.bindings == 0 {
                    self.uris.remove(uri);
                }
            }
        }
    }

    /// A raw `prefix:local` or `local` resolved against the bindings in
    /// scope, with the namespace name's id. Unprefixed attributes have no
    /// namespace.
    fn resolve(
        &mut self,
        raw: &'a str,
        element: bool,
        at: usize,
    ) -> Res<(QName<'a>, Option<usize>)> {
        self.steps += (raw.len() + 1) as u64;
        let (prefix, local) = match raw.split_once(':') {
            None => ("", raw),
            Some((p, l)) => {
                let ok = !p.is_empty()
                    && !l.contains(':')
                    && l.chars()
                        .next()
                        .is_some_and(|c| c != ':' && is_name_start(c));
                if !ok {
                    return self.err("a name that is not a qualified name", at);
                }
                (p, l)
            }
        };
        if prefix.is_empty() {
            let ns = if element {
                self.ns
                    .get("")
                    .and_then(|v| v.last())
                    .copied()
                    .filter(|(u, _)| !u.is_empty())
            } else {
                None
            };
            return Ok((
                QName {
                    ns: ns.map(|(u, _)| u),
                    local,
                },
                ns.map(|(_, id)| id),
            ));
        }
        if prefix == "xml" {
            return Ok((
                QName {
                    ns: Some(XML_NS),
                    local,
                },
                Some(XML_ID),
            ));
        }
        if prefix == "xmlns" {
            return self.err("an element with the xmlns prefix", at);
        }
        match self.ns.get(prefix).and_then(|v| v.last()) {
            Some(&(uri, id)) => Ok((
                QName {
                    ns: Some(uri),
                    local,
                },
                Some(id),
            )),
            None => self.err("an unbound namespace prefix", at),
        }
    }

    /// An attribute value, normalized: references replaced, each
    /// whitespace character (and `\r\n`) one space, as XML 1.0 3.3.3 does
    /// for CDATA attributes. Character references stay as written.
    fn attr_value(&mut self, a: &RawAttr<'a>) -> Res<Cow<'a, str>> {
        if !a.special {
            return Ok(Cow::Borrowed(a.value));
        }
        let b = self.b();
        let end = a.at + a.value.len();
        let mut out = String::with_capacity(a.value.len());
        let mut i = a.at;
        let mut run = i;
        while i < end {
            match b[i] {
                b'&' => {
                    out.push_str(&self.s[run..i]);
                    let (r, next) = self.reference(i)?;
                    match r {
                        Ref::Char(c) => out.push(c),
                        Ref::Str(s) => {
                            self.steps += s.len() as u64;
                            out.extend(s.chars().map(|c| {
                                if matches!(c, '\t' | '\n' | '\r') {
                                    ' '
                                } else {
                                    c
                                }
                            }))
                        }
                    }
                    i = next;
                    run = i;
                }
                b'\t' | b'\n' | b'\r' => {
                    out.push_str(&self.s[run..i]);
                    out.push(' ');
                    i += if b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') {
                        2
                    } else {
                        1
                    };
                    self.steps += 1;
                    run = i;
                }
                _ => {
                    i += 1;
                    self.steps += 1;
                }
            }
        }
        out.push_str(&self.s[run..end]);
        Ok(Cow::Owned(out))
    }

    /// No two attributes of one element share an expanded name, and no
    /// prefix is declared twice. A duplicate is reported at the later of
    /// the two. Every comparison and hash counts the bytes it may walk.
    fn unique(&mut self, keys: &[(Key<'a>, usize)]) -> Res<()> {
        let dup = if keys.len() <= PAIRWISE_ATTRS {
            let mut dup = None;
            for (j, (k, at)) in keys.iter().enumerate() {
                for (l, _) in &keys[..j] {
                    self.steps += k.cost().min(l.cost());
                    if k == l {
                        dup = dup.or(Some(*at));
                    }
                }
            }
            dup
        } else {
            let mut seen = HashSet::with_capacity(keys.len());
            let mut dup = None;
            for (k, at) in keys {
                self.steps += k.cost();
                if !seen.insert(k) {
                    dup = Some(*at);
                    break;
                }
            }
            dup
        };
        match dup {
            Some(at) => self.err("a duplicate attribute", at),
            None => Ok(()),
        }
    }

    /// An end tag at `</`; it must close the innermost open element.
    fn end_tag(&mut self) -> Res<Event<'a>> {
        let at = self.pos;
        self.pos += 2;
        let name = self.name()?;
        self.space();
        self.expect(">", "a malformed end tag")?;
        self.steps += name.len() as u64;
        match self.stack.last() {
            Some(o) if o.raw == name => Ok(self.pop()),
            _ => self.err("an end tag that does not match its start tag", at),
        }
    }

    /// Closes the innermost element.
    fn pop(&mut self) -> Event<'a> {
        let o = self
            .stack
            .pop()
            .expect("pop is called only with an open element");
        self.unbind(o.bindings);
        if self.stack.is_empty() {
            self.phase = Phase::Epilog;
        }
        Event::End(o.name)
    }
}
