//! `text.md-join-epub`: several Markdown files and the images they use to
//! one EPUB. Events as the other join actions: `skip`/`fail`/`warn` per
//! input as it happens, `take` per taken input once the book is finished,
//! `joined` once. A document that fails leaves the rest; a damaged image
//! fails alone and its place keeps the alt text; the book as a whole failing
//! (a ceiling, a limit, a write error) fails every input it would have
//! taken and leaves no file. Exit: 0 with a `joined`; 3 when nothing was
//! taken and nothing failed; else 1.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use action::{Events, Invocation};
use doc::{Dialect, Document, LimitError, ReadError};
use epub::{Image, ImageKind, ImageSource, Part};
use probe::{Format, ProbeError};

use crate::{
    Conversion, ConvertError, LIMITS, input_too_large, lang, limit_reason, title, too_large,
};

/// Largest image: 64 MiB, the same as the CBZ and PDF image limit, so an
/// image one action takes, the others take too. Past it that image fails.
pub(crate) const IMAGE_BYTES: u64 = 64 << 20;

/// Most pixels of one image: 180 million, the PDF join's limit
/// (`docs/resource-limits.md`; Pillow rejects above 179 MP). The header
/// check refuses more before anything else is read.
pub(crate) const MAX_PIXELS: u64 = 180_000_000;

/// The `fail` reason of a book whose documents together pass the one
/// document budget they share. Every document of the book is parsed and
/// held until the book is packed (the packer needs them all to resolve
/// links), and the memory a parsed document holds follows its nodes and
/// owned text, not its input bytes (a dense list holds ~180 times its
/// input). So the documents of one book share one document's node and
/// owned text budget (`Limits::DEFAULT`): the largest book holds what the
/// largest accepted single document holds (`docs/resource-limits.md`).
const BOOK_TOO_LARGE: &str = "book is too large (documents exceed the document budget together)";

/// What an input turned out to be.
enum Class {
    /// Markdown, or anything that is not an image: read and parsed in turn.
    Document,
    /// A valid image (a second path to a file already listed is one too:
    /// taken, but packaged once).
    Image,
    /// Already reported (`skip` or `fail`).
    Settled,
}

/// An image input, checked.
struct Item {
    path: PathBuf,
    /// The file's identity when it was checked.
    dev: u64,
    ino: u64,
    image: Image,
    /// Digest of its bytes, for the book's identifier.
    digest: u128,
    /// Its modification time, seconds since the epoch (before it: 0).
    modified: i64,
}

/// The images of the book, matched to references by file identity.
struct Pictures {
    items: Vec<Item>,
    by_id: HashMap<(u64, u64), usize>,
    /// The folder of each part's document, the base of its references.
    dirs: Vec<PathBuf>,
}

fn open_nonblocking(path: &Path) -> io::Result<File> {
    // `O_NONBLOCK`: a FIFO never waits for a writer; the type is checked
    // on the descriptor right after.
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

/// Whether `first`, the destination's first path segment, starts with a
/// URL scheme: `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"` (RFC 3986,
/// section 3.1), at least two characters before the colon so a Windows
/// drive letter (`C:`) is not one. Any other colon is part of a file name
/// (`a:b.png`).
fn has_scheme(first: &str) -> bool {
    let Some((scheme, _)) = first.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    scheme.len() >= 2
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// The absolute path an image destination names from the folder `dir` of
/// its document: percent-decoded, normalized lexically (`.` and `..` only,
/// no symlink is looked at). An absolute path or a URL (a scheme before
/// the first `/`) is not a local image: `None`.
fn reference_path(dir: &Path, dest: &str) -> Option<PathBuf> {
    let first = dest.split('/').next().unwrap_or_default();
    if dest.is_empty() || first.is_empty() || has_scheme(first) {
        return None;
    }
    let decoded = doc::xhtml::percent_decode(dest);
    if decoded.starts_with('/') || decoded.contains('\0') {
        return None;
    }
    let mut out = PathBuf::new();
    for c in dir.join(decoded).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    Some(out)
}

impl ImageSource for Pictures {
    fn resolve(&mut self, part: usize, dest: &str) -> Option<usize> {
        let path = reference_path(self.dirs.get(part)?, dest)?;
        // The file's identity decides, never its name: another path to an
        // input (a symlink, `./a.png`) is that input. Only `stat` (symlinks
        // followed): a file that is not an input is never opened, so a
        // device or FIFO named by a reference sees nothing. A failing stat
        // is not an input. `open` re-checks the identity on the descriptor
        // of the checked input itself.
        let m = fs::metadata(&path).ok()?;
        if !m.is_file() {
            return None;
        }
        self.by_id.get(&(m.dev(), m.ino())).copied()
    }

    fn count(&self) -> usize {
        self.items.len()
    }

    fn image(&self, i: usize) -> &Image {
        &self.items[i].image
    }

    fn open(&mut self, i: usize) -> io::Result<Box<dyn Read + '_>> {
        let item = &self.items[i];
        // Reopened by path, but only the checked file is accepted: a
        // replaced file has another identity or size.
        let f = open_nonblocking(&item.path)?;
        let m = f.metadata()?;
        if !m.is_file() || (m.dev(), m.ino(), m.len()) != (item.dev, item.ino, item.image.len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "image changed while the book was written",
            ));
        }
        Ok(Box::new(f))
    }
}

/// The `fail` reason of a header check that stopped an image.
fn image_reason(e: ProbeError) -> String {
    match e {
        ProbeError::TooLarge => {
            format!("image is larger than {} megapixels", MAX_PIXELS / 1_000_000)
        }
        ProbeError::TooManyChunks => {
            format!("PNG has more than {} data chunks", probe::MAX_IDAT_CHUNKS)
        }
        // Truncated, damaged, or a variant the header check does not give.
        _ => "damaged image data".to_string(),
    }
}

/// Checks `path` as an image: `Ok(None)` when it is not one (a document),
/// `Err(reason)` when it is one and unusable.
fn check_image(path: &Path) -> Result<Option<Item>, String> {
    let Ok(mut f) = open_nonblocking(path) else {
        return Ok(None);
    };
    let Ok(m) = f.metadata() else {
        return Ok(None);
    };
    if !m.is_file() {
        return Ok(None);
    }
    // Bounded: the header walk never reads past the image limit.
    let sniffed = match probe::sniff(&mut (&f).take(IMAGE_BYTES), MAX_PIXELS) {
        Ok(s) => s,
        Err(ProbeError::NotImage) => return Ok(None),
        Err(e) => return Err(image_reason(e)),
    };
    if m.len() > IMAGE_BYTES {
        return Err(format!("image is larger than {} MiB", IMAGE_BYTES >> 20));
    }
    f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let digest = epub::fnv1a_128_read((&f).take(m.len())).map_err(|e| e.to_string())?;
    let kind = match sniffed.format {
        Format::Jpeg => ImageKind::Jpeg,
        Format::Png => ImageKind::Png,
    };
    let alt = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let item = Item {
        path: path.to_path_buf(),
        dev: m.dev(),
        ino: m.ino(),
        image: Image {
            kind,
            len: m.len(),
            alt,
        },
        digest,
        modified: m.mtime().max(0),
    };
    Ok(Some(item))
}

/// A document that was read.
struct Parsed {
    path: PathBuf,
    doc: Document,
    len: usize,
    digest: u128,
}

/// Removes the half-written book on every way out but a finished join.
struct Partial {
    path: PathBuf,
    armed: bool,
}

impl Drop for Partial {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// The folder every path of `docs` is under, the deepest one, and each
/// document's path from it with `/` as separator.
fn keys(docs: &[Parsed]) -> Vec<String> {
    let mut common: Vec<Component> = docs[0]
        .path
        .parent()
        .map(|p| p.components().collect())
        .unwrap_or_default();
    for d in &docs[1..] {
        let parent: Vec<Component> = d
            .path
            .parent()
            .map(|p| p.components().collect())
            .unwrap_or_default();
        let n = common
            .iter()
            .zip(&parent)
            .take_while(|(a, b)| a == b)
            .count();
        common.truncate(n);
    }
    docs.iter()
        .map(|d| {
            let rest: Vec<String> = d
                .path
                .components()
                .skip(common.len())
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            rest.join("/")
        })
        .collect()
}

/// Seconds since the epoch of the newest input the book takes, documents
/// and the images it packages (an unreadable time, or one before the
/// epoch, counts as the epoch), so the book is reproducible from its
/// inputs.
fn modified_unix(docs: &[Parsed], pics: &Pictures) -> i64 {
    let newest_doc = docs
        .iter()
        .filter_map(|d| fs::metadata(&d.path).and_then(|m| m.modified()).ok())
        .filter_map(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .max()
        .unwrap_or(0);
    pics.items
        .iter()
        .map(|i| i.modified)
        .fold(newest_doc, i64::max)
}

/// What became of one document.
enum Doc {
    Parsed(Document, usize, u128),
    /// Its own `skip` was reported.
    Skipped,
    /// Its own `fail` was reported.
    Failed,
    /// It does not fit what the documents before it left of the budget
    /// they share: the book fails (nothing reported yet).
    BookTooLarge,
}

/// Reads and parses one document under `left`, what the documents before
/// it left of the shared budget, reporting its own `skip` or `fail`.
fn read_document(path: &Path, left: &mut doc::Limits, ev: &mut Events<impl Write>) -> Doc {
    // Nothing spent yet: the document has the whole budget, and passing it
    // is its own failure. After another one, passing what is left is the
    // book's.
    let first = *left == LIMITS;
    // Checked before the read: with no node left a document with content
    // cannot fit (an empty file still can; one of only blank lines fails
    // closed).
    if !first && left.nodes == 0 && fs::metadata(path).is_ok_and(|m| m.len() > 0) {
        return Doc::BookTooLarge;
    }
    let bytes = match action::read_file_bounded(path, LIMITS.input_bytes) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            ev.fail(path, &input_too_large());
            return Doc::Failed;
        }
        Err(e) => {
            ev.fail(path, &e.to_string());
            return Doc::Failed;
        }
    };
    match doc::md::read_spending(&bytes, left, Dialect::Gfm) {
        Ok(d) => Doc::Parsed(d, bytes.len(), epub::fnv1a_128(&[&bytes])),
        Err(ReadError::NotText) => {
            ev.skip(path, "unsupported");
            Doc::Skipped
        }
        Err(ReadError::Limit(LimitError::Exceeded("nodes" | "owned text"))) if !first => {
            Doc::BookTooLarge
        }
        Err(ReadError::Limit(l)) => {
            ev.fail(path, &limit_reason(l));
            Doc::Failed
        }
    }
}

/// The book's identifier seed: each input's kind, length and digest in
/// the sorted order, fixed-width records, so no two inputs run together.
fn id_seed(docs: &[Parsed], pics: &Pictures) -> Vec<u8> {
    let mut seed = Vec::with_capacity((docs.len() + pics.items.len()) * 25);
    let mut record = |tag: u8, len: u64, digest: u128| {
        seed.push(tag);
        seed.extend_from_slice(&len.to_be_bytes());
        seed.extend_from_slice(&digest.to_be_bytes());
    };
    for d in docs {
        record(b'd', d.len as u64, d.digest);
    }
    for i in &pics.items {
        record(b'i', i.image.len, i.digest);
    }
    seed
}

/// `text.md-join-epub`.
pub fn run(inv: &Invocation) -> u8 {
    if !inv.work.is_dir() {
        eprintln!("work directory does not exist: {}", inv.work.display());
        return action::EXIT_FAILED as u8;
    }
    let mut ev = Events::new(io::stdout().lock());
    let n = inv.inputs.len();
    ev.progress(0.0);
    let mut any_fail = false;

    // Pass 1: which input is an image, and is it usable. The documents
    // are told apart by not being images, so the result's name can be
    // worked out from all of them before any is read.
    let mut pics = Pictures {
        items: Vec::new(),
        by_id: HashMap::new(),
        dirs: Vec::new(),
    };
    let mut classes: Vec<Class> = Vec::with_capacity(n);
    for input in &inv.inputs {
        if input.to_str().is_none() {
            // Settled unread, as in every other action.
            ev.skip(input, "non-UTF-8 path");
            classes.push(Class::Settled);
            continue;
        }
        let class = match check_image(input) {
            Ok(None) => Class::Document,
            Err(reason) => {
                ev.fail(input, &reason);
                any_fail = true;
                Class::Settled
            }
            Ok(Some(item)) => match pics.by_id.get(&(item.dev, item.ino)) {
                Some(_) => Class::Image,
                None => {
                    pics.by_id.insert((item.dev, item.ino), pics.items.len());
                    pics.items.push(item);
                    Class::Image
                }
            },
        };
        classes.push(class);
    }
    let given: Vec<&Path> = inv
        .inputs
        .iter()
        .zip(&classes)
        .filter(|(_, c)| matches!(c, Class::Document))
        .map(|(p, _)| p.as_path())
        .collect();

    // Pass 2: the documents, in order, under one shared budget.
    let mut docs: Vec<Parsed> = Vec::new();
    let mut left = LIMITS;
    for (i, (input, class)) in inv.inputs.iter().zip(&classes).enumerate() {
        if matches!(class, Class::Document) {
            match read_document(input, &mut left, &mut ev) {
                Doc::Parsed(doc, len, digest) => docs.push(Parsed {
                    path: input.clone(),
                    doc,
                    len,
                    digest,
                }),
                Doc::Skipped => {}
                Doc::Failed => any_fail = true,
                Doc::BookTooLarge => {
                    // The whole book fails: every input it would have
                    // taken (the documents read, this one, the ones not
                    // yet read, the images); no file was made.
                    for (j, (input, class)) in inv.inputs.iter().zip(&classes).enumerate() {
                        let would_take = match class {
                            Class::Image => true,
                            Class::Document => j >= i || docs.iter().any(|d| d.path == *input),
                            Class::Settled => false,
                        };
                        if would_take {
                            ev.fail(input, BOOK_TOO_LARGE);
                        }
                    }
                    return action::EXIT_FAILED as u8;
                }
            }
        }
        ev.progress((i + 1) as f32 / n as f32);
    }

    if docs.is_empty() {
        // No document, no book (the images have nothing to join).
        for (input, class) in inv.inputs.iter().zip(&classes) {
            if matches!(class, Class::Image) {
                ev.skip(input, "no document to join");
            }
        }
        return nothing_taken(any_fail);
    }

    // The book. Its title: the result's name (the folder with the most
    // documents, from the same rule the engine places the result by) for
    // several documents, the one document's own title otherwise.
    let name = if given.len() >= 2 {
        action::anchor_of(&given, action::BOOK_FALLBACK_NAME).map(|(_, name)| name)
    } else {
        None
    };
    let keys = keys(&docs);
    pics.dirs = docs
        .iter()
        .map(|d| d.path.parent().map(Path::to_path_buf).unwrap_or_default())
        .collect();
    let parts: Vec<Part> = docs
        .iter()
        .zip(keys)
        .map(|(d, file_key)| Part {
            doc: &d.doc,
            title: title(Conversion::MdToEpub, &d.doc, &d.path),
            file_key,
        })
        .collect();
    let meta = epub::Meta {
        title: name.unwrap_or_else(|| parts[0].title.clone()),
        lang: lang(),
        modified_unix: modified_unix(&docs, &pics),
        id_seed: id_seed(&docs, &pics),
    };

    let taken: Vec<&Path> = inv
        .inputs
        .iter()
        .zip(&classes)
        .filter(|(p, c)| match c {
            Class::Image => true,
            Class::Document => docs.iter().any(|d| d.path == **p),
            Class::Settled => false,
        })
        .map(|(p, _)| p.as_path())
        .collect();
    let out = action::join_path(&inv.work, "epub");
    match pack(&out, &parts, &mut pics, &meta) {
        Ok(losses) => {
            let first = docs[0].path.as_path();
            for loss in losses {
                ev.warn(first, loss.message());
            }
            for input in &taken {
                ev.take(input);
            }
            ev.joined(out.strip_prefix(&inv.work).unwrap_or(&out));
            action::EXIT_OK as u8
        }
        Err(e) => {
            let reason = match e {
                ConvertError::Limit(l) => limit_reason(l),
                ConvertError::Other(r) => r,
                ConvertError::NotText => "not text".to_string(),
            };
            for input in &taken {
                ev.fail(input, &reason);
            }
            action::EXIT_FAILED as u8
        }
    }
}

/// The exit of a join that took nothing: 3 when every input was a `skip`,
/// else 1.
fn nothing_taken(any_fail: bool) -> u8 {
    if any_fail {
        action::EXIT_FAILED as u8
    } else {
        action::EXIT_UNSUPPORTED as u8
    }
}

/// Writes the book to `out` (a new file in a new `join` folder). Whatever
/// goes wrong, no file stays.
fn pack(
    out: &Path,
    parts: &[Part],
    pics: &mut Pictures,
    meta: &epub::Meta,
) -> Result<std::collections::BTreeSet<doc::Loss>, ConvertError> {
    let other = |what: &str, e: io::Error| ConvertError::Other(format!("{what}: {e}"));
    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir).map_err(|e| other("cannot create output", e))?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)
        .map_err(|e| other("cannot create output", e))?;
    let mut partial = Partial {
        path: out.to_path_buf(),
        armed: true,
    };
    // SIGXFSZ would kill the process at the first byte past
    // `RLIMIT_FSIZE`; ignored, that write fails with EFBIG and the book
    // fails with the size copy. The size of a book is known only once it
    // is packed, so this is the ceiling's one check; the file is cut back
    // to empty by `epub::write_book` and removed by `partial`.
    // SAFETY: `signal` with SIG_IGN installs no handler code and touches
    // no Rust state; SIGXFSZ is a valid signal number.
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
    }
    let losses = epub::write_book(parts, pics, meta, file, LIMITS).map_err(|e| {
        if e.write_error().and_then(io::Error::raw_os_error) == Some(libc::EFBIG) {
            too_large("EPUB")
        } else {
            e.into()
        }
    })?;
    partial.armed = false;
    Ok(losses)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_is_rfc_3986_and_two_characters() {
        for s in ["http:", "https://x", "mailto:a@b", "a+b-c.d:x", "ab:c.png"] {
            assert!(has_scheme(s), "{s}");
        }
        for s in [
            "a:b.png",
            "C:",
            "1http:x",
            "ht_tp:x",
            ":x",
            "plain.png",
            "-ab:x",
        ] {
            assert!(!has_scheme(s), "{s}");
        }
    }

    /// A unique, empty scratch folder, removed by the caller.
    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("inacircle-join-unit-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn png_file(path: &Path) {
        // 2x2 RGB, one shade: a valid image of a few dozen bytes.
        let rows: Vec<u8> = [vec![0u8], vec![9u8; 6], vec![0u8], vec![9u8; 6]].concat();
        fs::write(path, testzip::png(2, 2, 2, None, false, false, &rows)).unwrap();
    }

    fn fcntl(f: &File, cmd: libc::c_int) -> libc::c_int {
        use std::os::fd::AsRawFd;
        // SAFETY: a valid open descriptor and a command that takes no
        // argument.
        let r = unsafe { libc::fcntl(f.as_raw_fd(), cmd) };
        assert!(r >= 0, "{}", io::Error::last_os_error());
        r
    }

    /// Both flags are set: `O_NONBLOCK` (a FIFO never waits for a writer)
    /// and `O_CLOEXEC` (no child inherits the image).
    #[test]
    fn open_is_nonblocking_and_close_on_exec() {
        let d = scratch("flags");
        let p = d.join("a.png");
        png_file(&p);
        let f = open_nonblocking(&p).unwrap();
        assert_ne!(fcntl(&f, libc::F_GETFL) & libc::O_NONBLOCK, 0);
        assert_ne!(fcntl(&f, libc::F_GETFD) & libc::FD_CLOEXEC, 0);
        let _ = fs::remove_dir_all(&d);
    }

    /// Each header-check failure has its own reason; the rest is damage.
    #[test]
    fn image_reasons() {
        assert_eq!(
            image_reason(ProbeError::TooLarge),
            "image is larger than 180 megapixels"
        );
        assert_eq!(
            image_reason(ProbeError::TooManyChunks),
            format!("PNG has more than {} data chunks", probe::MAX_IDAT_CHUNKS)
        );
        for e in [
            ProbeError::Truncated,
            ProbeError::Damaged,
            ProbeError::NotImage,
        ] {
            assert_eq!(image_reason(e), "damaged image data");
        }
    }

    /// The image limit is inclusive: a file of exactly `IMAGE_BYTES` is an
    /// image, one byte more fails with the size reason; a header past the
    /// pixel limit fails with the pixel reason.
    #[test]
    fn image_size_limit_is_inclusive() {
        let d = scratch("limit");
        let p = d.join("big.png");
        png_file(&p);
        // Bytes after the IEND chunk: the header check reads none of them.
        let f = OpenOptions::new().write(true).open(&p).unwrap();
        f.set_len(IMAGE_BYTES).unwrap();
        drop(f);
        let item = check_image(&p).unwrap().expect("an image");
        assert_eq!(item.image.len, IMAGE_BYTES);
        let f = OpenOptions::new().write(true).open(&p).unwrap();
        f.set_len(IMAGE_BYTES + 1).unwrap();
        drop(f);
        assert_eq!(
            check_image(&p).err().as_deref(),
            Some("image is larger than 64 MiB")
        );

        // 20,000 x 20,000 = 400 MP: the header alone is past the limit.
        let wide = d.join("wide.png");
        fs::write(
            &wide,
            testzip::png(20_000, 20_000, 2, None, false, false, &[]),
        )
        .unwrap();
        assert_eq!(
            check_image(&wide).err().as_deref(),
            Some("image is larger than 180 megapixels")
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// `open` gives back only the file that was checked: another file at
    /// the path (another identity) and a rewritten one (another size) are
    /// refused, the unchanged one is read.
    #[test]
    fn open_refuses_a_changed_file() {
        let d = scratch("open");
        let p = d.join("a.png");
        png_file(&p);
        let item = check_image(&p).unwrap().expect("an image");
        let mut pics = Pictures {
            items: vec![item],
            by_id: HashMap::new(),
            dirs: Vec::new(),
        };
        let mut bytes = Vec::new();
        pics.open(0).unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, fs::read(&p).unwrap());

        // Same inode, other size.
        let mut longer = fs::read(&p).unwrap();
        longer.push(0);
        fs::write(&p, &longer).unwrap();
        assert_eq!(
            pics.open(0).err().map(|e| e.kind()),
            Some(io::ErrorKind::InvalidData)
        );

        // Another file renamed over it, of the original size.
        let item = check_image(&p).unwrap().expect("an image");
        pics.items[0] = item;
        let other = d.join("other.png");
        fs::write(&other, &longer).unwrap();
        fs::rename(&other, &p).unwrap();
        assert_eq!(
            pics.open(0).err().map(|e| e.kind()),
            Some(io::ErrorKind::InvalidData)
        );
        let _ = fs::remove_dir_all(&d);
    }

    fn parsed(path: &str) -> Parsed {
        let mut left = LIMITS;
        Parsed {
            path: PathBuf::from(path),
            doc: doc::md::read_spending(b"# x\n", &mut left, Dialect::Gfm).unwrap(),
            len: 4,
            digest: 0,
        }
    }

    /// The keys are the paths below the deepest folder all documents are
    /// under.
    #[test]
    fn keys_are_relative_to_the_common_folder() {
        let docs = [
            parsed("/r/x/a/1.md"),
            parsed("/r/x/b/2.md"),
            parsed("/r/x/3.md"),
        ];
        assert_eq!(keys(&docs), ["a/1.md", "b/2.md", "3.md"]);
        let docs = [parsed("/r/x/a.md"), parsed("/r/x/b.md")];
        assert_eq!(keys(&docs), ["a.md", "b.md"]);
        let docs = [parsed("/r/x/a/1.md"), parsed("/r/y/2.md")];
        assert_eq!(keys(&docs), ["x/a/1.md", "y/2.md"]);
        assert_eq!(keys(&[parsed("/r/x/a.md")]), ["a.md"]);
    }
}
