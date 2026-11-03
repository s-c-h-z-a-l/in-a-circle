//! `text.epub-to-md`: one EPUB book (foreign input) to one GitHub
//! Flavored Markdown file and the JPEG and PNG images its chapters show,
//! side by side (`<stem>.md`, `<stem>-0001.jpg`...). Two passes over the
//! spine: the first reads every chapter once for its headings only (their
//! text gives the book's unique heading ids, and their `id` attributes
//! where a link lands), the second reads one chapter at a time again,
//! rewrites its links and images, writes it with the GFM writer and
//! appends it; a chapter's tree is dropped before the next. A spine item
//! that cannot be read is a paragraph in its place, as in the plain-text
//! reading. Nothing is reported `done` before the whole book is written:
//! on a failure every file written for it is cut back to empty and
//! removed, so no half folder is left; and as no `done` came, the engine
//! publishes nothing of it even when the process dies first.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use action::{Events, Invocation};
use doc::md::ListEnd;
use doc::xhtml::{self, Slugs};
use doc::xhtml_read::{self, Ids, XhtmlError};
use doc::{Block, Dialect, Document, Inline, Limits, Loss};
use epub::read::{Book, ChapterError, ChapterRef};

use crate::epub_read::{
    MAX_WARN, SVG_TYPE, Sink, book_limit, chapter, chapter_with, external, ignore_sigxfsz,
    open_input, reason, unreadable,
};
use crate::join::{IMAGE_BYTES, MAX_PIXELS};
use crate::{LIMITS, Outcome, output_cap};

/// Most images written beside one book: 1023. The supervisor accepts at
/// most 1024 `done` lines from one action process (the md is the last),
/// and a join takes at most 1024 inputs, so the md and all its images
/// make one book again. Past it an image is its alt text.
const MAX_IMAGES: usize = 1023;

/// Most entries the heading index holds: the text family's node budget
/// (2^19), the most headings one document may have; a book's headings
/// and their ids together are held to what one document holds. Past it:
/// `book is too large`, nothing is written.
const INDEX_ENTRIES: usize = LIMITS.nodes;

/// Most bytes of text the heading index holds (heading ids, element ids
/// and chapter paths): the text family's input budget (8 MiB). The index
/// holds each about three times (the numbering's two sets and the
/// lookup), so at most about 24 MiB. Past it: `book is too large`.
const INDEX_BYTES: usize = LIMITS.input_bytes;

/// The images written for one book, and what each image entry turned out
/// to be. Holds one image's bytes at a time.
struct Images<'a> {
    dir: &'a Path,
    /// The input's stem: every image is named `<stem>-NNNN.<ext>`.
    stem: &'a str,
    /// Entry path -> its destination in the md (`None`: refused, its
    /// place keeps the alt text). Only entries that exist are kept, so at
    /// most the ZIP reader's 12,000 entries of at most 512 B: each entry
    /// is read (inflated) at most once, however often it is referenced.
    seen: HashMap<String, Option<String>>,
    /// Every file created for the book, the md and the images in order:
    /// removed unless the book finishes.
    written: Discard,
    /// Bytes still allowed to be read for images: twice the book file at
    /// first. Every image read is charged its entry's declared size
    /// before it is made, kept, refused or failed, so a book cannot make
    /// more than twice its own size be inflated for its images.
    budget: u64,
    /// The process's `RLIMIT_FSIZE`: an image larger is its alt text.
    cap: Option<u64>,
    /// An image's name was too long for the file system (`ENAMETOOLONG`:
    /// a long input stem, which the md's own shorter name still fits):
    /// every image's is as long, so none is read any more and each keeps
    /// its alt text; the book is still made.
    names_too_long: bool,
    buf: Vec<u8>,
}

impl Images<'_> {
    /// The md destination of the image `dest` (as written in `chapter`),
    /// or `None` when its place keeps the alt text: outside the book,
    /// missing, encrypted, damaged, neither JPEG nor PNG, too large, past
    /// the image budget or past [`MAX_IMAGES`]. `Err` is a write error:
    /// the book fails.
    fn place(
        &mut self,
        book: &Book<'_>,
        chapter: &ChapterRef,
        dest: &str,
    ) -> Result<Option<String>, String> {
        let Ok(path) = book.resolve(chapter, dest) else {
            return Ok(None);
        };
        if let Some(known) = self.seen.get(&path) {
            return Ok(known.clone());
        }
        if self.written.images.len() >= MAX_IMAGES || self.names_too_long {
            return Ok(None);
        }
        let placed = match book.resource_size(&path) {
            // No entry: nothing was read, nothing to remember.
            Err(ChapterError::Missing | ChapterError::OutsideTheBook) => return Ok(None),
            Err(_) => None,
            Ok(size) if size > IMAGE_BYTES.min(self.budget) => None,
            Ok(size) => {
                // Charged before the read, whatever it gives: the ZIP
                // reader inflates the whole entry before it can tell its
                // CRC or length is wrong, and never past its declared
                // size, so a damaged image costs the budget what it cost
                // to find out.
                self.budget -= size;
                match book.resource(&path, size, &mut self.buf) {
                    Err(_) => None,
                    Ok(()) => match image_ext(&self.buf) {
                        Some(ext) if self.cap.is_none_or(|c| self.buf.len() as u64 <= c) => {
                            self.write(ext)?
                        }
                        _ => None,
                    },
                }
            }
        };
        self.buf.clear();
        self.seen.insert(path, placed.clone());
        Ok(placed)
    }

    /// Writes the bytes read as the next image file; its md destination,
    /// or `None` when its name is too long (the image keeps its alt text).
    fn write(&mut self, ext: &str) -> Result<Option<String>, String> {
        let name = format!("{}-{:04}.{ext}", self.stem, self.written.images.len() + 1);
        let path = self.dir.join(&name);
        // Created new: an existing file is never overwritten.
        let mut f = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.raw_os_error() == Some(libc::ENAMETOOLONG) => {
                self.names_too_long = true;
                return Ok(None);
            }
            Err(e) => return Err(format!("cannot create output: {e}")),
        };
        self.written.images.push(path);
        f.write_all(&self.buf)
            .map_err(|e| format!("cannot write output: {e}"))?;
        Ok(Some(md_dest(&name)))
    }
}

/// The file extension of an image that may go beside the md: a JPEG or a
/// PNG the probe checks in full (valid structure, at most
/// [`MAX_PIXELS`]). A valid image the PDF writer cannot pass through
/// (alpha, interlace, 16 bits...) is still a valid JPEG or PNG for a
/// Markdown viewer and for the join, which takes any (`probe::sniff`), so
/// it is kept too; its format is the header's.
fn image_ext(bytes: &[u8]) -> Option<&'static str> {
    match probe::probe(bytes, MAX_PIXELS) {
        Ok(_) | Err(probe::ProbeError::Unsupported(_)) => {}
        Err(_) => return None,
    }
    probe::sniff(&mut &bytes[..], MAX_PIXELS)
        .ok()
        .map(|s| s.format.ext())
}

/// A file name as a link destination: `%`, `#` and `?` percent-encoded,
/// so a viewer does not take them for an escape, a fragment or a query,
/// and so is every control character (each of its UTF-8 bytes), which the
/// md writer would turn into a space (a line feed) or a link could not
/// hold; the join (which decodes `%XX`) finds the file. Everything else is
/// written as it is (the md writer puts a name with a space in `<...>`).
fn md_dest(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            c if c.is_control() => {
                for b in c.encode_utf8(&mut [0; 4]).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Where the links of the book land: per chapter entry (its first place
/// in the spine), the id of its first heading and the heading id each
/// `id` attribute names.
#[derive(Default)]
struct Index {
    chapters: HashMap<String, Targets>,
    entries: usize,
    bytes: usize,
}

#[derive(Default)]
struct Targets {
    first: Option<String>,
    ids: HashMap<String, String>,
}

impl Index {
    /// Charges `n` entries of `bytes` text; past either limit the book is
    /// too large.
    fn charge(&mut self, n: usize, bytes: usize) -> Result<(), String> {
        self.entries = self.entries.saturating_add(n);
        self.bytes = self.bytes.saturating_add(bytes);
        if self.entries > INDEX_ENTRIES || self.bytes > INDEX_BYTES {
            return Err("book is too large".into());
        }
        Ok(())
    }

    /// The md destination of the in-book link `dest` written in `chapter`:
    /// `#<anchor>` of the heading it lands on (the heading its fragment
    /// names, or without one the chapter's first heading), as a viewer
    /// names it ([`xhtml::md_anchor`]); `None` for any other place and for
    /// a heading with no anchor.
    fn target(&self, book: &Book<'_>, chapter: &ChapterRef, dest: &str) -> Option<String> {
        let path = book.resolve(chapter, dest).ok()?;
        let t = self.chapters.get(&path)?;
        let fragment = dest
            .find('#')
            .map(|k| &dest[k + 1..])
            .filter(|f| !f.is_empty());
        let id = match fragment {
            // Ids are kept as written; a fragment is a URL part, `%XX`
            // encoded.
            Some(f) => t.ids.get(&xhtml::percent_decode(f))?,
            None => t.first.as_ref()?,
        };
        // A heading with no anchor text (`!!!`) has no anchor to land on.
        (!id.is_empty()).then(|| format!("#{id}"))
    }
}

/// The first pass: every chapter's headings, numbered over the whole book
/// in spine order after the title heading, as a viewer or the join
/// numbers the md's headings ([`Slugs`]), each kept as a viewer's anchor
/// ([`xhtml::md_anchor`]: no `h-` before a digit). A chapter that cannot be read
/// here cannot be read in the second pass either (the same bytes, the
/// same reader) and has no heading there.
fn index(book: &Book<'_>, title: Option<&str>) -> Result<Index, String> {
    let mut index = Index::default();
    let mut slugs = Slugs::default();
    if let Some(t) = title {
        let id = slugs.unique(t);
        index.charge(1, id.len())?;
    }
    let mut raw = Vec::new();
    let mut texts = Vec::new();
    for c in &book.chapters {
        if book.chapter(c.index, &mut raw).is_err() || is_svg(book, c) {
            continue;
        }
        let Ok((d, ids)) = chapter_with(&raw, |text| headings_of(text, LIMITS)) else {
            continue;
        };
        texts.clear();
        xhtml::headings(&d.blocks, &mut texts);
        drop(d);
        let mut own = Vec::with_capacity(texts.len());
        for t in &texts {
            let id = slugs.unique(t);
            let anchor = xhtml::md_anchor(t, &id).to_string();
            index.charge(1, anchor.len())?;
            own.push(anchor);
        }
        // `chapter` read it, so the item exists.
        let Some(item) = book.item(c) else { continue };
        if index.chapters.contains_key(&item.path) {
            // A spine item repeated: links land on its first place.
            continue;
        }
        let mut targets = Targets {
            first: own.first().cloned(),
            ids: HashMap::new(),
        };
        let mut bytes = item.path.len() + targets.first.as_ref().map_or(0, String::len);
        for (ordinal, id) in ids.headings {
            if let Some(slug) = own.get(ordinal)
                && !targets.ids.contains_key(&id)
            {
                // The first element with an id is the one a link finds.
                bytes += id.len() + slug.len();
                targets.ids.insert(id, slug.clone());
            }
        }
        index.charge(1 + targets.ids.len(), bytes)?;
        index.chapters.insert(item.path.clone(), targets);
    }
    Ok(index)
}

/// A chapter's tree and heading ids. The ids are charged to the chapter's
/// text budget, so a chapter near it can fail with them and still read
/// without: then its headings are numbered all the same, and only no
/// `id` lands on them.
fn headings_of(text: &str, limits: Limits) -> Result<(Document, Ids), XhtmlError> {
    match xhtml_read::read_with_ids(text, limits) {
        Ok((d, _, ids)) => Ok((d, ids)),
        Err(XhtmlError::Limit(_)) => {
            xhtml_read::read_limited(text, limits).map(|(d, _)| (d, Ids::default()))
        }
        Err(e) => Err(e),
    }
}

fn is_svg(book: &Book<'_>, c: &ChapterRef) -> bool {
    book.item(c)
        .is_some_and(|it| it.media_type.eq_ignore_ascii_case(SVG_TYPE))
}

/// One book to `<stem>.md` and its images. Nothing is created before the
/// skeleton reads; `done` lines come only once everything is written.
pub(crate) fn process(
    inv: &Invocation,
    i: usize,
    input: &Path,
    ev: &mut Events<impl Write>,
) -> Outcome {
    let file = match open_input(input) {
        Ok(f) => f,
        Err(e) => {
            ev.fail(input, &e.to_string());
            return Outcome::Failed;
        }
    };
    let book_len = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            ev.fail(input, &e.to_string());
            return Outcome::Failed;
        }
    };
    let book = match epub::read::open(&file) {
        Ok(b) => b,
        Err(e) => {
            ev.fail(input, &e.to_string());
            return Outcome::Failed;
        }
    };
    let path = match action::output_path(&inv.work, i, input, "md") {
        Ok(p) => p,
        Err(e) => {
            ev.fail(input, &format!("cannot create output: {e}"));
            return Outcome::Failed;
        }
    };
    // The md's own name, so the images sit beside it under the same stem
    // (only the input's stem and a counter: never a name from the book).
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = path.parent().unwrap_or(&inv.work);
    let mut out = match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            ev.fail(input, &format!("cannot create output: {e}"));
            return Outcome::Failed;
        }
    };
    let mut images = Images {
        dir,
        stem: &stem,
        seen: HashMap::new(),
        written: Discard {
            md: Some(path.clone()),
            images: Vec::new(),
        },
        // Twice the book file: real images barely compress in the ZIP, so
        // a book's own images fit; a bomb stops here.
        budget: book_len.saturating_mul(2),
        cap: None,
        names_too_long: false,
        buf: Vec::new(),
    };
    match convert(&book, &mut out, &mut images) {
        Ok(done) => {
            // Images first, the md last; all only now that the book is
            // whole.
            for p in &images.written.images {
                ev.done(input, p.strip_prefix(&inv.work).unwrap_or(p));
            }
            ev.done(input, path.strip_prefix(&inv.work).unwrap_or(&path));
            for line in warnings(&done) {
                ev.warn(input, &line);
            }
            images.written.keep();
            Outcome::Done
        }
        Err(reason) => {
            // Cut first: should the removal fail, an empty file is left,
            // never a half book; the images go when `images` drops.
            let _ = out.set_len(0);
            drop(out);
            drop(images);
            ev.fail(input, &reason);
            Outcome::Failed
        }
    }
}

/// Removes what was written for a book that did not finish, on every way
/// out of `process` but success (an error; an unwinding panic in a test
/// build): each file is cut back to empty first, so a removal that fails
/// leaves no content. A shipped build aborts on a panic (`panic =
/// "abort"`), so nothing is removed then; no `done` was sent, so the
/// engine publishes nothing of the book either way.
struct Discard {
    md: Option<PathBuf>,
    images: Vec<PathBuf>,
}

impl Discard {
    /// The book finished: everything stays.
    fn keep(&mut self) {
        self.md = None;
        self.images.clear();
    }
}

impl Drop for Discard {
    fn drop(&mut self) {
        for p in self.md.iter().chain(&self.images) {
            if let Ok(f) = OpenOptions::new().write(true).open(p) {
                let _ = f.set_len(0);
            }
            let _ = fs::remove_file(p);
        }
    }
}

/// What a converted book reports.
struct Converted {
    unread: usize,
    /// Image references whose place keeps the alt text.
    replaced: usize,
    losses: BTreeSet<Loss>,
}

/// One counted line for the chapters that could not be read, one for the
/// images replaced by their alt text, then each loss once. The count of
/// images says what the images loss would, so with it the images loss
/// has no line of its own: at most 1 + 1 + (every kind but one), the
/// supervisor's cap, held by a test over every kind.
fn warnings(done: &Converted) -> Vec<String> {
    let mut lines = Vec::new();
    if done.unread > 0 {
        lines.push(format!("{} chapter(s) could not be read", done.unread));
    }
    if done.replaced > 0 {
        lines.push(format!("{} image(s) replaced by alt text", done.replaced));
    }
    lines.extend(
        done.losses
            .iter()
            .filter(|&&l| !(l == Loss::Images && done.replaced > 0))
            .map(|l| l.message().to_string()),
    );
    debug_assert!(lines.len() <= MAX_WARN, "{} warn lines", lines.len());
    lines
}

/// Writes the header and every spine item of `book` to `file`, the images
/// through `images`.
fn convert(book: &Book<'_>, file: &mut File, images: &mut Images<'_>) -> Result<Converted, String> {
    let cap = output_cap().map_err(reason)?;
    images.cap = cap;
    ignore_sigxfsz();
    let index = index(book, book.title.as_deref())?;
    let mut sink = Sink::new(file, cap, "Markdown");
    sink.write(&header(book)?)?;
    // Every book has styles, and none is read.
    let mut losses = BTreeSet::from([Loss::Styles]);
    let (mut unread, mut read, mut replaced, mut text, mut any) =
        (0usize, 0usize, 0usize, false, false);
    let mut raw = Vec::new();
    // The list the md written so far ends with: the next chapter's first
    // list of the same kind takes the other marker, so a reader keeps the
    // two apart (the header ends with no list).
    let mut last: Option<ListEnd> = None;
    for c in &book.chapters {
        let part = match book.chapter(c.index, &mut raw) {
            Ok(()) if is_svg(book, c) => {
                read += 1;
                losses.insert(Loss::Images);
                continue;
            }
            Ok(()) => match chapter(&raw) {
                Ok((mut d, more)) => {
                    read += 1;
                    // Before the rewrite: an image's alt text is not text.
                    text |= xhtml_read::has_text(&d);
                    losses.extend(more);
                    replaced += rewrite(&mut d, book, c, &index, images, &mut losses)?;
                    let (s, more, end) = chapter_md(&d, sink.written, last)?;
                    losses.extend(more);
                    (s, end)
                }
                Err(why) => {
                    unread += 1;
                    (paragraph_md(&unreadable(c.index, &why))?, None)
                }
            },
            Err(e) => {
                unread += 1;
                (paragraph_md(&unreadable(c.index, &e.reason()))?, None)
            }
        };
        let (part, end) = part;
        if part.is_empty() {
            continue;
        }
        last = end;
        if any {
            sink.write("\n")?;
        }
        sink.write(&part)?;
        any = true;
    }
    if read == 0 {
        return Err("no chapter could be read".into());
    }
    if !text && images.written.images.is_empty() {
        // A picture book whose image names did not fit: say so, not that
        // it has no text.
        return Err(if images.names_too_long {
            "image names too long for this folder"
        } else {
            "book has no text"
        }
        .into());
    }
    Ok(Converted {
        unread,
        replaced,
        losses,
    })
}

/// `# <title>`, then the authors joined by `, ` as a paragraph, then a
/// blank line; what the book lacks is left out, and with neither, the
/// header and its blank line. Written by the GFM writer, so a title is
/// escaped as any heading.
fn header(book: &Book<'_>) -> Result<String, String> {
    let mut blocks = Vec::new();
    if let Some(t) = &book.title {
        blocks.push(Block::Heading {
            level: 1,
            content: vec![Inline::Text(t.clone())],
        });
    }
    if !book.authors.is_empty() {
        blocks.push(Block::Paragraph(vec![Inline::Text(
            book.authors.join(", "),
        )]));
    }
    let (mut h, _) = doc::md::write_limited_with(&Document { blocks }, LIMITS, Dialect::Gfm)
        .map_err(book_limit)?;
    if !h.is_empty() {
        h.push('\n');
    }
    Ok(h)
}

/// The line that stands in place of a chapter, as a paragraph: its `[`
/// and `]` escaped by the writer, so it is never read as a link.
fn paragraph_md(line: &str) -> Result<String, String> {
    let d = Document {
        blocks: vec![Block::Paragraph(vec![Inline::Text(
            line.trim_end().to_string(),
        )])],
    };
    doc::md::write_limited_with(&d, LIMITS, Dialect::Gfm)
        .map(|(s, _)| s)
        .map_err(book_limit)
}

/// One chapter's Markdown under what is left of the book's output budget
/// after `written` bytes, after a part that ended with the list `after`;
/// and the list it ends with.
fn chapter_md(
    d: &Document,
    written: usize,
    after: Option<ListEnd>,
) -> Result<(String, BTreeSet<Loss>, Option<ListEnd>), String> {
    let left = Limits {
        output_bytes: LIMITS.output_bytes.saturating_sub(written),
        ..LIMITS
    };
    doc::md::write_part(d, left, Dialect::Gfm, after).map_err(book_limit)
}

/// Whether a link keeps its destination as it is: `http:`, `https:`,
/// `mailto:` or another host (`//host/...`). Any other scheme
/// (`javascript:`, `data:`, `file:`...) keeps only its text, so the md
/// never carries a link that runs or reads something on the reader's
/// machine.
fn kept_external(dest: &str) -> bool {
    if !external(dest) {
        return false;
    }
    let scheme = dest.split_once(':').map_or("", |(s, _)| s);
    dest.starts_with("//")
        || ["http", "https", "mailto"]
            .iter()
            .any(|s| scheme.eq_ignore_ascii_case(s))
}

/// Rewrites chapter `c`'s links and images for the md: a link into the
/// book lands on its heading's id or keeps only its text
/// ([`Loss::Links`]); an image is written beside the md and named by its
/// file, or its place keeps the alt text (counted, the return value).
/// One walk in document order (a block, then the blocks inside it; an
/// inline, then the inlines inside it), so images are numbered in the
/// order of their first reference. Explicit stacks, no recursion; the
/// content of a link that goes is spliced in its place and walked as
/// part of its parent.
fn rewrite(
    d: &mut Document,
    book: &Book<'_>,
    c: &ChapterRef,
    index: &Index,
    images: &mut Images<'_>,
    losses: &mut BTreeSet<Loss>,
) -> Result<usize, String> {
    let mut replaced = 0;
    for is in inline_runs(&mut d.blocks) {
        let mut stack = vec![Frame {
            todo: std::mem::take(is).into(),
            out: Vec::new(),
            wrap: Wrap::Run,
        }];
        while let Some(f) = stack.last_mut() {
            let Some(i) = f.todo.pop_front() else {
                let f = stack.pop().expect("the frame just looked at");
                let node = match f.wrap {
                    Wrap::Run => {
                        *is = f.out;
                        break;
                    }
                    Wrap::Emph => Inline::Emph(f.out),
                    Wrap::Strong => Inline::Strong(f.out),
                    Wrap::Strikethrough => Inline::Strikethrough(f.out),
                    Wrap::Link { dest, title, auto } => Inline::Link {
                        dest,
                        title,
                        content: f.out,
                        auto,
                    },
                };
                if let Some(parent) = stack.last_mut() {
                    parent.out.push(node);
                }
                continue;
            };
            let (content, wrap) = match i {
                Inline::Emph(content) => (content, Wrap::Emph),
                Inline::Strong(content) => (content, Wrap::Strong),
                Inline::Strikethrough(content) => (content, Wrap::Strikethrough),
                Inline::Link {
                    dest,
                    title,
                    content,
                    auto,
                } => {
                    let to = if kept_external(&dest) {
                        Some(dest)
                    } else if external(&dest) {
                        None
                    } else {
                        index.target(book, c, &dest)
                    };
                    let Some(dest) = to else {
                        losses.insert(Loss::Links);
                        for x in content.into_iter().rev() {
                            f.todo.push_front(x);
                        }
                        continue;
                    };
                    (content, Wrap::Link { dest, title, auto })
                }
                Inline::Image { dest, title, alt } => {
                    match images.place(book, c, &dest)? {
                        Some(dest) => f.out.push(Inline::Image { dest, title, alt }),
                        None => {
                            replaced += 1;
                            let mut s = String::new();
                            xhtml::plain(&alt, &mut s);
                            if !s.is_empty() {
                                f.out.push(Inline::Text(s));
                            }
                        }
                    }
                    continue;
                }
                other @ (Inline::Text(_)
                | Inline::Code(_)
                | Inline::SoftBreak
                | Inline::HardBreak
                | Inline::Html(_)) => {
                    f.out.push(other);
                    continue;
                }
            };
            stack.push(Frame {
                todo: content.into(),
                out: Vec::new(),
                wrap,
            });
        }
    }
    Ok(replaced)
}

/// An inline list being rebuilt by [`rewrite`]: what is left of it, what
/// it has become so far, and the node it belongs to.
struct Frame {
    todo: VecDeque<Inline>,
    out: Vec<Inline>,
    wrap: Wrap,
}

/// The node a rebuilt inline list goes back into.
enum Wrap {
    /// A paragraph's, heading's or cell's own list: the walk's root.
    Run,
    Emph,
    Strong,
    Strikethrough,
    Link {
        dest: String,
        title: String,
        auto: bool,
    },
}

/// Every block-level inline list of `blocks` (paragraphs, headings, table
/// cells) in document order: a block, then the blocks inside it.
fn inline_runs(blocks: &mut [Block]) -> Vec<&mut Vec<Inline>> {
    let mut runs = Vec::new();
    let mut stack: Vec<Box<dyn Iterator<Item = &mut Block> + '_>> =
        vec![Box::new(blocks.iter_mut())];
    while let Some(top) = stack.last_mut() {
        let Some(b) = top.next() else {
            stack.pop();
            continue;
        };
        match b {
            Block::Paragraph(c) | Block::Heading { content: c, .. } => runs.push(c),
            Block::BlockQuote(inner) => stack.push(Box::new(inner.iter_mut())),
            Block::List { items, .. } => {
                stack.push(Box::new(items.iter_mut().flat_map(|i| i.blocks.iter_mut())));
            }
            Block::Table { head, rows, .. } => {
                runs.extend(head.iter_mut());
                runs.extend(rows.iter_mut().flatten());
            }
            Block::CodeBlock { .. } | Block::ThematicBreak | Block::HtmlBlock(_) => {}
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epub_read::tests::every_loss;

    /// Every loss kind, the chapters line and the images line stay within
    /// the supervisor's `warn` cap: the images count takes the images
    /// loss's place.
    #[test]
    fn warn_lines_fit_the_supervisor_cap() {
        let all = every_loss();
        let done = Converted {
            unread: 3,
            replaced: 2,
            losses: all.iter().copied().collect(),
        };
        let lines = warnings(&done);
        assert_eq!(lines.len(), MAX_WARN);
        assert_eq!(lines[0], "3 chapter(s) could not be read");
        assert_eq!(lines[1], "2 image(s) replaced by alt text");
        assert!(!lines.iter().any(|l| l == Loss::Images.message()));
        // No image replaced: the images loss (an SVG page) has its line.
        let done = Converted {
            replaced: 0,
            ..done
        };
        let lines = warnings(&done);
        assert_eq!(lines.len(), MAX_WARN);
        assert!(lines.iter().any(|l| l == Loss::Images.message()));
    }

    #[test]
    fn file_names_as_destinations() {
        assert_eq!(md_dest("a b-0001.jpg"), "a b-0001.jpg");
        assert_eq!(md_dest("100%#1?-0001.png"), "100%25%231%3F-0001.png");
        assert_eq!(md_dest("Árvíztűrő-0001.jpg"), "Árvíztűrő-0001.jpg");
        assert_eq!(
            md_dest("new\nline\r\ttab\u{7f}\u{85}-0001.png"),
            "new%0Aline%0D%09tab%7F%C2%85-0001.png"
        );
    }

    #[test]
    fn only_web_and_mail_links_stay() {
        for d in ["http://x", "HTTPS://x", "mailto:a@b", "//host/x"] {
            assert!(kept_external(d), "{d}");
        }
        for d in [
            "javascript:alert(1)",
            "data:text/html,x",
            "file:///etc",
            "#x",
            "a.xhtml",
        ] {
            assert!(!kept_external(d), "{d}");
        }
    }
}
