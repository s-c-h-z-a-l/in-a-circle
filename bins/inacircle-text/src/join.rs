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
use doc::{Dialect, Document, ReadError};
use epub::{Image, ImageKind, ImageSource, Part};
use probe::{Format, ProbeError};

use crate::{
    Conversion, ConvertError, LIMITS, input_too_large, lang, limit_reason, title, too_large,
};

/// Largest image: 64 MiB, the same as the CBZ and PDF image limit, so an
/// image one action takes, the others take too. Past it that image fails.
const IMAGE_BYTES: u64 = 64 << 20;

/// Most pixels of one image: 180 million, the PDF join's limit
/// (`docs/resource-limits.md`; Pillow rejects above 179 MP). The header
/// check refuses more before anything else is read.
const MAX_PIXELS: u64 = 180_000_000;

/// Most Markdown bytes held at once, summed over the documents: 16 MiB.
/// Every document of the book is parsed and held until the book is packed
/// (the packer needs them all to resolve links), and a parsed document
/// takes 12 to 25 times its input (`Limits::DEFAULT`), so the sum is what
/// the memory follows: 16 MiB is twice one document's budget, about 200 to
/// 400 MB parsed at the worst, under the sandbox's `RLIMIT_DATA` (>= 1 GiB)
/// and 25 times the largest real text measured. The document that would
/// pass it fails (`fail`) and the rest still join.
const DOCUMENTS_BYTES: usize = 16 << 20;

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

/// The absolute path an image destination names from the folder `dir` of
/// its document: percent-decoded, normalized lexically (`.` and `..` only,
/// no symlink is looked at). An absolute path or a URL (a scheme before
/// the first `/`) is not a local image: `None`.
fn reference_path(dir: &Path, dest: &str) -> Option<PathBuf> {
    let first = dest.split('/').next().unwrap_or_default();
    if dest.is_empty() || first.is_empty() || first.contains(':') {
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
        // The opened file's identity decides, never its name: another
        // path to an input (a symlink, `./a.png`) is that input; a file
        // that is not an input is not read.
        let m = open_nonblocking(&path).ok()?.metadata().ok()?;
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

/// Seconds since the epoch of the newest document (an unreadable time, or
/// one before the epoch, counts as the epoch), so the book is reproducible
/// from its inputs.
fn modified_unix(docs: &[Parsed]) -> i64 {
    docs.iter()
        .filter_map(|d| fs::metadata(&d.path).and_then(|m| m.modified()).ok())
        .filter_map(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .max()
        .unwrap_or(0)
}

/// What became of one document.
enum Doc {
    Parsed(Document, usize, u128),
    /// Its own `skip` was reported.
    Skipped,
    /// Its own `fail` was reported.
    Failed,
}

/// Reads and parses one document, reporting its own `skip` or `fail`.
fn read_document(path: &Path, held: &mut usize, ev: &mut Events<impl Write>) -> Doc {
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
    if held.saturating_add(bytes.len()) > DOCUMENTS_BYTES {
        ev.fail(
            path,
            &format!(
                "The Markdown files together are larger than {} MiB.",
                DOCUMENTS_BYTES >> 20
            ),
        );
        return Doc::Failed;
    }
    match doc::md::read_limited_with(&bytes, LIMITS, Dialect::Gfm) {
        Ok(d) => {
            *held += bytes.len();
            Doc::Parsed(d, bytes.len(), epub::fnv1a_128(&[&bytes]))
        }
        Err(ReadError::NotText) => {
            ev.skip(path, "unsupported");
            Doc::Skipped
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
        let class = match check_image(input) {
            Ok(None) => Class::Document,
            Err(reason) => {
                ev.fail(input, &reason);
                any_fail = true;
                Class::Settled
            }
            Ok(Some(_)) if input.to_str().is_none() => {
                ev.skip(input, "non-UTF-8 path");
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

    // Pass 2: the documents, in order.
    let mut docs: Vec<Parsed> = Vec::new();
    let mut held = 0;
    for (i, (input, class)) in inv.inputs.iter().zip(&classes).enumerate() {
        if matches!(class, Class::Document) {
            if input.to_str().is_none() {
                ev.skip(input, "non-UTF-8 path");
            } else {
                match read_document(input, &mut held, &mut ev) {
                    Doc::Parsed(doc, len, digest) => docs.push(Parsed {
                        path: input.clone(),
                        doc,
                        len,
                        digest,
                    }),
                    Doc::Skipped => {}
                    Doc::Failed => any_fail = true,
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
        modified_unix: modified_unix(&docs),
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
