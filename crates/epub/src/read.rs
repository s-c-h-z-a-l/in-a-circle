//! EPUB reader: the book's skeleton (the OCF container, the package
//! document, its metadata, manifest and spine) and each spine item's raw
//! bytes, one at a time, from a book on disk that is foreign input.
//!
//! `open` reads the ZIP directory (`zip::open`, with the limits below),
//! `META-INF/container.xml`, the first package rootfile it names and the
//! package's `dc:title`, `dc:creator`, manifest and spine (EPUB 2 and 3:
//! the spine is the same in both; the `mimetype` entry is not required,
//! many real books do not store it first). Every spine item is resolved
//! to its ZIP entry then; [`Book::chapter`] reads one when asked and keeps
//! nothing. A broken skeleton fails the whole book ([`BookError`]); a
//! spine item that cannot be read fails only itself ([`ChapterError`]),
//! and the caller reports it in its place.
//!
//! Paths: an `href` is relative to the package document's folder, its
//! `#fragment` cut, `%XX` decoded and normalized lexically; entry names
//! are matched exactly (case-sensitive), as the ZIP holds them. An href
//! with a URL scheme, an absolute one, or one that climbs above the
//! container root is outside the book and never looked up.
//!
//! DRM: `META-INF/encryption.xml` lists the encrypted resources
//! (`EncryptedData`, its `EncryptionMethod` and `CipherReference URI`,
//! relative to the container root). Font obfuscation (the IDPF and Adobe
//! algorithms) is not encryption and is ignored; any other method, or none
//! stated, is. A ZIP entry with an encryption flag is encrypted too. When
//! every XHTML spine item that is in the book is encrypted, the book is
//! DRM-protected; when only some are, those items fail alone. `rights.xml`
//! decides nothing.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io;

use doc::xhtml::{normalize, percent_decode};
use xml::{Event, Options, Reader};

/// Most ZIP directory records: 12,000, the CBZ reader's value, so the two
/// archive readers accept the same archives. A long book is a few thousand
/// entries (chapters, images, fonts); about 200 B per record, 2.4 MB at the
/// limit. Past it: `damaged EPUB: too many entries`.
const ZIP_ENTRIES: usize = 12_000;

/// The ZIP reader's per-entry size limit: the largest size a non-ZIP64
/// header can declare (u32::MAX), as for the CBZ reader. The real limits
/// (`Limits::chapter_bytes`, `Limits::package_bytes`) are checked here per
/// entry before it is read, so a large font or image the reader never
/// opens does not fail the book.
const ZIP_ENTRY_BYTES: u64 = u32::MAX as u64;

/// Largest `size / compressed_size` of an entry: 1032, the most a deflate
/// stream can reach (a 258-byte match costs at least two bits); the CBZ
/// reader's value. A header claiming more is lying or damaged, and the
/// ZIP reader refuses the archive before inflating anything. Past it:
/// `damaged EPUB: an entry's compression ratio is too high`.
const ZIP_RATIO: u64 = 1032;

/// Longest entry name: 512 bytes, the CBZ reader's value (twice the
/// 255-byte file name limit, room for `OEBPS/Text/<name>` paths); at most
/// 6 MB for 12,000 names. Past it: `damaged EPUB: an entry name is too
/// long`.
const ZIP_NAME_BYTES: usize = 512;

/// The ZIP reader's limits for a book: the CBZ reader's values.
const ZIP_LIMITS: zip::Limits = zip::Limits {
    entries: ZIP_ENTRIES,
    entry_bytes: ZIP_ENTRY_BYTES,
    ratio: ZIP_RATIO,
    name_bytes: ZIP_NAME_BYTES,
};

/// Most spine items: 10,000, the CBZ reader's page limit (below the
/// 12,000 entries, so the message is reachable). The longest real books
/// split into a few hundred files; one item costs about 500 B here (the
/// resolved path, its media type, the slot). Past it:
/// `book has more than 10000 chapters`.
pub const MAX_SPINE: usize = 10_000;

/// The package media type `container.xml` must name.
const PACKAGE_TYPE: &str = "application/oebps-package+xml";
/// The one spine item type that is read as a chapter.
const XHTML_TYPE: &str = "application/xhtml+xml";
/// A spine item of this type is a picture page: it reads as nothing.
const SVG_TYPE: &str = "image/svg+xml";
/// Dublin Core elements, the namespace of `dc:title` and `dc:creator`.
const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
const CONTAINER: &str = "META-INF/container.xml";
const ENCRYPTION: &str = "META-INF/encryption.xml";
/// Font obfuscation algorithms (OCF's IDPF obfuscation and Adobe's): a
/// font scrambled with the book's identifier, not a cipher; a resource
/// they name is read as it is.
const OBFUSCATION: [&str; 2] = [
    "http://www.idpf.org/2008/embedding",
    "http://ns.adobe.com/pdf/enc#RC",
];

/// The reader's own budgets. [`Limits::DEFAULT`] is what [`open`] uses;
/// smaller ones are for the fuzz target, which reaches them at speed.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest chapter, uncompressed, checked on the declared size before
    /// reading and on the bytes produced while reading.
    pub chapter_bytes: u64,
    /// Largest sum of the chapters read, each time it is in the spine.
    pub book_bytes: u64,
    /// Largest `container.xml`, package document or `encryption.xml`,
    /// uncompressed.
    pub package_bytes: u64,
    /// Most XML events (elements and text runs) one package file may have.
    pub package_nodes: usize,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        // 8 MiB: the text family's input limit (`doc::Limits::DEFAULT`),
        // so a chapter is held to what a text file is. Past it: that
        // chapter is `too large`, the rest of the book still reads.
        chapter_bytes: doc::Limits::DEFAULT.input_bytes as u64,
        // 64 MiB: the book's text output is at most 32 MiB (the text
        // family's output budget), and XHTML markup is at least about as
        // large as the text it holds, so twice the output covers the
        // chapters a book within that budget needs; it also bounds the
        // work for one book at 64 MiB of XML. Checked at `open` on the
        // declared sizes (the ZIP reader never produces more than it
        // declares). Past it: `book is too large`, nothing is read.
        book_bytes: 64 << 20,
        // 8 MiB: a chapter's limit; a real package document is tens of
        // KiB, a container a few hundred bytes. Past it: `not an EPUB
        // (package document too large)`.
        package_bytes: doc::Limits::DEFAULT.input_bytes as u64,
        // The text family's node budget (`doc::Limits::DEFAULT`, 2^19),
        // the most structure one chapter may build; the manifest of the
        // largest accepted book is a few tens of thousands of elements.
        // Past it: `not an EPUB (package document too large)`.
        package_nodes: doc::Limits::DEFAULT.nodes,
    };
}

/// Why a whole book cannot be read: nothing is written for it.
#[derive(Debug)]
pub enum BookError {
    NoContainer,
    NoPackage,
    PackageTooLarge,
    /// `container.xml` or the package document is not well-formed (or
    /// uses what the XML reader refuses); `path` is its entry name.
    PackageXml {
        path: String,
        error: xml::Error,
    },
    /// No spine, or an empty one.
    NoSpine,
    TooManyChapters,
    DrmProtected,
    TooLarge,
    /// The archive itself: the ZIP reader's reason, or a skeleton file it
    /// could not read.
    Damaged(String),
}

impl fmt::Display for BookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoContainer => f.write_str("not an EPUB (no META-INF/container.xml)"),
            Self::NoPackage => f.write_str("not an EPUB (no package document)"),
            Self::PackageTooLarge => f.write_str("not an EPUB (package document too large)"),
            Self::PackageXml { path, error } => write!(f, "not an EPUB ({error} in {path})"),
            Self::NoSpine => f.write_str("book has no spine"),
            Self::TooManyChapters => write!(f, "book has more than {MAX_SPINE} chapters"),
            Self::DrmProtected => f.write_str("book is DRM-protected"),
            Self::TooLarge => f.write_str("book is too large"),
            Self::Damaged(why) => write!(f, "damaged EPUB: {why}"),
        }
    }
}

impl std::error::Error for BookError {}

/// Why one spine item cannot be read; the rest of the book still can.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChapterError {
    TooLarge,
    Encrypted,
    OutsideTheBook,
    Missing,
    /// Neither XHTML nor SVG: the item's media type.
    UnsupportedType(String),
    /// The ZIP reader's reason (CRC, method, deflate stream).
    Damaged(String),
}

impl ChapterError {
    /// The reason as the reader's report names it:
    /// `[Chapter N could not be read: <reason>]`.
    pub fn reason(&self) -> String {
        match self {
            Self::TooLarge => "too large".into(),
            Self::Encrypted => "encrypted".into(),
            Self::OutsideTheBook => "outside the book".into(),
            Self::Missing => "missing".into(),
            Self::UnsupportedType(t) => format!("unsupported type {t}"),
            Self::Damaged(why) => format!("damaged ({why})"),
        }
    }
}

/// One spine item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterRef {
    /// Its spine position, from 1: the `N` of `Chapter N`.
    pub index: usize,
    /// The entry it resolves to (or, if it does not, its `href` as
    /// written; empty when the manifest has no such item).
    pub path: String,
    /// The manifest's `media-type` (empty when there is no such item).
    pub media_type: String,
    /// False for `linear="no"`; such an item is still read in its place.
    pub linear: bool,
}

/// How a spine item was resolved at `open`.
#[derive(Debug)]
enum Slot {
    /// An XHTML chapter in this entry, within the chapter limit.
    Chapter(usize),
    /// A picture page (SVG): nothing to read.
    Picture,
    Fail(ChapterError),
}

/// An opened book: its metadata and spine, and the archive the chapters
/// are read from. Holds no chapter.
pub struct Book<'f> {
    /// The first non-empty `dc:title`, whitespace collapsed.
    pub title: Option<String>,
    /// The non-empty `dc:creator`s in document order.
    pub authors: Vec<String>,
    /// Every spine item in spine order, `linear="no"` included.
    pub chapters: Vec<ChapterRef>,
    archive: zip::Archive<'f>,
    slots: Vec<Slot>,
    limits: Limits,
}

/// Open the book in `file` with [`Limits::DEFAULT`].
pub fn open(file: &File) -> Result<Book<'_>, BookError> {
    open_with(file, Limits::DEFAULT)
}

/// Open the book in `file` with these limits.
pub fn open_with(file: &File, limits: Limits) -> Result<Book<'_>, BookError> {
    let archive = zip::open(file, ZIP_LIMITS).map_err(|e| BookError::Damaged(e.to_string()))?;
    // Name -> entry index, borrowing the names. The ZIP reader has refused
    // duplicate names, so each name is one entry.
    let names: HashMap<&[u8], usize> = archive
        .entries()
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.is_dir())
        .map(|(i, e)| (e.name.as_slice(), i))
        .collect();

    let at = *names
        .get(CONTAINER.as_bytes())
        .ok_or(BookError::NoContainer)?;
    let bytes = read_package(&archive, at, &limits)?;
    let rootfile = parse_container(&bytes, &limits)?.ok_or(BookError::NoPackage)?;
    let opf_path = normalize(&percent_decode(&rootfile));
    if escapes(&opf_path) {
        return Err(BookError::NoPackage);
    }
    let at = *names.get(opf_path.as_bytes()).ok_or(BookError::NoPackage)?;
    let bytes = read_package(&archive, at, &limits)?;
    let pkg = parse_package(&bytes, &opf_path, &limits)?;
    drop(bytes);
    let encrypted = match names.get(ENCRYPTION.as_bytes()) {
        None => HashSet::new(),
        Some(&at) => parse_encryption(&read_package(&archive, at, &limits)?, &limits)?,
    };

    let base = opf_path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let mut chapters = Vec::with_capacity(pkg.spine.len());
    let mut slots = Vec::with_capacity(pkg.spine.len());
    let mut total: u64 = 0;
    // XHTML items in the book: encrypted ones, and ones that are not.
    let (mut locked, mut unlocked) = (0usize, 0usize);
    for (k, (idref, linear)) in pkg.spine.iter().enumerate() {
        let Some((href, media_type)) = pkg.manifest.get(idref.as_str()) else {
            chapters.push(ChapterRef {
                index: k + 1,
                path: String::new(),
                media_type: String::new(),
                linear: *linear,
            });
            slots.push(Slot::Fail(ChapterError::Missing));
            continue;
        };
        let (path, slot) = match resolve(base, href) {
            None => (href.clone(), Slot::Fail(ChapterError::OutsideTheBook)),
            Some(path) => match names.get(path.as_bytes()) {
                None => (path, Slot::Fail(ChapterError::Missing)),
                Some(&i) => {
                    let entry = &archive.entries()[i];
                    let is_locked = entry.encrypted || encrypted.contains(&path);
                    if media_type == XHTML_TYPE {
                        *(if is_locked {
                            &mut locked
                        } else {
                            &mut unlocked
                        }) += 1;
                    }
                    let slot = if is_locked {
                        Slot::Fail(ChapterError::Encrypted)
                    } else {
                        classify(entry, i, media_type, &limits)
                    };
                    (path, slot)
                }
            },
        };
        if let Slot::Chapter(i) = slot {
            total = total.saturating_add(archive.entries()[i].size);
        }
        chapters.push(ChapterRef {
            index: k + 1,
            path,
            media_type: media_type.clone(),
            linear: *linear,
        });
        slots.push(slot);
    }
    // Every XHTML item in the book is encrypted (at least one is).
    if locked > 0 && unlocked == 0 {
        return Err(BookError::DrmProtected);
    }
    if total > limits.book_bytes {
        return Err(BookError::TooLarge);
    }
    Ok(Book {
        title: pkg.title,
        authors: pkg.authors,
        chapters,
        archive,
        slots,
        limits,
    })
}

/// What a resolved spine item in entry `i` is.
fn classify(entry: &zip::Entry, i: usize, media_type: &str, limits: &Limits) -> Slot {
    match media_type {
        SVG_TYPE => Slot::Picture,
        XHTML_TYPE if entry.size > limits.chapter_bytes => Slot::Fail(ChapterError::TooLarge),
        XHTML_TYPE => Slot::Chapter(i),
        other => Slot::Fail(ChapterError::UnsupportedType(other.to_string())),
    }
}

impl Book<'_> {
    /// The raw bytes of spine item `i` (1-based, as [`ChapterRef::index`])
    /// in `out`, which is cleared first and left empty on an error. An SVG
    /// page reads as nothing (`Ok` with `out` empty); an index that is no
    /// spine item is `Missing`. Nothing is kept: the same item can be read
    /// again.
    pub fn chapter(&self, i: usize, out: &mut Vec<u8>) -> Result<(), ChapterError> {
        out.clear();
        let slot = i
            .checked_sub(1)
            .and_then(|k| self.slots.get(k))
            .ok_or(ChapterError::Missing)?;
        let at = match slot {
            Slot::Chapter(at) => *at,
            Slot::Picture => return Ok(()),
            Slot::Fail(e) => return Err(e.clone()),
        };
        let r = read_capped(&self.archive, at, self.limits.chapter_bytes, out);
        r.map_err(|e| match e {
            Capped::Over => ChapterError::TooLarge,
            Capped::Zip(zip::ZipError::Encrypted) => ChapterError::Encrypted,
            Capped::Zip(e) => ChapterError::Damaged(e.to_string()),
        })
    }
}

enum Capped {
    Over,
    Zip(zip::ZipError),
}

/// Entry `at` into `out` (cleared first, and again on an error), refused
/// past `cap` bytes on the declared size and on the bytes produced.
fn read_capped(
    archive: &zip::Archive<'_>,
    at: usize,
    cap: u64,
    out: &mut Vec<u8>,
) -> Result<(), Capped> {
    out.clear();
    let size = archive.entries()[at].size;
    if size > cap {
        return Err(Capped::Over);
    }
    // At most `cap` (checked just above) is reserved.
    out.try_reserve_exact(size as usize)
        .map_err(|_| Capped::Over)?;
    let mut over = false;
    let r = archive.read_to(at, &mut |chunk| {
        if (out.len() + chunk.len()) as u64 > cap {
            over = true;
            return Err(io::Error::other("over the limit"));
        }
        out.extend_from_slice(chunk);
        Ok(())
    });
    if let Err(e) = r {
        out.clear();
        return Err(if over { Capped::Over } else { Capped::Zip(e) });
    }
    Ok(())
}

/// A skeleton file's bytes, within `Limits::package_bytes`.
fn read_package(
    archive: &zip::Archive<'_>,
    at: usize,
    limits: &Limits,
) -> Result<Vec<u8>, BookError> {
    let mut v = Vec::new();
    match read_capped(archive, at, limits.package_bytes, &mut v) {
        Ok(()) => Ok(v),
        Err(Capped::Over) => Err(BookError::PackageTooLarge),
        Err(Capped::Zip(e)) => Err(BookError::Damaged(e.to_string())),
    }
}

/// The XML reader over a skeleton file, every event counted against
/// `Limits::package_nodes`. `each` gets the open element's local names
/// (the event's own last for a start tag) and the event.
fn walk<'a>(
    text: &'a str,
    path: &str,
    limits: &Limits,
    mut each: impl FnMut(&[&'a str], &Event<'a>),
) -> Result<(), BookError> {
    let xml_err = |error| BookError::PackageXml {
        path: path.to_string(),
        error,
    };
    let mut r = Reader::new(
        text,
        Options {
            html_entities: false,
            ..Options::default()
        },
    );
    // Local names of the open elements; bounded by the reader's depth
    // limit (256).
    let mut open: Vec<&'a str> = Vec::new();
    let mut nodes = 0usize;
    while let Some(ev) = r.next().map_err(xml_err)? {
        if !matches!(ev, Event::End(_)) {
            nodes += 1;
            if nodes > limits.package_nodes {
                return Err(BookError::PackageTooLarge);
            }
        }
        match &ev {
            Event::Start(e) => {
                open.push(e.name.local);
                each(&open, &ev);
            }
            Event::End(_) => {
                each(&open, &ev);
                open.pop();
            }
            Event::Text(_) => each(&open, &ev),
        }
    }
    Ok(())
}

fn decode<'b>(bytes: &'b [u8], path: &str) -> Result<std::borrow::Cow<'b, str>, BookError> {
    xml::decode(bytes).map_err(|error| BookError::PackageXml {
        path: path.to_string(),
        error,
    })
}

/// The `full-path` of the first rootfile of the package media type.
/// Element names are matched on their local part: the container's
/// vocabulary is fixed, and a book with a wrong namespace is still a book.
fn parse_container(bytes: &[u8], limits: &Limits) -> Result<Option<String>, BookError> {
    let text = decode(bytes, CONTAINER)?;
    let mut found = None;
    walk(&text, CONTAINER, limits, |_, ev| {
        if let Event::Start(e) = ev
            && found.is_none()
            && e.name.local == "rootfile"
            && e.attr(None, "media-type").map(str::trim) == Some(PACKAGE_TYPE)
            && let Some(p) = e.attr(None, "full-path")
            && !p.is_empty()
        {
            found = Some(p.to_string());
        }
    })?;
    Ok(found)
}

/// What the package document says.
struct Package {
    title: Option<String>,
    authors: Vec<String>,
    /// id -> (href, media type); the first item of an id counts.
    manifest: HashMap<String, (String, String)>,
    /// (idref, linear) in spine order.
    spine: Vec<(String, bool)>,
}

/// The package's metadata, manifest and spine (EPUB 2 and 3). The OPF
/// structure is matched on local names (its vocabulary is fixed); the
/// metadata needs the Dublin Core namespace.
fn parse_package(bytes: &[u8], path: &str, limits: &Limits) -> Result<Package, BookError> {
    #[derive(PartialEq)]
    enum Collect {
        No,
        Title,
        Creator,
    }
    let text = decode(bytes, path)?;
    let mut pkg = Package {
        title: None,
        authors: Vec::new(),
        manifest: HashMap::new(),
        spine: Vec::new(),
    };
    let mut collect = Collect::No;
    // The depth of the element being collected; its text is every text
    // event until its end tag (nested elements' text included).
    let mut depth = 0usize;
    let mut buf = String::new();
    // 0: before the first spine; 1: in it; 2: past it (later spines are
    // not read).
    let mut spine_state = 0u8;
    let mut spine_seen = false;
    let mut too_many = false;
    walk(&text, path, limits, |open, ev| {
        let parent = |n: usize| open.len().checked_sub(n + 1).map(|k| open[k]);
        match ev {
            Event::Start(e) => {
                let local = e.name.local;
                if collect == Collect::No && e.name.ns == Some(DC_NS) {
                    collect = match local {
                        "title" if pkg.title.is_none() => Collect::Title,
                        "creator" => Collect::Creator,
                        _ => Collect::No,
                    };
                    depth = open.len();
                    buf.clear();
                    return;
                }
                match (local, parent(1)) {
                    ("item", Some("manifest")) => {
                        if let (Some(id), Some(href)) = (e.attr(None, "id"), e.attr(None, "href")) {
                            let mt = e.attr(None, "media-type").unwrap_or("").trim();
                            pkg.manifest
                                .entry(id.to_string())
                                .or_insert_with(|| (href.to_string(), mt.to_string()));
                        }
                    }
                    ("spine", _) if spine_state == 0 => {
                        spine_state = 1;
                        spine_seen = true;
                    }
                    ("itemref", Some("spine")) if spine_state == 1 && !too_many => {
                        if pkg.spine.len() == MAX_SPINE {
                            too_many = true;
                            return;
                        }
                        let idref = e.attr(None, "idref").unwrap_or("").to_string();
                        let linear = e.attr(None, "linear").map(str::trim) != Some("no");
                        pkg.spine.push((idref, linear));
                    }
                    _ => {}
                }
            }
            Event::Text(t) => {
                if collect != Collect::No {
                    buf.push_str(t);
                }
            }
            Event::End(name) => {
                if collect != Collect::No && open.len() == depth {
                    let s = collapse(&buf);
                    if !s.is_empty() {
                        match collect {
                            Collect::Title => pkg.title = Some(s),
                            Collect::Creator => pkg.authors.push(s),
                            Collect::No => {}
                        }
                    }
                    collect = Collect::No;
                } else if name.local == "spine" && spine_state == 1 {
                    spine_state = 2;
                }
            }
        }
    })?;
    if too_many {
        return Err(BookError::TooManyChapters);
    }
    if !spine_seen || pkg.spine.is_empty() {
        return Err(BookError::NoSpine);
    }
    Ok(pkg)
}

/// `s` trimmed, inner whitespace runs as one space.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A normalized path that names something above the container root or an
/// absolute path.
fn escapes(path: &str) -> bool {
    path == ".." || path.starts_with("../") || path.starts_with('/')
}

/// The entry name `href` (relative to the folder `base`) names: fragment
/// cut, `%XX` decoded, normalized; `None` when it is outside the book (a
/// URL scheme, an absolute path, or above the container root, also when
/// that is only seen once decoded).
fn resolve(base: &str, href: &str) -> Option<String> {
    let href = href.split_once('#').map_or(href, |(p, _)| p);
    if has_scheme(href) || href.starts_with('/') {
        return None;
    }
    let decoded = percent_decode(href);
    let path = if base.is_empty() {
        normalize(&decoded)
    } else {
        normalize(&format!("{base}/{decoded}"))
    };
    (!escapes(&path)).then_some(path)
}

/// `href` starts with a URL scheme (RFC 3986: a letter, then letters,
/// digits, `+`, `-` or `.`, then `:`).
fn has_scheme(href: &str) -> bool {
    let Some((scheme, _)) = href.split_once(':') else {
        return false;
    };
    let mut c = scheme.bytes();
    c.next().is_some_and(|b| b.is_ascii_alphabetic())
        && c.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

/// The entries `encryption.xml` marks encrypted with a real cipher. Only
/// an `EncryptionMethod` and a `CipherData` that are direct children of an
/// `EncryptedData` count (an `EncryptedKey` inside its `KeyInfo` has its
/// own). An `EncryptedData` that states no method is encrypted (fail
/// closed); a URI outside the book names nothing in it.
fn parse_encryption(bytes: &[u8], limits: &Limits) -> Result<HashSet<String>, BookError> {
    let text = decode(bytes, ENCRYPTION)?;
    let mut out = HashSet::new();
    // Per open EncryptedData: its depth, its method, its URIs.
    let mut data: Vec<(usize, Option<String>, Vec<String>)> = Vec::new();
    walk(&text, ENCRYPTION, limits, |open, ev| match ev {
        Event::Start(e) => {
            let depth = open.len();
            match e.name.local {
                "EncryptedData" => data.push((depth, None, Vec::new())),
                "EncryptionMethod" => {
                    if let Some(d) = data.last_mut()
                        && d.0 + 1 == depth
                    {
                        d.1 = Some(e.attr(None, "Algorithm").unwrap_or("").trim().to_string());
                    }
                }
                "CipherReference" => {
                    if let Some(d) = data.last_mut()
                        && d.0 + 2 == depth
                        && open[depth - 2] == "CipherData"
                        && let Some(uri) = e.attr(None, "URI")
                        && let Some(path) = resolve("", uri.trim())
                    {
                        d.2.push(path);
                    }
                }
                _ => {}
            }
        }
        Event::End(name) => {
            if name.local == "EncryptedData"
                && data.last().is_some_and(|d| d.0 == open.len())
                && let Some((_, method, uris)) = data.pop()
                && !method.is_some_and(|m| OBFUSCATION.contains(&m.as_str()))
            {
                out.extend(uris);
            }
        }
        Event::Text(_) => {}
    })?;
    Ok(out)
}
