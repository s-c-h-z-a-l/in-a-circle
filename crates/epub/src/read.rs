//! EPUB reader: the book's skeleton (the OCF container, the package
//! document, its metadata, manifest and spine) and each spine item's raw
//! bytes, one at a time, from a book on disk that is foreign input.
//!
//! `open` reads the ZIP directory (`zip::open`, with the limits below),
//! `META-INF/container.xml`, the first package rootfile it names and the
//! package's `dc:title`, `dc:creator`, manifest and spine (EPUB 2 and 3:
//! the spine is the same in both; the `mimetype` entry is not required,
//! many real books do not store it first). Every spine item is resolved
//! to its ZIP entry then, each manifest item once however often the spine
//! names it; [`Book::chapter`] reads one when asked and keeps nothing. A
//! broken skeleton fails the whole book ([`BookError`]); a spine item that
//! cannot be read fails only itself ([`ChapterError`]), and the caller
//! reports it in its place.
//!
//! Paths: an `href` is relative to the package document's folder, its
//! `?query` and `#fragment` cut, `%XX` decoded and normalized lexically;
//! entry names are matched exactly (case-sensitive), as the ZIP holds
//! them. An href with a URL scheme, an absolute one, or one that climbs
//! above the container root is outside the book and never looked up.
//! Media types are compared by their `type/subtype`, ASCII case ignored,
//! parameters cut.
//!
//! Resources: [`Book::resolve`] resolves an `href` or `src` of a chapter
//! by the same rules, relative to the chapter's folder (an empty one, or
//! only a fragment, is the chapter itself); [`Book::resource`] reads one
//! entry by its exact name under a size cap the caller gives, checked on
//! the declared size before anything is reserved and on the bytes
//! produced.
//!
//! DRM: `META-INF/encryption.xml` lists the encrypted resources
//! (`EncryptedData`, its `EncryptionMethod` and `CipherReference URI`,
//! relative to the container root). Font obfuscation (the IDPF and Adobe
//! algorithms) is not encryption and is ignored; any other method (also
//! next to an obfuscation one), or none stated, is. A ZIP entry with an
//! encryption flag is encrypted too. When every XHTML spine item that is
//! in the book is encrypted, the book is DRM-protected; when only some
//! are, those items fail alone. `rights.xml` decides nothing.

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
/// split into a few hundred files. A spine item costs a few dozen bytes
/// here (its idref, until `open` returns, and a [`ChapterRef`]); the
/// manifest item it names at most about 1 KB, once ([`MAX_PATH`],
/// [`MAX_MEDIA_TYPE`]). Past it: `book has more than 10000 chapters`.
pub const MAX_SPINE: usize = 10_000;

/// Longest path an [`Item`] keeps: 512 bytes, the ZIP reader's name limit
/// (`ZIP_NAME_BYTES`). A resolved path longer than that can name no entry,
/// so the item is `missing`, and only its first 512 bytes are kept (an
/// href outside the book likewise), so a spine of 10,000 items naming one
/// href of megabytes holds 512 bytes, not 10,000 copies.
pub const MAX_PATH: usize = ZIP_NAME_BYTES;

/// Longest media type (its `type/subtype`, parameters cut): 255 bytes,
/// RFC 6838 section 4.2 (type and subtype names at most 127 characters
/// each, and the `/`). A longer one is no
/// media type: the item is `unsupported type <first 255 bytes>...` (cut at
/// a character boundary, `...` appended), and only that is kept.
pub const MAX_MEDIA_TYPE: usize = 255;

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
    /// Most XML nodes (elements, attributes and text runs) one skeleton
    /// file may have.
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
    /// Neither XHTML nor SVG: the item's media type (empty when it states
    /// none; the reason then says `(none)`).
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
            // An item that states no media type: `(none)`, not a blank.
            Self::UnsupportedType(t) if t.is_empty() => "unsupported type (none)".into(),
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
    /// The manifest item it names, an index into [`Book::items`]; `None`
    /// when the manifest has no such id.
    pub item: Option<usize>,
    /// False for `linear="no"`; such an item is still read in its place.
    pub linear: bool,
}

/// A manifest item the spine names, resolved once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The entry it resolves to; or, when it is outside the book, its
    /// `href` as written. At most [`MAX_PATH`] bytes and `...` (a longer
    /// one is cut at a character boundary): 515 bytes.
    pub path: String,
    /// The essence of the manifest's `media-type` (`type/subtype` as
    /// written, parameters and blanks cut); at most
    /// [`MAX_MEDIA_TYPE`] bytes and `...` (a longer one is cut).
    pub media_type: String,
}

/// How a manifest item was resolved at `open`.
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
    /// The manifest items the spine names, each once, in the order the
    /// spine first names them.
    pub items: Vec<Item>,
    archive: zip::Archive<'f>,
    /// The archive's file entries (not folders) by name: indices into
    /// `archive.entries()`, sorted by name. 8 bytes per entry, 96 KB at
    /// the 12,000-entry limit.
    index: Vec<usize>,
    /// The entries `encryption.xml` marks encrypted, sorted (a ZIP entry's
    /// own flag is on the entry).
    locked: Vec<usize>,
    /// One per item.
    slots: Vec<Slot>,
    limits: Limits,
    steps: u64,
}

/// The entries of `index` (see [`Book::index`]) sorted by name.
fn by_name(archive: &zip::Archive<'_>) -> Vec<usize> {
    let entries = archive.entries();
    let mut index: Vec<usize> = (0..entries.len())
        .filter(|&i| !entries[i].is_dir())
        .collect();
    index.sort_unstable_by(|&a, &b| entries[a].name.cmp(&entries[b].name));
    index
}

/// The file entry named exactly `name`. The ZIP reader has refused
/// duplicate names, so a name is at most one entry.
fn find(archive: &zip::Archive<'_>, index: &[usize], name: &[u8]) -> Option<usize> {
    let entries = archive.entries();
    index
        .binary_search_by(|&i| entries[i].name.as_slice().cmp(name))
        .ok()
        .map(|k| index[k])
}

/// Open the book in `file` with [`Limits::DEFAULT`].
pub fn open(file: &File) -> Result<Book<'_>, BookError> {
    open_with(file, Limits::DEFAULT)
}

/// Open the book in `file` with these limits.
pub fn open_with(file: &File, limits: Limits) -> Result<Book<'_>, BookError> {
    let archive = zip::open(file, ZIP_LIMITS).map_err(|e| BookError::Damaged(e.to_string()))?;
    let index = by_name(&archive);
    let names = |name: &str| find(&archive, &index, name.as_bytes());

    let at = names(CONTAINER).ok_or(BookError::NoContainer)?;
    let bytes = read_package(&archive, at, &limits)?;
    let rootfile = parse_container(&bytes, &limits)?.ok_or(BookError::NoPackage)?;
    let opf_path = normalize(&percent_decode(&rootfile));
    if escapes(&opf_path) {
        return Err(BookError::NoPackage);
    }
    let at = names(&opf_path).ok_or(BookError::NoPackage)?;
    let bytes = read_package(&archive, at, &limits)?;
    let pkg = parse_package(&bytes, &opf_path, &limits)?;
    drop(bytes);
    let encrypted = match names(ENCRYPTION) {
        None => HashSet::new(),
        Some(at) => parse_encryption(&read_package(&archive, at, &limits)?, &limits)?,
    };
    // The entries it names (a URI naming none names nothing).
    let mut ciphered: Vec<usize> = encrypted.iter().filter_map(|p| names(p)).collect();
    drop(encrypted);
    ciphered.sort_unstable();

    let base = opf_path.rsplit_once('/').map_or("", |(dir, _)| dir);
    // Manifest item -> its index in `items`, set when the spine first
    // names it: each item is resolved once, however often it is named.
    let mut seen: Vec<Option<usize>> = vec![None; pkg.items.len()];
    let mut items = Vec::new();
    let mut slots = Vec::new();
    // Per item: `Some(encrypted)` for an XHTML item in the book.
    let mut xhtml_locked: Vec<Option<bool>> = Vec::new();
    let mut steps: u64 = 0;
    let mut chapters = Vec::with_capacity(pkg.spine.len());
    let mut total: u64 = 0;
    // XHTML spine items in the book: encrypted ones, and ones that are not.
    let (mut locked, mut unlocked) = (0usize, 0usize);
    for (k, (idref, linear)) in pkg.spine.iter().enumerate() {
        let item = pkg.manifest.get(idref.as_str()).map(|&m| {
            *seen[m].get_or_insert_with(|| {
                let (href, media_type) = &pkg.items[m];
                steps += (href.len() + media_type.len()) as u64;
                let (item, slot, xhtml) =
                    resolve_item(base, href, media_type, &names, &archive, &ciphered, &limits);
                items.push(item);
                slots.push(slot);
                xhtml_locked.push(xhtml);
                items.len() - 1
            })
        });
        if let Some(at) = item {
            match xhtml_locked[at] {
                Some(true) => locked += 1,
                Some(false) => unlocked += 1,
                None => {}
            }
            if let Slot::Chapter(i) = slots[at] {
                total = total.saturating_add(archive.entries()[i].size);
            }
        }
        chapters.push(ChapterRef {
            index: k + 1,
            item,
            linear: *linear,
        });
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
        items,
        archive,
        index,
        locked: ciphered,
        slots,
        limits,
        steps,
    })
}

/// One manifest item: what the book keeps of it, how it reads, and, for
/// an XHTML item in the book, whether it is encrypted. Linear in the
/// lengths of `href` and `media_type`.
fn resolve_item(
    base: &str,
    href: &str,
    media_type: &str,
    names: &dyn Fn(&str) -> Option<usize>,
    archive: &zip::Archive<'_>,
    locked: &[usize],
    limits: &Limits,
) -> (Item, Slot, Option<bool>) {
    let media_type = essence(media_type);
    let shown_type = cut(media_type, MAX_MEDIA_TYPE);
    let item = |path: &str| Item {
        path: cut(path, MAX_PATH),
        media_type: shown_type.clone(),
    };
    let Some(path) = resolve(base, href) else {
        return (item(href), Slot::Fail(ChapterError::OutsideTheBook), None);
    };
    // A path longer than any entry name is looked up nowhere.
    let at = (path.len() <= MAX_PATH).then(|| names(&path)).flatten();
    let Some(i) = at else {
        return (item(&path), Slot::Fail(ChapterError::Missing), None);
    };
    let entry = &archive.entries()[i];
    let is_locked = entry.encrypted || locked.binary_search(&i).is_ok();
    let xhtml = media_type
        .eq_ignore_ascii_case(XHTML_TYPE)
        .then_some(is_locked);
    let slot = if is_locked {
        Slot::Fail(ChapterError::Encrypted)
    } else if media_type.len() > MAX_MEDIA_TYPE {
        Slot::Fail(ChapterError::UnsupportedType(shown_type.clone()))
    } else {
        classify(entry, i, media_type, limits)
    };
    (item(&path), slot, xhtml)
}

/// `s` if it is at most `max` bytes; else its longest prefix of at most
/// `max` bytes that ends at a character boundary, and `...`.
fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// What a resolved spine item in entry `i` is, by its media type's
/// essence (ASCII case ignored).
fn classify(entry: &zip::Entry, i: usize, media_type: &str, limits: &Limits) -> Slot {
    if media_type.eq_ignore_ascii_case(SVG_TYPE) {
        Slot::Picture
    } else if !media_type.eq_ignore_ascii_case(XHTML_TYPE) {
        Slot::Fail(ChapterError::UnsupportedType(media_type.to_string()))
    } else if entry.size > limits.chapter_bytes {
        Slot::Fail(ChapterError::TooLarge)
    } else {
        Slot::Chapter(i)
    }
}

/// A media type's essence, `type/subtype`: its parameters (`; charset=…`)
/// cut and blanks trimmed. Compare it ignoring ASCII case (RFC 6838
/// section 4.2: type and subtype names are case-insensitive).
fn essence(media_type: &str) -> &str {
    media_type
        .split_once(';')
        .map_or(media_type, |(t, _)| t)
        .trim()
}

impl Book<'_> {
    /// The manifest item spine item `c` names (`None` when there is none).
    pub fn item(&self, c: &ChapterRef) -> Option<&Item> {
        c.item.and_then(|k| self.items.get(k))
    }

    /// Bytes of hrefs and media types `open` examined to resolve the spine:
    /// each manifest item the spine names once, so at most the package
    /// document's length (plus the UTF-16 to UTF-8 growth). For the
    /// linearity tests.
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// The raw bytes of spine item `i` (1-based, as [`ChapterRef::index`])
    /// in `out`, which is cleared first and left empty on an error. An SVG
    /// page reads as nothing (`Ok` with `out` empty); an index that is no
    /// spine item is `Missing`. Nothing is kept: the same item can be read
    /// again.
    pub fn chapter(&self, i: usize, out: &mut Vec<u8>) -> Result<(), ChapterError> {
        out.clear();
        let slot = i
            .checked_sub(1)
            .and_then(|k| self.chapters.get(k))
            .and_then(|c| c.item)
            .and_then(|k| self.slots.get(k))
            .ok_or(ChapterError::Missing)?;
        let at = match slot {
            Slot::Chapter(at) => *at,
            Slot::Picture => return Ok(()),
            Slot::Fail(e) => return Err(e.clone()),
        };
        let r = read_capped(&self.archive, at, self.limits.chapter_bytes, out);
        r.map_err(chapter_error)
    }

    /// The entry name `href` (an `href` or `src` written in spine item
    /// `chapter`) names: relative to the chapter's folder, with the rules
    /// of the spine's hrefs (query and fragment cut, `%XX` decoded,
    /// normalized lexically). An href that is empty once they are cut (a
    /// link to `#id`) names the chapter itself. Nothing is looked up: the
    /// entry need not exist ([`Book::resource`] says `Missing`).
    ///
    /// `OutsideTheBook`: the href has a URL scheme (`http:`, `data:`...),
    /// is absolute (`/x`, `//host/x`) or climbs above the container root,
    /// or the chapter itself is outside the book. `Missing`: the spine
    /// item names no manifest item of this book, or the path is longer
    /// than [`MAX_PATH`] (no entry has such a name). Linear in the
    /// lengths of `href` and the chapter's path.
    pub fn resolve(&self, chapter: &ChapterRef, href: &str) -> Result<String, ChapterError> {
        let k = chapter.item.ok_or(ChapterError::Missing)?;
        let (Some(item), Some(slot)) = (self.items.get(k), self.slots.get(k)) else {
            return Err(ChapterError::Missing);
        };
        if let Slot::Fail(ChapterError::OutsideTheBook) = slot {
            return Err(ChapterError::OutsideTheBook);
        }
        // A cut path (`...` appended) is no entry's: no folder to start from.
        if item.path.len() > MAX_PATH {
            return Err(ChapterError::Missing);
        }
        let own = href.find(['?', '#']).map_or(href, |end| &href[..end]);
        let path = if own.is_empty() {
            item.path.clone()
        } else {
            let base = item.path.rsplit_once('/').map_or("", |(dir, _)| dir);
            resolve(base, href).ok_or(ChapterError::OutsideTheBook)?
        };
        if path.len() > MAX_PATH {
            return Err(ChapterError::Missing);
        }
        Ok(path)
    }

    /// The bytes of the file entry named exactly `path` (as
    /// [`Book::resolve`] gives it) in `out`, cleared first and left empty
    /// on an error; at most `cap` bytes, checked on the entry's declared
    /// size before anything is reserved or inflated, and on the bytes
    /// produced. `TooLarge`: over `cap`. `Missing`: no such file entry (a
    /// folder entry is none). `Encrypted`: the ZIP entry's flag, or
    /// `encryption.xml` names it with a cipher. `Damaged`: the ZIP
    /// reader's reason (CRC, method, deflate stream). `OutsideTheBook`:
    /// `path` is absolute or climbs above the root (never a name
    /// [`Book::resolve`] gives). Nothing is kept.
    pub fn resource(&self, path: &str, cap: u64, out: &mut Vec<u8>) -> Result<(), ChapterError> {
        out.clear();
        if escapes(path) {
            return Err(ChapterError::OutsideTheBook);
        }
        let at = (path.len() <= MAX_PATH)
            .then(|| find(&self.archive, &self.index, path.as_bytes()))
            .flatten()
            .ok_or(ChapterError::Missing)?;
        if self.archive.entries()[at].encrypted || self.locked.binary_search(&at).is_ok() {
            return Err(ChapterError::Encrypted);
        }
        read_capped(&self.archive, at, cap, out).map_err(chapter_error)
    }
}

/// A read's failure as a spine item's or resource's reason.
fn chapter_error(e: Capped) -> ChapterError {
    match e {
        Capped::Over => ChapterError::TooLarge,
        Capped::Zip(zip::ZipError::Encrypted) => ChapterError::Encrypted,
        Capped::Zip(e) => ChapterError::Damaged(e.to_string()),
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

/// The XML reader over a skeleton file, every element, attribute and text
/// run counted against `Limits::package_nodes`. `each` gets the open
/// element's local names (the event's own last for a start tag) and the
/// event.
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
        // An element, each of its attributes and a text run are a node
        // each: an attribute is structure the reader holds as much as an
        // element is.
        nodes += match &ev {
            Event::Start(e) => 1 + e.attrs().count(),
            Event::Text(_) => 1,
            Event::End(_) => 0,
        };
        if nodes > limits.package_nodes {
            return Err(BookError::PackageTooLarge);
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
            && e.attr(None, "media-type")
                .is_some_and(|t| essence(t).eq_ignore_ascii_case(PACKAGE_TYPE))
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
    /// id -> index into `items`; the first item of an id counts.
    manifest: HashMap<String, usize>,
    /// (href, media type) of each manifest item that counts.
    items: Vec<(String, String)>,
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
        items: Vec::new(),
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
                            if !pkg.manifest.contains_key(id) {
                                pkg.manifest.insert(id.to_string(), pkg.items.len());
                                pkg.items.push((href.to_string(), mt.to_string()));
                            }
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

/// The entry name `href` (relative to the folder `base`) names: query and
/// fragment cut (the first `?` or `#` and all after it, before decoding),
/// `%XX` decoded, normalized; `None` when it is outside the book (a
/// URL scheme, an absolute path, or above the container root, also when
/// that is only seen once decoded).
fn resolve(base: &str, href: &str) -> Option<String> {
    let href = href.find(['?', '#']).map_or(href, |end| &href[..end]);
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
/// closed), and so is one that states a cipher among other methods, in
/// any order; a URI outside the book names nothing in it.
fn parse_encryption(bytes: &[u8], limits: &Limits) -> Result<HashSet<String>, BookError> {
    let text = decode(bytes, ENCRYPTION)?;
    let mut out = HashSet::new();
    // Per open EncryptedData: its depth, whether it names a cipher (`None`
    // until a method is seen), its URIs.
    let mut data: Vec<(usize, Option<bool>, Vec<String>)> = Vec::new();
    walk(&text, ENCRYPTION, limits, |open, ev| match ev {
        Event::Start(e) => {
            let depth = open.len();
            match e.name.local {
                "EncryptedData" => data.push((depth, None, Vec::new())),
                "EncryptionMethod" => {
                    if let Some(d) = data.last_mut()
                        && d.0 + 1 == depth
                    {
                        let alg = e.attr(None, "Algorithm").unwrap_or("").trim();
                        // Once a cipher, always: a later obfuscation method
                        // does not make the data plain.
                        d.1 = Some(d.1 == Some(true) || !OBFUSCATION.contains(&alg));
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
                && let Some((_, cipher, uris)) = data.pop()
                && cipher != Some(false)
            {
                out.extend(uris);
            }
        }
        Event::Text(_) => {}
    })?;
    Ok(out)
}
