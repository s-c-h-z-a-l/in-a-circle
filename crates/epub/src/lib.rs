//! EPUB 3 packer: a [`Document`] as a reflowable book, through the stored
//! ZIP writer.
//!
//! Layout: `mimetype` first and stored (no extra field), then
//! `META-INF/container.xml`, `OEBPS/content.opf`, `OEBPS/nav.xhtml` (the
//! EPUB 3 table of contents; no NCX), `OEBPS/style.css` and one XHTML file
//! per chapter, `OEBPS/ch0001.xhtml` onwards. The chapters are the split of
//! [`doc::book::split`]; the table of contents lists them and, one level
//! below, their top-level headings of the next level.
//!
//! Memory: the package document and the table of contents are built first
//! from the chapters' titles and heading ids (bounded by the headings'
//! text); then one chapter's XHTML at a time is written and streamed into
//! the archive. Nothing depends on the clock or chance: the identifier is
//! a hash of [`Meta::id_seed`], the modification time is the caller's, so
//! the same input gives the same bytes.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read};
use std::{fmt, ops::Range};

use doc::book::{self, MAX_CHAPTERS};
use doc::xhtml::{self, Anchors, Chapter, Mode};
use doc::{Block, Document, LimitError, Limits, Loss};
use zip::{StoreWriter, WriteError};

/// What the book says about itself.
#[derive(Debug, Clone)]
pub struct Meta {
    /// `dc:title`; the caller's choice (the first H1 or the file stem).
    pub title: String,
    /// A BCP 47 language tag (`hu`, `pt-BR`); anything that is not a
    /// well-formed tag is written as `und`.
    pub lang: String,
    /// The input's modification time, seconds since the Unix epoch:
    /// `dcterms:modified`, in UTC.
    pub modified_unix: i64,
    /// The bytes the identifier is hashed from with [`fnv1a_128`]. A
    /// caller holding a large input passes its digest (the 16 bytes of
    /// `fnv1a_128(..).to_be_bytes()`) rather than a copy of it.
    pub id_seed: Vec<u8>,
}

#[derive(Debug)]
pub enum BookError {
    /// The split gives more than [`MAX_CHAPTERS`]; nothing was written.
    TooManyChapters,
    Zip(WriteError),
    /// The output budget ran out (or memory for the next piece).
    Limit(LimitError),
    Io(io::Error),
}

impl fmt::Display for BookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyChapters => {
                write!(f, "book would have more than {MAX_CHAPTERS} chapters")
            }
            Self::Zip(e) => e.fmt(f),
            Self::Limit(e) => e.fmt(f),
            Self::Io(e) => write!(f, "write failed: {e}"),
        }
    }
}

impl std::error::Error for BookError {}

impl From<WriteError> for BookError {
    fn from(e: WriteError) -> Self {
        Self::Zip(e)
    }
}

impl From<LimitError> for BookError {
    fn from(e: LimitError) -> Self {
        Self::Limit(e)
    }
}

/// FNV-1a offset basis, 128 bits (the FNV reference).
const FNV_OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
/// FNV prime, 128 bits: 2^88 + 2^8 + 0x3b (the FNV reference).
const FNV_PRIME: u128 = 0x0000000001000000000000000000013b;

/// 128-bit FNV-1a of the concatenation of `chunks`. Not a security hash:
/// it only makes the book's identifier deterministic and, for different
/// inputs, different. Chunks are not separated, so a caller hashing
/// several inputs puts each input's length (as 8 bytes) before it, so
/// that `ab` + `c` and `a` + `bc` differ.
pub fn fnv1a_128(chunks: &[&[u8]]) -> u128 {
    let mut h = FNV_OFFSET;
    for &b in chunks.iter().flat_map(|c| c.iter()) {
        h ^= u128::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// `h` as a version 8 UUID (RFC 9562, custom): the version nibble set to
/// 8 and the variant bits to `10`, the remaining 122 bits from `h`;
/// lowercase `8-4-4-4-12` hex.
pub fn uuid_v8(h: u128) -> String {
    let mut b = h.to_be_bytes();
    b[6] = (b[6] & 0x0f) | 0x80;
    b[8] = (b[8] & 0x3f) | 0x80;
    let mut s = String::with_capacity(36);
    for (i, x) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// Earliest and latest times the `YYYY-MM-DDThh:mm:ssZ` form can write
/// (four-digit years 0001 to 9999); an mtime outside is clamped to them.
const MIN_TIME: i64 = -62_135_596_800;
const MAX_TIME: i64 = 253_402_300_799;
const DAY: i64 = 86_400;

/// `t` (Unix seconds) in UTC as `YYYY-MM-DDThh:mm:ssZ`.
fn utc(t: i64) -> String {
    let t = t.clamp(MIN_TIME, MAX_TIME);
    let (days, secs) = (t.div_euclid(DAY), t.rem_euclid(DAY));
    // Civil date from days since 1970-01-01 (H. Hinnant's
    // `civil_from_days`): 719 468 days from 0000-03-01 to the epoch, eras
    // of 400 years (146 097 days), years starting on 1 March.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// Longest language tag taken: RFC 5646 (4.4.1) asks implementations to
/// handle at least 35 characters, which covers every registered form; a
/// longer one is `und`.
const MAX_LANG: usize = 35;

/// `lang` if it is a well-formed tag of this subset of the RFC 5646
/// grammar, subtags joined by `-`: a language of 2-3 ASCII letters, then
/// optionally a script (4 letters), then optionally a region (2 letters or
/// 3 digits), then any variants (5-8 letters or digits, or a digit and 3
/// letters or digits). Anything else, extensions and private use
/// included, is `und` (undetermined). Fail closed: the value goes into
/// attributes and readers pick fonts and hyphenation by it.
fn lang_tag(lang: &str) -> &str {
    let alpha = |p: &str| p.bytes().all(|b| b.is_ascii_alphabetic());
    let digit = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    let alnum = |p: &str| p.bytes().all(|b| b.is_ascii_alphanumeric());
    let script = |p: &str| p.len() == 4 && alpha(p);
    let region = |p: &str| (p.len() == 2 && alpha(p)) || (p.len() == 3 && digit(p));
    let variant = |p: &str| {
        ((5..=8).contains(&p.len()) && alnum(p))
            || (p.len() == 4 && p.as_bytes()[0].is_ascii_digit() && alnum(p))
    };
    let mut parts = lang.split('-').peekable();
    let primary_ok = parts
        .next()
        .is_some_and(|p| (2..=3).contains(&p.len()) && alpha(p));
    parts.next_if(|p| script(p));
    parts.next_if(|p| region(p));
    let rest_ok = parts.all(variant);
    if lang.len() <= MAX_LANG && primary_ok && rest_ok {
        lang
    } else {
        "und"
    }
}

/// The title when the caller's is empty (or only characters XML forbids):
/// `dc:title` and every `<title>` must have text.
const UNTITLED: &str = "Untitled";

/// `s` trimmed, without the characters XML forbids (each removal noted as
/// [`Loss::ControlCharacters`]).
fn clean(s: &str, losses: &mut BTreeSet<Loss>) -> String {
    let out: String = s.chars().filter(|&c| xhtml::xml_char(c)).collect();
    if out.len() != s.len() {
        losses.insert(Loss::ControlCharacters);
    }
    out.trim().to_string()
}

/// `s` escaped for XML text and double-quoted attributes. `s` is
/// [`clean`] already.
fn esc(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            _ => out.push(c),
        }
    }
}

/// A heading id in a URL fragment, as the chapter writer writes links:
/// ASCII letters, digits, `-` and `_` as they are, every other byte
/// percent-encoded (a URL is ASCII; readers decode it back to the id).
fn fragment(out: &mut String, id: &str) {
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

const MIMETYPE: &str = "application/epub+zip";

const CONTAINER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">
<rootfiles>
<rootfile full-path=\"OEBPS/content.opf\" media-type=\"application/oebps-package+xml\"/>
</rootfiles>
</container>
";

/// Start of every XHTML file of the book, up to the language.
const XHTML_START: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html>\n";

const CHAPTER_END: &str = "</body>\n</html>\n";

/// The file name of chapter `i` (from 0) inside `OEBPS/`: four digits, as
/// [`MAX_CHAPTERS`] (1024) needs.
fn chapter_file(i: usize) -> String {
    format!("ch{:04}.xhtml", i + 1)
}

/// One chapter's entry in the table of contents.
struct Toc {
    title: String,
    /// (id, text) of the top-level headings one level below the chapter's.
    below: Vec<(String, String)>,
}

/// The heading level the chapters start at: the split's, or for a book of
/// one chapter the first top-level heading's (none without headings).
fn chapter_level(doc: &Document, ranges: &[Range<usize>]) -> Option<u8> {
    let heading = |b: &Block| match b {
        Block::Heading { level, .. } => Some(*level),
        _ => None,
    };
    if ranges.len() > 1 {
        // A split: every range but a leading preface starts at its heading.
        ranges
            .last()
            .and_then(|r| doc.blocks.get(r.start))
            .and_then(heading)
    } else {
        doc.blocks.iter().find_map(heading)
    }
}

fn toc(
    chapters: &[Chapter],
    anchors: &Anchors,
    level: Option<u8>,
    book_title: &str,
    losses: &mut BTreeSet<Loss>,
) -> Vec<Toc> {
    let single = chapters.len() == 1;
    chapters
        .iter()
        .enumerate()
        .map(|(i, ch)| {
            let has_heading = ch.blocks.iter().any(|b| matches!(b, Block::Heading { .. }));
            let title = if single && !has_heading {
                book_title.to_string()
            } else {
                clean(&book::chapter_title(ch.blocks, i), losses)
            };
            let title = if title.is_empty() {
                format!("Chapter {}", i + 1)
            } else {
                title
            };
            let mut below = Vec::new();
            if let Some(level) = level {
                for (bi, id) in anchors.top_level_ids(i, ch.blocks) {
                    if let Block::Heading { level: l, content } = &ch.blocks[bi]
                        && *l == level + 1
                    {
                        let mut t = String::new();
                        xhtml::plain(content, &mut t);
                        let t = clean(&t, losses);
                        if !t.is_empty() {
                            below.push((id.to_string(), t));
                        }
                    }
                }
            }
            Toc { title, below }
        })
        .collect()
}

fn nav(toc: &[Toc], title: &str, lang: &str) -> String {
    let mut s = String::from(XHTML_START);
    s.push_str("<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\" xml:lang=\"");
    s.push_str(lang);
    s.push_str("\" lang=\"");
    s.push_str(lang);
    s.push_str("\">\n<head>\n<title>");
    esc(&mut s, title);
    s.push_str("</title>\n</head>\n<body>\n<nav epub:type=\"toc\" id=\"toc\">\n<h1>");
    esc(&mut s, title);
    s.push_str("</h1>\n<ol>\n");
    for (i, t) in toc.iter().enumerate() {
        let file = chapter_file(i);
        s.push_str("<li><a href=\"");
        s.push_str(&file);
        s.push_str("\">");
        esc(&mut s, &t.title);
        s.push_str("</a>");
        if !t.below.is_empty() {
            s.push_str("\n<ol>\n");
            for (id, text) in &t.below {
                s.push_str("<li><a href=\"");
                s.push_str(&file);
                s.push('#');
                fragment(&mut s, id);
                s.push_str("\">");
                esc(&mut s, text);
                s.push_str("</a></li>\n");
            }
            s.push_str("</ol>\n");
        }
        s.push_str("</li>\n");
    }
    s.push_str("</ol>\n</nav>\n</body>\n</html>\n");
    s
}

fn opf(meta: &Meta, title: &str, lang: &str, chapters: usize) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str("<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"uid\" xml:lang=\"");
    s.push_str(lang);
    s.push_str("\">\n<metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n");
    s.push_str("<dc:identifier id=\"uid\">urn:uuid:");
    s.push_str(&uuid_v8(fnv1a_128(&[&meta.id_seed])));
    s.push_str("</dc:identifier>\n<dc:title>");
    esc(&mut s, title);
    s.push_str("</dc:title>\n<dc:language>");
    s.push_str(lang);
    s.push_str("</dc:language>\n<meta property=\"dcterms:modified\">");
    s.push_str(&utc(meta.modified_unix));
    s.push_str("</meta>\n</metadata>\n<manifest>\n");
    s.push_str("<item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n");
    s.push_str("<item id=\"css\" href=\"style.css\" media-type=\"text/css\"/>\n");
    for i in 0..chapters {
        s.push_str(&format!(
            "<item id=\"ch{n:04}\" href=\"ch{n:04}.xhtml\" media-type=\"application/xhtml+xml\"/>\n",
            n = i + 1
        ));
    }
    s.push_str("</manifest>\n<spine>\n");
    for i in 0..chapters {
        s.push_str(&format!("<itemref idref=\"ch{:04}\"/>\n", i + 1));
    }
    s.push_str("</spine>\n</package>\n");
    s
}

/// The chapter file around a body from [`xhtml::write_chapter`]'s
/// fragment mode, up to and including `<body>`.
fn chapter_head(title: &str, lang: &str) -> String {
    let mut s = String::from(XHTML_START);
    s.push_str("<html xmlns=\"http://www.w3.org/1999/xhtml\" xml:lang=\"");
    s.push_str(lang);
    s.push_str("\" lang=\"");
    s.push_str(lang);
    s.push_str("\">\n<head>\n<title>");
    esc(&mut s, title);
    s.push_str("</title>\n<link rel=\"stylesheet\" type=\"text/css\" href=\"style.css\"/>\n</head>\n<body>\n");
    s
}

/// Charges `n` bytes to the output budget.
fn charge(used: &mut usize, n: usize, limits: Limits) -> Result<(), LimitError> {
    *used = used.saturating_add(n);
    if *used > limits.output_bytes {
        return Err(LimitError::Exceeded("output"));
    }
    Ok(())
}

fn add(zip: &mut StoreWriter, name: &str, parts: &[&str]) -> Result<(), WriteError> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    let mut r: Box<dyn Read + '_> = Box::new(io::empty());
    for p in parts {
        r = Box::new(r.chain(p.as_bytes()));
    }
    zip.add_exact(name, &mut r, len as u64)?;
    Ok(())
}

/// Writes `doc` as an EPUB 3 book to `out`, which should be an empty file
/// opened for writing. Returns what the conversion lost. The whole book's
/// XHTML (chapters, table of contents, package document) is charged to
/// `limits.output_bytes`.
///
/// More than [`MAX_CHAPTERS`] chapters is `TooManyChapters`, before
/// anything is written. On any other error the file is cut back to empty
/// (best effort; removing it stays the caller's job), so no half book is
/// left that a reader might open.
pub fn write(
    doc: &Document,
    meta: &Meta,
    out: File,
    limits: Limits,
) -> Result<BTreeSet<Loss>, BookError> {
    let ranges = book::split(doc);
    if ranges.len() > MAX_CHAPTERS {
        return Err(BookError::TooManyChapters);
    }
    let guard = out.try_clone().map_err(BookError::Io)?;
    let r = pack(doc, meta, out, limits, &ranges);
    if r.is_err() {
        let _ = guard.set_len(0);
    }
    r
}

fn pack(
    doc: &Document,
    meta: &Meta,
    out: File,
    limits: Limits,
    ranges: &[Range<usize>],
) -> Result<BTreeSet<Loss>, BookError> {
    let mut losses = BTreeSet::new();
    let lang = lang_tag(&meta.lang);
    let title = clean(&meta.title, &mut losses);
    let title = if title.is_empty() { UNTITLED } else { &title };

    let chapters: Vec<Chapter> = ranges
        .iter()
        .enumerate()
        .map(|(i, r)| Chapter {
            blocks: &doc.blocks[r.clone()],
            file: chapter_file(i),
        })
        .collect();
    let anchors = Anchors::build(&chapters);
    let toc = toc(
        &chapters,
        &anchors,
        chapter_level(doc, ranges),
        title,
        &mut losses,
    );

    let mut used = 0;
    let nav = nav(&toc, title, lang);
    charge(&mut used, nav.len(), limits)?;
    let opf = opf(meta, title, lang, chapters.len());
    charge(&mut used, opf.len(), limits)?;

    let mut zip = StoreWriter::new(out);
    add(&mut zip, "mimetype", &[MIMETYPE])?;
    add(&mut zip, "META-INF/container.xml", &[CONTAINER])?;
    add(&mut zip, "OEBPS/content.opf", &[&opf])?;
    add(&mut zip, "OEBPS/nav.xhtml", &[&nav])?;
    add(&mut zip, "OEBPS/style.css", &[xhtml::STYLE_EPUB])?;
    drop((nav, opf));

    for (ch, t) in chapters.iter().zip(&toc) {
        let head = chapter_head(&t.title, lang);
        charge(&mut used, head.len() + CHAPTER_END.len(), limits)?;
        let budget = Limits {
            output_bytes: limits.output_bytes - used,
            ..limits
        };
        let (body, lost) = xhtml::write_chapter(ch, &anchors, &Mode::Fragment, budget)?;
        charge(&mut used, body.len(), limits)?;
        losses.extend(lost);
        add(
            &mut zip,
            &format!("OEBPS/{}", ch.file),
            &[&head, &body, CHAPTER_END],
        )?;
    }
    zip.finish()?;
    Ok(losses)
}
