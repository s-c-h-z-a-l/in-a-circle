//! `inacircle-pages`: the pages family. `pages.cbz-to-pdf` turns a comic
//! book archive (CBZ) into one PDF: one page per image, in natural name
//! order, JPEG and PNG data passed through unchanged (lossless). Contract
//! v0 (docs/architecture.md). One CBZ gives one PDF, all or nothing: any
//! page that cannot be taken makes the whole input `skip` or `fail` and the
//! half-written PDF is removed. `pages.images-to-pdf` is a join action: any
//! number of JPEG and PNG files become one PDF, one page per file in the
//! given order; a file that cannot be taken is left out (`skip` or `fail`)
//! and the rest is joined. `pages.images-to-cbz` is the other join: the
//! same files become one CBZ, each image stored byte for byte under a
//! numbered name (`001.jpg`, `002.png`, ...), in the given order.

use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use action::{Command, Events, Invocation, natural_cmp, parse_args};
use probe::{Image, ProbeError, Unsupported};

// ---------------------------------------------------------------------
// Limits (docs/resource-limits.md, "Pages"). Each is checked on what is
// walked or produced, before the memory is allocated, and past it the
// input fails with its own copy (`fail`, nothing staged); the batch goes on.
// The sandbox's RLIMIT_DATA (>= 1 GiB) and RLIMIT_FSIZE are the second net.
// ---------------------------------------------------------------------

/// Most central-directory records: 12,000. The longest real comic
/// collections run to a few thousand pages plus a folder entry per chapter
/// and a metadata file; 12,000 is twice the 5,000-page case the memory test
/// measures with room for folders. Cost: the entry list is about 200 B per
/// record (struct, name, sort index), 2.4 MB at the limit. Past it:
/// `CBZ has more than 12000 entries`.
const ENTRIES: usize = 12_000;

/// Largest declared size of one page file (not its decoded pixels, the
/// data is passed through): 64 MiB. The page is the one thing held whole
/// (`docs/resource-limits.md`: one page plus the entry list). A 180
/// megapixel scan is around 20-40 MB as JPEG, and a flat 180 MP PNG far
/// less; 64 MiB covers them with room, and with the entry list stays under
/// 80 MB (about 77 MB worst case). The fuzz target `inflate` runs with a
/// small limit for speed; the 64 MiB boundary is covered by unit tests.
/// Past it: `page <name> is larger than 64 MiB`. It is also the inflate output
/// limit, so a lying size cannot produce more.
const ENTRY_BYTES: u64 = 64 << 20;

/// What the ZIP reader gets as its per-entry size limit: the largest size a
/// non-ZIP64 header can declare (u32::MAX). The real page limit,
/// `ENTRY_BYTES`, is checked per page here so the message names the page
/// (the reader's error does not), before anything is read. Entries that
/// are not pages are never read, so their size is not our concern.
const ZIP_ENTRY_BYTES: u64 = u32::MAX as u64;

/// Largest `size / compressed_size` of an entry: 1032, the most a deflate
/// stream can reach (a 258-byte match costs at least two bits). Every valid
/// stream passes, including a blank page stored as an uncompressed PNG
/// (about 1000:1); a header claiming more is lying or the stream is
/// damaged, and the reader refuses it before inflating. What bounds a bomb
/// is not this ratio but `ENTRY_BYTES`: nothing past 64 MiB is produced
/// per page. Past it: `damaged CBZ: an entry's compression ratio is too
/// high`.
const RATIO: u64 = 1032;

/// Longest entry name: 512 bytes, twice the 255-byte file name limit of
/// common file systems, which fits `Series/Volume/Chapter/page` paths with
/// room. Names are held for every entry: 12,000 x 512 B = 6 MB at the
/// worst. Past it: `damaged CBZ: an entry name is too long`.
const NAME_BYTES: usize = 512;

/// Most pages: 10,000 (below `ENTRIES`, so the page copy is reachable). An
/// omnibus of a long-running series is a few thousand pages. Cost per page:
/// the page order entry and the PDF writer's page object number and
/// offset, about 100 B, 1 MB at the limit. Past it:
/// `CBZ has more than 10000 pages`.
const PAGES: usize = 10_000;

/// Most pixels of one page: 180 million, `docs/resource-limits.md` (Pillow
/// rejects above 179 MP). Checked by the probe on the image header, before
/// the PNG data is walked. Past it: `page <name> is larger than 180
/// megapixels`.
const MAX_PIXELS: u64 = 180_000_000;

/// Bytes of PDF structure around the page data, per page: 2,560. Derived
/// from `crates/pdf`: a page is three objects. The image object has
/// `N 0 obj\n`, a dictionary of about 300 B (a palette PNG adds its
/// `/Indexed` table, at most 256 entries as 1,536 hex digits plus about
/// 30 B), `/Length`, `stream\n` and `\nendstream\nendobj\n`, together about
/// 1,900 B at the worst. The content stream (`q w 0 0 h 0 0 cm /Im0 Do Q`
/// in its object) is about 100 B and the page object (MediaBox, Resources,
/// Parent, Contents) about 200 B. The cross-reference table takes 20 B per
/// object, 60 B per page, and the Kids list `N 0 R ` at most 11 B per page.
/// The sum, about 2,270 B, is rounded up to 2,560 (2.5 KiB). Past the
/// estimate the total is still checked against the bytes actually written.
const PAGE_OVERHEAD: u64 = 2_560;

/// Bytes of PDF structure once per file: 1,024. The header (15 B), the
/// Pages node and Catalog (about 100 B plus the Kids list, counted per
/// page), the `xref` heading with its free entry (about 50 B) and the
/// trailer (about 80 B) come to about 250 B; 1,024 is generous.
const FILE_OVERHEAD: u64 = 1_024;

/// The soft `RLIMIT_FSIZE` of this process, the largest file it may write
/// (the engine sets it to 16 x the input, 256 MiB to 16 GiB). `None` is
/// unlimited. A failing `getrlimit` is an error, so the caller fails
/// closed instead of writing without a ceiling. `getrlimit` on itself is
/// allowed by the sandbox's seccomp policy.
fn output_cap() -> io::Result<Option<u64>> {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rl` is a valid, writable `rlimit` for the whole call, and
    // RLIMIT_FSIZE is a valid resource; the call only writes into `rl`.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut rl) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((rl.rlim_cur != libc::RLIM_INFINITY).then_some(rl.rlim_cur))
}

/// A writer that refuses to pass `cap` bytes: the write that would cross
/// it fails first and sets `tripped`, so the caller can tell the ceiling
/// from an I/O error.
struct Capped<W: Write> {
    inner: W,
    cap: Option<u64>,
    written: u64,
    tripped: Rc<Cell<bool>>,
}

impl<W: Write> Write for Capped<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let next = self.written.saturating_add(b.len() as u64);
        if self.cap.is_some_and(|c| next > c) {
            self.tripped.set(true);
            return Err(io::Error::other("output size limit"));
        }
        let n = self.inner.write(b)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn manifest_text() -> String {
    let mut s = manifest::header("inacircle-manifest", 0);
    s.push_str(&manifest::write_line(
        &manifest::Line::new("action")
            .bare("pages.cbz-to-pdf")
            .pair("family", "pages")
            .pair("in", "application/vnd.comicbook+zip")
            .pair("out", "application/pdf")
            .pair("ext", "pdf")
            .pair("label", "PDF")
            .pair("group", "book")
            .pair("cost", "10")
            .pair("lossy", "yes")
            .pair("batch", "yes"),
    ));
    s.push_str(&manifest::write_line(
        &manifest::Line::new("action")
            .bare("pages.images-to-pdf")
            .pair("family", "pages")
            .pair("in", "image/jpeg,image/png")
            .pair("out", "application/pdf")
            .pair("ext", "pdf")
            .pair("label", "PDF")
            .pair("group", "book")
            .pair("cost", "10")
            .pair("lossy", "yes")
            .pair("batch", "yes")
            .pair("join", "yes")
            .pair("min", "1"),
    ));
    s.push_str(&manifest::write_line(
        &manifest::Line::new("action")
            .bare("pages.images-to-cbz")
            .pair("family", "pages")
            .pair("in", "image/jpeg,image/png")
            .pair("out", "application/vnd.comicbook+zip")
            .pair("ext", "cbz")
            .pair("label", "CBZ")
            .pair("group", "book")
            .pair("cost", "10")
            .pair("lossy", "no")
            .pair("batch", "yes")
            .pair("join", "yes")
            .pair("min", "1"),
    ));
    s
}

/// Why an input stopped.
enum Stop {
    /// Not for this action (`skip`).
    Skip(String),
    /// Broken or over a limit (`fail`).
    Fail(String),
}

fn skip<T>(reason: impl Into<String>) -> Result<T, Stop> {
    Err(Stop::Skip(reason.into()))
}

fn fail<T>(reason: impl Into<String>) -> Result<T, Stop> {
    Err(Stop::Fail(reason.into()))
}

enum Outcome {
    Done,
    Failed,
    Unsupported,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Jpeg,
    Png,
}

/// The image kind the entry name claims, from its extension (any case).
fn kind_of(name: &[u8]) -> Option<Kind> {
    let ext = name
        .rsplit(|&c| c == b'.')
        .next()
        .filter(|e| e.len() < name.len())?;
    if ext.eq_ignore_ascii_case(b"jpg") || ext.eq_ignore_ascii_case(b"jpeg") {
        Some(Kind::Jpeg)
    } else if ext.eq_ignore_ascii_case(b"png") {
        Some(Kind::Png)
    } else {
        None
    }
}

/// Metadata a Finder-made zip carries beside the pages: anything under
/// `__MACOSX/` and any file whose last path component starts with `.`
/// (`._1.jpg` resource forks, a bare `.png`). Not pages, even with an image
/// extension.
fn is_metadata(name: &[u8]) -> bool {
    let base = name.rsplit(|&c| c == b'/').next().unwrap_or(name);
    name.split(|&c| c == b'/').next() == Some(b"__MACOSX") || base.starts_with(b".")
}

/// The page name for messages: lossy where the bytes are not UTF-8.
fn lossy(name: &[u8]) -> String {
    String::from_utf8_lossy(name).into_owned()
}

/// The `skip` or `fail` of a probe error on the page `name`. With `None`
/// (a join input, whose path the event already names) the copy has no page
/// prefix.
fn probe_stop<T>(name: Option<&str>, e: ProbeError) -> Result<T, Stop> {
    let with = |rest: String| match name {
        Some(n) => format!("page {n}: {rest}"),
        None => rest,
    };
    match e {
        ProbeError::NotImage => fail(with("not a JPEG or PNG".into())),
        ProbeError::Truncated | ProbeError::Damaged => fail(with("damaged image data".into())),
        ProbeError::TooLarge => {
            let mp = MAX_PIXELS / 1_000_000;
            fail(match name {
                Some(n) => format!("page {n} is larger than {mp} megapixels"),
                None => format!("image is larger than {mp} megapixels"),
            })
        }
        ProbeError::TooManyChunks => fail(with(format!(
            "PNG has more than {} data chunks",
            probe::MAX_IDAT_CHUNKS
        ))),
        ProbeError::Unsupported(Unsupported::JpegComponents(n)) => {
            // The probe's text is number-free here; the count is ours.
            skip(with(format!("JPEG with {n} components is not supported")))
        }
        ProbeError::Unsupported(u) => skip(with(probe::unsupported_text(&u).into())),
    }
}

/// A removable output: dropped while armed, the file is removed, so every
/// way out of a conversion (error, early return, panic) leaves no PDF.
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

/// What a finished conversion reports once.
#[derive(Default)]
struct Notes {
    non_image: usize,
    orientation: usize,
    profile: usize,
}

/// Open the input as a regular file without ever waiting (a FIFO would
/// block a plain open until a writer shows up).
fn open_input(path: &Path) -> io::Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    if !f.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(f)
}

fn convert(
    inv: &Invocation,
    i: usize,
    input: &Path,
    n: usize,
    ev: &mut Events<impl Write>,
) -> Result<(PathBuf, Notes), Stop> {
    let file = match open_input(input) {
        Ok(f) => f,
        Err(e) => return fail(e.to_string()),
    };
    let limits = zip::Limits {
        entries: ENTRIES,
        entry_bytes: ZIP_ENTRY_BYTES,
        ratio: RATIO,
        name_bytes: NAME_BYTES,
    };
    let archive = match zip::open(&file, limits) {
        Ok(a) => a,
        Err(zip::ZipError::Zip64) => return skip("ZIP64 CBZ is not supported"),
        Err(zip::ZipError::TooManyEntries) => {
            return fail(format!("CBZ has more than {ENTRIES} entries"));
        }
        Err(e) => return fail(format!("damaged CBZ: {e}")),
    };

    // Pages in natural order; everything else that is not a folder is left
    // out and counted.
    let entries = archive.entries();
    let mut notes = Notes::default();
    let mut pages: Vec<usize> = Vec::new();
    for (idx, e) in entries.iter().enumerate() {
        if e.is_dir() {
            continue;
        }
        if !is_metadata(&e.name) && kind_of(&e.name).is_some() {
            pages.push(idx);
        } else {
            notes.non_image += 1;
        }
    }
    if pages.is_empty() {
        return skip("no pages");
    }
    if pages.len() > PAGES {
        return fail(format!("CBZ has more than {PAGES} pages"));
    }
    pages.sort_by(|&a, &b| natural_cmp(&entries[a].name, &entries[b].name));

    // Everything that can be known from the directory is checked before the
    // output exists or any page is read.
    for &idx in &pages {
        let e = &entries[idx];
        let name = lossy(&e.name);
        if e.encrypted {
            return skip("encrypted CBZ");
        }
        if let zip::Method::Other(m) = e.method {
            return skip(format!(
                "page {name}: compression method {m} is not supported"
            ));
        }
        if e.size > ENTRY_BYTES {
            return fail(format!(
                "page {name} is larger than {} MiB",
                ENTRY_BYTES >> 20
            ));
        }
    }

    // The output ceiling, before the output exists: the declared sizes are
    // true upper bounds on the page bytes (the reader enforces that the
    // bytes produced equal the declared size), so their sum plus the PDF
    // structure is the file size at the worst.
    let cap = match output_cap() {
        Ok(c) => c,
        Err(e) => return fail(format!("cannot read the output size limit: {e}")),
    };
    let too_large = |c: u64| Stop::Fail(format!("PDF would be larger than {} MiB", c >> 20));
    if let Some(c) = cap {
        let total = pages.iter().fold(FILE_OVERHEAD, |t, &idx| {
            t.saturating_add(entries[idx].size)
                .saturating_add(PAGE_OVERHEAD)
        });
        if total > c {
            return Err(too_large(c));
        }
    }

    let out_path = match action::output_path(&inv.work, i, input, "pdf") {
        Ok(p) => p,
        Err(e) => return fail(format!("cannot create output: {e}")),
    };
    let out_file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&out_path)
    {
        Ok(f) => f,
        Err(e) => return fail(format!("cannot create output: {e}")),
    };
    let mut partial = Partial {
        path: out_path,
        armed: true,
    };
    let tripped = Rc::new(Cell::new(false));
    let write_err = |e: io::Error| match cap {
        Some(c) if tripped.get() => too_large(c),
        _ => Stop::Fail(format!("cannot write output: {e}")),
    };
    let capped = Capped {
        inner: out_file,
        cap,
        written: 0,
        tripped: Rc::clone(&tripped),
    };
    let mut writer = pdf::Writer::new(BufWriter::new(capped)).map_err(write_err)?;

    // One page at a time: the buffer is reused, so memory is the largest
    // page, not the sum.
    let mut buf: Vec<u8> = Vec::new();
    for (k, &idx) in pages.iter().enumerate() {
        let e = &entries[idx];
        let name = lossy(&e.name);
        buf.clear();
        if let Err(err) = archive.read(idx, &mut buf) {
            return fail(format!("damaged CBZ: {err}"));
        }
        // The signature must agree with the extension before the probe
        // runs, so a PNG named .jpg is a mismatch, not a "damaged" JPEG.
        let sig = if buf.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
            Some(Kind::Png)
        } else if buf.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Kind::Jpeg)
        } else {
            None
        };
        if sig.is_some() && sig != kind_of(&e.name) {
            return fail(format!("page {name}: content does not match its extension"));
        }
        let probed = match probe::probe(&buf, MAX_PIXELS) {
            Ok(p) => p,
            Err(err) => return probe_stop(Some(&name), err),
        };
        if let Image::Png(info) = &probed.image
            && let Err(err) = probe::check_png_data(&buf, info)
        {
            return probe_stop(Some(&name), err);
        }
        if probed.orientation_ignored {
            notes.orientation += 1;
        }
        if probed.profile_ignored {
            notes.profile += 1;
        }
        writer.add_page(&buf, &probed.image).map_err(write_err)?;
        ev.progress((i as f32 + (k + 1) as f32 / pages.len() as f32) / n as f32);
    }
    let mut out = writer.finish().map_err(write_err)?;
    out.flush().map_err(write_err)?;
    partial.armed = false;
    Ok((partial.path.clone(), notes))
}

fn process(inv: &Invocation, i: usize, input: &Path, ev: &mut Events<impl Write>) -> Outcome {
    if input.to_str().is_none() {
        ev.skip(input, "non-UTF-8 path");
        return Outcome::Unsupported;
    }
    match convert(inv, i, input, inv.inputs.len(), ev) {
        Ok((out, notes)) => {
            ev.done(input, out.strip_prefix(&inv.work).unwrap_or(&out));
            if notes.non_image > 0 {
                ev.warn(
                    input,
                    &format!("{} non-image entries left out", notes.non_image),
                );
            }
            if notes.orientation > 0 {
                ev.warn(
                    input,
                    &format!("orientation ignored on {} pages", notes.orientation),
                );
            }
            if notes.profile > 0 {
                ev.warn(
                    input,
                    &format!("colour profile not embedded on {} pages", notes.profile),
                );
            }
            Outcome::Done
        }
        Err(Stop::Skip(reason)) => {
            ev.skip(input, &reason);
            Outcome::Unsupported
        }
        Err(Stop::Fail(reason)) => {
            ev.fail(input, &reason);
            Outcome::Failed
        }
    }
}

fn run(inv: &Invocation) -> u8 {
    if !inv.work.is_dir() {
        eprintln!("work directory does not exist: {}", inv.work.display());
        return action::EXIT_FAILED as u8;
    }
    let mut ev = Events::new(io::stdout().lock());
    let n = inv.inputs.len();
    // The existing `progress 0` event, before any input work: the first
    // line the engine reads, so it marks the child's entry for the
    // measurement log. No new event.
    ev.progress(0.0);
    let (mut done, mut failed) = (0, 0);
    for (i, input) in inv.inputs.iter().enumerate() {
        match process(inv, i, input, &mut ev) {
            Outcome::Done => done += 1,
            Outcome::Failed => failed += 1,
            Outcome::Unsupported => {}
        }
        ev.progress((i + 1) as f32 / n as f32);
    }
    // Batch rule: any `done` -> 0; else any `fail` -> 1; else all unsupported -> 3.
    if done > 0 {
        action::EXIT_OK as u8
    } else if failed > 0 {
        action::EXIT_FAILED as u8
    } else {
        action::EXIT_UNSUPPORTED as u8
    }
}

/// Why the whole join stopped: every input taken so far, and every one not
/// yet looked at, fails with the same reason and nothing is published.
struct Whole(String);

/// The one output of a join in progress.
struct JoinOut {
    writer: pdf::Writer<BufWriter<Capped<File>>>,
    /// Removes the half-written PDF on every way out but a finished join.
    partial: Partial,
    /// Upper bound of the file size so far (see `PAGE_OVERHEAD`).
    estimate: u64,
}

/// What became of one join input that did not fail the whole join.
#[derive(Clone, Copy, PartialEq)]
enum Fate {
    Taken,
    Skipped,
    Failed,
}

/// Add one image to the join. The input's own `skip` or `fail` is reported
/// here and the join goes on; `Err` fails the whole join.
fn join_one(
    inv: &Invocation,
    input: &Path,
    cap: Option<u64>,
    tripped: &Rc<Cell<bool>>,
    out: &mut Option<JoinOut>,
    ev: &mut Events<impl Write>,
) -> Result<Fate, Whole> {
    let too_large = |c: u64| Whole(format!("PDF would be larger than {} MiB", c >> 20));
    // One image at a time, read whole and bounded: memory is the largest
    // image, not the sum. `read_file_bounded` never waits on a FIFO.
    let buf = match action::read_file_bounded(input, ENTRY_BYTES as usize) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            ev.fail(
                input,
                &format!("image is larger than {} MiB", ENTRY_BYTES >> 20),
            );
            return Ok(Fate::Failed);
        }
        Err(e) => {
            ev.fail(input, &e.to_string());
            return Ok(Fate::Failed);
        }
    };
    let probed = match probe::probe(&buf, MAX_PIXELS) {
        Ok(p) => p,
        Err(e) => return Ok(report(input, probe_stop(None, e), ev)),
    };
    if let Image::Png(info) = &probed.image
        && let Err(e) = probe::check_png_data(&buf, info)
    {
        return Ok(report(input, probe_stop(None, e), ev));
    }

    // The output ceiling: the sum of the image sizes plus the PDF structure
    // is the file size at the worst, checked before the page is written.
    let add = (buf.len() as u64).saturating_add(PAGE_OVERHEAD);
    let before = out.as_ref().map_or(FILE_OVERHEAD, |o| o.estimate);
    let estimate = before.saturating_add(add);
    if let Some(c) = cap
        && estimate > c
    {
        return Err(too_large(c));
    }
    if out.is_none() {
        // Created on the first image that can be taken, so a join with
        // nothing to take leaves no file and no `join/` folder behind.
        let path = action::join_path(&inv.work, "pdf");
        if let Some(dir) = path.parent()
            && let Err(e) = fs::create_dir_all(dir)
        {
            return Err(Whole(format!("cannot create output: {e}")));
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| Whole(format!("cannot create output: {e}")))?;
        let partial = Partial { path, armed: true };
        let capped = Capped {
            inner: file,
            cap,
            written: 0,
            tripped: Rc::clone(tripped),
        };
        let writer =
            pdf::Writer::new(BufWriter::new(capped)).map_err(|e| write_whole(e, cap, tripped))?;
        *out = Some(JoinOut {
            writer,
            partial,
            estimate: FILE_OVERHEAD,
        });
    }
    let o = out.as_mut().expect("created above");
    o.writer
        .add_page(&buf, &probed.image)
        .map_err(|e| write_whole(e, cap, tripped))?;
    o.estimate = estimate;
    if probed.orientation_ignored {
        ev.warn(input, "orientation ignored");
    }
    if probed.profile_ignored {
        ev.warn(input, "colour profile not embedded");
    }
    Ok(Fate::Taken)
}

/// Report a stopped input as its `skip` or `fail`.
fn report(input: &Path, stop: Result<(), Stop>, ev: &mut Events<impl Write>) -> Fate {
    match stop {
        Err(Stop::Skip(r)) => {
            ev.skip(input, &r);
            Fate::Skipped
        }
        Err(Stop::Fail(r)) => {
            ev.fail(input, &r);
            Fate::Failed
        }
        Ok(()) => Fate::Taken,
    }
}

/// A write error as the reason the whole join fails: the size ceiling by
/// its own copy, anything else as an I/O error.
fn write_whole(e: io::Error, cap: Option<u64>, tripped: &Rc<Cell<bool>>) -> Whole {
    match cap {
        Some(c) if tripped.get() => Whole(format!("PDF would be larger than {} MiB", c >> 20)),
        _ => Whole(format!("cannot write output: {e}")),
    }
}

/// `pages.images-to-pdf`. Events: `take` per image, `skip`/`fail`/`warn`
/// per image, `joined` once. Exit: 0 with a `joined`; 3 when nothing was
/// taken and every input was `skip`ped; else 1.
fn run_join(inv: &Invocation) -> u8 {
    if !inv.work.is_dir() {
        eprintln!("work directory does not exist: {}", inv.work.display());
        return action::EXIT_FAILED as u8;
    }
    let mut ev = Events::new(io::stdout().lock());
    let n = inv.inputs.len();
    ev.progress(0.0);
    // Takes are reported only once the PDF is finished: a join that fails
    // as a whole must not have announced inputs as taken.
    let mut taken: Vec<&Path> = Vec::new();
    let mut any_fail = false;
    let mut whole: Option<Whole> = None;
    let mut out: Option<JoinOut> = None;
    let tripped = Rc::new(Cell::new(false));
    let cap = match output_cap() {
        Ok(c) => c,
        Err(e) => {
            whole = Some(Whole(format!("cannot read the output size limit: {e}")));
            None
        }
    };
    let mut next = 0;
    if whole.is_none() {
        for (i, input) in inv.inputs.iter().enumerate() {
            if input.to_str().is_none() {
                ev.skip(input, "non-UTF-8 path");
                ev.progress((i + 1) as f32 / n as f32);
                continue;
            }
            match join_one(inv, input, cap, &tripped, &mut out, &mut ev) {
                Ok(Fate::Taken) => taken.push(input.as_path()),
                Ok(Fate::Failed) => any_fail = true,
                Ok(Fate::Skipped) => {}
                Err(w) => {
                    // The input that tripped it and all after it.
                    next = i;
                    whole = Some(w);
                    break;
                }
            }
            ev.progress((i + 1) as f32 / n as f32);
        }
    }

    if let Some(Whole(reason)) = whole {
        // Dropping `out` removes the half-written PDF.
        drop(out);
        for input in taken
            .iter()
            .copied()
            .chain(inv.inputs[next..].iter().map(PathBuf::as_path))
        {
            ev.fail(input, &reason);
        }
        return action::EXIT_FAILED as u8;
    }
    let Some(JoinOut {
        writer,
        mut partial,
        ..
    }) = out
    else {
        // Nothing taken: 3 when every input was a `skip`, else 1.
        return if any_fail {
            action::EXIT_FAILED as u8
        } else {
            action::EXIT_UNSUPPORTED as u8
        };
    };
    let finished = writer
        .finish()
        .and_then(|mut w| w.flush())
        .map_err(|e| write_whole(e, cap, &tripped));
    if let Err(Whole(reason)) = finished {
        drop(partial);
        for input in &taken {
            ev.fail(input, &reason);
        }
        return action::EXIT_FAILED as u8;
    }
    partial.armed = false;
    for input in &taken {
        ev.take(input);
    }
    let rel = partial
        .path
        .strip_prefix(&inv.work)
        .unwrap_or(&partial.path);
    ev.joined(rel);
    action::EXIT_OK as u8
}

/// Fewest digits of a CBZ page number: 3, so a book of up to 999 pages
/// sorts by name in every file manager (`001` ... `999`) and the usual
/// comic readers, which sort names as text, keep the order. A longer join
/// widens every name to the digits of the input count (see `cbz_width`).
const CBZ_MIN_WIDTH: usize = 3;

/// The digits of every page number in a join of `inputs` files: at least
/// `CBZ_MIN_WIDTH`, else the digits of the count, so all names have one
/// width and sort as text. The count includes inputs that fail, which can
/// only make the width one digit too wide, never too narrow.
fn cbz_width(inputs: usize) -> usize {
    (inputs.max(1).ilog10() as usize + 1).max(CBZ_MIN_WIDTH)
}

/// The one output of a CBZ join in progress.
struct CbzOut {
    writer: zip::StoreWriter,
    /// Removes the half-written CBZ on every way out but a finished join:
    /// the archive has no end record before `finish`, but image data may
    /// hold one, so a leftover prefix is never kept.
    partial: Partial,
}

/// Why an entry could not be added.
#[derive(Debug, PartialEq)]
enum AddFail {
    /// Only this input fails; the writer rolled the entry back.
    Input(String),
    /// The whole join fails: the file is in an unknown state or at a limit.
    Whole(String),
}

/// Add `len` bytes of `r` as `name`. The writer reads at most `len` bytes
/// (the size the file had when it was measured, already checked against
/// `ENTRY_BYTES`), so the file written never passes the size the ceiling
/// check allowed, even if the source grows while it is copied.
fn add_entry(
    w: &mut zip::StoreWriter,
    name: &str,
    r: &mut impl Read,
    len: u64,
) -> Result<(), AddFail> {
    use zip::WriteError as W;
    match w.add(name, r, len) {
        Ok(_) => Ok(()),
        Err(W::Read(e)) => Err(AddFail::Input(e.to_string())),
        Err(W::EntryTooLarge) => Err(AddFail::Input("image changed while it was read".into())),
        Err(W::ArchiveTooLarge) => Err(AddFail::Whole("CBZ would be larger than 4 GiB".into())),
        // Unreachable, hence untested here (the writer's own test covers the
        // error): the run takes at most 1024 inputs, far below the 65 534
        // entries a ZIP holds without the ZIP64 marker, and the projection
        // checks the count before an entry is added. The text must still be
        // true if the limits ever change.
        Err(W::TooManyEntries) => Err(AddFail::Whole(format!(
            "CBZ would have more than {} images",
            zip::MAX_ENTRIES
        ))),
        Err(e @ (W::Io(_) | W::Poisoned | W::BadName | W::DuplicateName)) => {
            Err(AddFail::Whole(format!("cannot write output: {e}")))
        }
    }
}

/// The ceilings before an entry of `len` bytes is written: the ZIP format's
/// (4 GiB, entry count) and the process's `RLIMIT_FSIZE` (`cap`).
fn check_ceilings(
    w: &zip::StoreWriter,
    name_len: usize,
    len: u64,
    cap: Option<u64>,
) -> Result<(), Whole> {
    match w.projected_len(name_len, len) {
        None if w.len() >= zip::MAX_ENTRIES => Err(Whole(format!(
            "CBZ would have more than {} images",
            zip::MAX_ENTRIES
        ))),
        None => Err(Whole("CBZ would be larger than 4 GiB".into())),
        Some(total) => match cap {
            Some(c) if total > c => Err(Whole(format!("CBZ would be larger than {} MiB", c >> 20))),
            _ => Ok(()),
        },
    }
}

/// A whole-join failure: the half-written CBZ goes (dropping `out`) and
/// every input that was taken or not yet looked at gets the same `fail`.
fn fail_cbz_whole<'a>(
    out: Option<CbzOut>,
    inputs: impl IntoIterator<Item = &'a Path>,
    reason: &str,
    ev: &mut Events<impl Write>,
) -> u8 {
    drop(out);
    for input in inputs {
        ev.fail(input, reason);
    }
    action::EXIT_FAILED as u8
}

/// Finish the archive. On an error the file goes and every taken input
/// fails; otherwise the takes and the one `joined` follow.
fn finish_cbz(o: CbzOut, work: &Path, taken: &[&Path], ev: &mut Events<impl Write>) -> u8 {
    let CbzOut {
        writer,
        mut partial,
    } = o;
    if let Err(e) = writer.finish() {
        let reason = format!("cannot write output: {e}");
        drop(partial);
        for input in taken {
            ev.fail(input, &reason);
        }
        return action::EXIT_FAILED as u8;
    }
    partial.armed = false;
    for input in taken {
        ev.take(input);
    }
    let rel = partial.path.strip_prefix(work).unwrap_or(&partial.path);
    ev.joined(rel);
    action::EXIT_OK as u8
}

/// Add one image to the CBZ join. The input's own `skip` or `fail` is
/// reported here and the join goes on; `Err` fails the whole join.
fn cbz_one(
    inv: &Invocation,
    input: &Path,
    width: usize,
    cap: Option<u64>,
    out: &mut Option<CbzOut>,
    ev: &mut Events<impl Write>,
) -> Result<Fate, Whole> {
    // Opened without waiting (a FIFO fails here) and read as a stream: the
    // image is never held whole, so memory is one 64 KiB copy buffer.
    let mut file = match open_input(input) {
        Ok(f) => f,
        Err(e) => {
            ev.fail(input, &e.to_string());
            return Ok(Fate::Failed);
        }
    };
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            ev.fail(input, &e.to_string());
            return Ok(Fate::Failed);
        }
    };
    if len > ENTRY_BYTES {
        ev.fail(
            input,
            &format!("image is larger than {} MiB", ENTRY_BYTES >> 20),
        );
        return Ok(Fate::Failed);
    }
    // Header check only (the image is stored as it is). A read error is
    // reported by the sniff as damaged data. `take` bounds the walk if the
    // file grows after the size check above: the sniff stops at the first
    // SOF, but a growing file must never make it read without end.
    let sniffed = match probe::sniff(&mut (&mut file).take(ENTRY_BYTES), MAX_PIXELS) {
        Ok(s) => s,
        Err(e) => return Ok(report(input, probe_stop(None, e), ev)),
    };
    if let Err(e) = file.seek(SeekFrom::Start(0)) {
        ev.fail(input, &e.to_string());
        return Ok(Fate::Failed);
    }

    if out.is_none() {
        // Created on the first image that can be taken, so a join with
        // nothing to take leaves no file and no `join/` folder behind.
        let path = action::join_path(&inv.work, "cbz");
        if let Some(dir) = path.parent()
            && let Err(e) = fs::create_dir_all(dir)
        {
            return Err(Whole(format!("cannot create output: {e}")));
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| Whole(format!("cannot create output: {e}")))?;
        *out = Some(CbzOut {
            writer: zip::StoreWriter::new(file),
            partial: Partial { path, armed: true },
        });
    }
    let o = out.as_mut().expect("created above");
    let name = format!("{:0width$}.{}", o.writer.len() + 1, sniffed.format.ext());

    // The ceilings before anything is written: the projection is the exact
    // size of the finished archive with this entry (headers, directory and
    // end record), so no write passes the ZIP format's 4 GiB or the
    // process's RLIMIT_FSIZE.
    check_ceilings(&o.writer, name.len(), len, cap)?;
    match add_entry(&mut o.writer, &name, &mut file, len) {
        Ok(()) => Ok(Fate::Taken),
        Err(AddFail::Input(reason)) => {
            ev.fail(input, &reason);
            Ok(Fate::Failed)
        }
        Err(AddFail::Whole(reason)) => Err(Whole(reason)),
    }
}

/// `pages.images-to-cbz`. Events as `run_join`: `take` per image once the
/// CBZ is finished, `skip`/`fail` per image, `joined` once. A write error
/// or a ceiling fails the whole join and removes the half-written file.
fn run_cbz_join(inv: &Invocation) -> u8 {
    if !inv.work.is_dir() {
        eprintln!("work directory does not exist: {}", inv.work.display());
        return action::EXIT_FAILED as u8;
    }
    let mut ev = Events::new(io::stdout().lock());
    let n = inv.inputs.len();
    ev.progress(0.0);
    let width = cbz_width(n);
    let mut taken: Vec<&Path> = Vec::new();
    let mut any_fail = false;
    let mut whole: Option<Whole> = None;
    let mut out: Option<CbzOut> = None;
    let cap = match output_cap() {
        Ok(c) => c,
        Err(e) => {
            whole = Some(Whole(format!("cannot read the output size limit: {e}")));
            None
        }
    };
    let mut next = 0;
    if whole.is_none() {
        for (i, input) in inv.inputs.iter().enumerate() {
            if input.to_str().is_none() {
                ev.skip(input, "non-UTF-8 path");
                ev.progress((i + 1) as f32 / n as f32);
                continue;
            }
            match cbz_one(inv, input, width, cap, &mut out, &mut ev) {
                Ok(Fate::Taken) => taken.push(input.as_path()),
                Ok(Fate::Failed) => any_fail = true,
                Ok(Fate::Skipped) => {}
                Err(w) => {
                    next = i;
                    whole = Some(w);
                    break;
                }
            }
            ev.progress((i + 1) as f32 / n as f32);
        }
    }

    if let Some(Whole(reason)) = whole {
        let inputs = taken
            .iter()
            .copied()
            .chain(inv.inputs[next..].iter().map(PathBuf::as_path));
        return fail_cbz_whole(out, inputs, &reason, &mut ev);
    }
    let Some(out) = out else {
        return if any_fail {
            action::EXIT_FAILED as u8
        } else {
            action::EXIT_UNSUPPORTED as u8
        };
    };
    finish_cbz(out, &inv.work, &taken, &mut ev)
}

fn main() -> ExitCode {
    let inv = match parse_args(std::env::args_os().skip(1)) {
        Ok(Command::Manifest) => {
            let _ = io::stdout().write_all(manifest_text().as_bytes());
            return ExitCode::SUCCESS;
        }
        Ok(Command::Run(inv)) => inv,
        Err(msg) => {
            eprintln!("inacircle-pages: {msg}");
            return ExitCode::from(action::EXIT_USAGE as u8);
        }
    };
    match inv.action.as_str() {
        "pages.cbz-to-pdf" => ExitCode::from(run(&inv)),
        "pages.images-to-pdf" => ExitCode::from(run_join(&inv)),
        "pages.images-to-cbz" => ExitCode::from(run_cbz_join(&inv)),
        _ => {
            eprintln!("inacircle-pages: unknown action {}", inv.action);
            ExitCode::from(action::EXIT_USAGE as u8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_stops_before_the_write_that_crosses_the_cap() {
        let tripped = Rc::new(Cell::new(false));
        let mut w = Capped {
            inner: Vec::new(),
            cap: Some(10),
            written: 0,
            tripped: Rc::clone(&tripped),
        };
        w.write_all(b"0123456789").unwrap();
        assert!(!tripped.get(), "exactly the cap is allowed");
        assert!(w.write_all(b"x").is_err());
        assert!(tripped.get());
        assert_eq!(w.inner, b"0123456789", "nothing past the cap is written");
    }

    #[test]
    fn capped_without_a_cap_passes_everything() {
        let mut w = Capped {
            inner: Vec::new(),
            cap: None,
            written: 0,
            tripped: Rc::new(Cell::new(false)),
        };
        w.write_all(&[1; 100]).unwrap();
        assert_eq!(w.inner.len(), 100);
    }

    #[test]
    fn capped_flush_reaches_the_inner_writer() {
        struct Flushes(u32);
        impl Write for Flushes {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0 += 1;
                Ok(())
            }
        }
        let mut w = Capped {
            inner: Flushes(0),
            cap: None,
            written: 0,
            tripped: Rc::new(Cell::new(false)),
        };
        w.flush().unwrap();
        assert_eq!(w.inner.0, 1);
    }

    #[test]
    fn write_whole_names_the_ceiling_only_when_it_tripped() {
        let err = || io::Error::other("disk");
        let tripped = Rc::new(Cell::new(true));
        assert_eq!(
            write_whole(err(), Some(3 << 20), &tripped).0,
            "PDF would be larger than 3 MiB"
        );
        tripped.set(false);
        assert_eq!(
            write_whole(err(), Some(3 << 20), &tripped).0,
            "cannot write output: disk"
        );
        assert_eq!(
            write_whole(err(), None, &Rc::new(Cell::new(true))).0,
            "cannot write output: disk"
        );
    }

    #[test]
    fn extension_needs_a_dot() {
        assert!(kind_of(b"1.png") == Some(Kind::Png));
        assert!(kind_of(b"a.b.JPEG") == Some(Kind::Jpeg));
        assert!(kind_of(b".jpg") == Some(Kind::Jpeg));
        // A name that is only an extension word has no extension.
        assert!(kind_of(b"png").is_none());
        assert!(kind_of(b"jpg").is_none());
        assert!(kind_of(b"dir.png/page").is_none());
    }

    #[test]
    fn metadata_names() {
        assert!(is_metadata(b"__MACOSX/._1.jpg"));
        assert!(is_metadata(b"__MACOSX/a/2.png"));
        assert!(is_metadata(b"a/._1.jpg"));
        assert!(is_metadata(b".png"));
        assert!(!is_metadata(b"a/1.png"));
        assert!(!is_metadata(b"__MACOSXX/1.png"));
    }

    /// A file path under the temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            Scratch(
                std::env::temp_dir()
                    .join(format!("inacircle-pages-unit-{}-{tag}", std::process::id())),
            )
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn writer(s: &Scratch) -> zip::StoreWriter {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&s.0)
            .unwrap();
        zip::StoreWriter::new(f)
    }

    #[test]
    fn cbz_width_is_three_digits_or_the_count_digits() {
        assert_eq!(cbz_width(0), 3);
        assert_eq!(cbz_width(1), 3);
        assert_eq!(cbz_width(999), 3);
        assert_eq!(cbz_width(1000), 4);
        assert_eq!(cbz_width(9999), 4);
        assert_eq!(cbz_width(10_000), 5);
    }

    #[test]
    fn a_source_that_grows_past_its_length_fails_only_that_entry() {
        let s = Scratch::new("grow");
        let mut w = writer(&s);
        assert!(add_entry(&mut w, "001.png", &mut &b"abcd"[..], 4).is_ok());
        // Measured as 4 bytes, yields 10: the entry is rolled back.
        let got = add_entry(&mut w, "002.png", &mut &b"0123456789"[..], 4);
        assert_eq!(
            got,
            Err(AddFail::Input("image changed while it was read".into()))
        );
        // The writer is usable and the next entry takes the number.
        assert!(add_entry(&mut w, "002.png", &mut &b"wxyz"[..], 4).is_ok());
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn a_failing_source_fails_only_that_entry() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("disk gone"))
            }
        }
        let s = Scratch::new("broken");
        let mut w = writer(&s);
        let got = add_entry(&mut w, "001.png", &mut Broken, 4);
        assert_eq!(got, Err(AddFail::Input("disk gone".into())));
        assert!(add_entry(&mut w, "001.png", &mut &b"abcd"[..], 4).is_ok());
    }

    #[test]
    fn a_write_error_fails_the_whole_join_and_the_file_goes() {
        // A file opened read-only: the first write fails, as ENOSPC would.
        let s = Scratch::new("ro");
        fs::write(&s.0, b"").unwrap();
        let ro = File::open(&s.0).unwrap();
        let mut out = Some(CbzOut {
            writer: zip::StoreWriter::new(ro),
            partial: Partial {
                path: s.0.clone(),
                armed: true,
            },
        });
        let o = out.as_mut().unwrap();
        let got = add_entry(&mut o.writer, "001.png", &mut &b"abcd"[..], 4);
        assert!(matches!(got, Err(AddFail::Whole(ref r)) if r.starts_with("cannot write output")));
        // Every later add is refused too (poisoned), then dropping the
        // output removes the half-written file.
        let again = add_entry(&mut o.writer, "002.png", &mut &b"abcd"[..], 4);
        assert!(matches!(again, Err(AddFail::Whole(_))));
        assert!(s.0.exists());
        drop(out);
        assert!(!s.0.exists(), "the half-written CBZ was kept");
    }

    /// A writer over a read-only file (every write fails, as ENOSPC would)
    /// and the guard that removes that file.
    fn read_only_out(s: &Scratch) -> CbzOut {
        fs::write(&s.0, b"").unwrap();
        CbzOut {
            writer: zip::StoreWriter::new(File::open(&s.0).unwrap()),
            partial: Partial {
                path: s.0.clone(),
                armed: true,
            },
        }
    }

    fn event_lines(buf: &[u8], kind: &str) -> Vec<String> {
        String::from_utf8_lossy(buf)
            .lines()
            .filter(|l| l.split('\t').next() == Some(kind))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn a_finish_error_keeps_no_file_and_fails_every_taken_input() {
        let s = Scratch::new("finish");
        let o = read_only_out(&s);
        let (a, b) = (PathBuf::from("a.png"), PathBuf::from("b.png"));
        let mut buf = Vec::new();
        let code = finish_cbz(
            o,
            Path::new("/"),
            &[a.as_path(), b.as_path()],
            &mut Events::new(&mut buf),
        );
        assert_eq!(code, action::EXIT_FAILED as u8);
        assert!(!s.0.exists(), "the half-written CBZ was kept");
        assert!(event_lines(&buf, "joined").is_empty());
        assert!(event_lines(&buf, "take").is_empty());
        let f = event_lines(&buf, "fail");
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f.iter().all(|l| l.contains("cannot write output")), "{f:?}");
    }

    #[test]
    fn a_finished_cbz_takes_every_input_and_joins_once() {
        let s = Scratch::new("finished");
        let mut o = Some(CbzOut {
            writer: writer(&s),
            partial: Partial {
                path: s.0.clone(),
                armed: true,
            },
        });
        add_entry(
            &mut o.as_mut().unwrap().writer,
            "001.png",
            &mut &b"abcd"[..],
            4,
        )
        .unwrap();
        let a = PathBuf::from("a.png");
        let mut buf = Vec::new();
        let code = finish_cbz(
            o.take().unwrap(),
            s.0.parent().unwrap(),
            &[a.as_path()],
            &mut Events::new(&mut buf),
        );
        assert_eq!(code, action::EXIT_OK as u8);
        assert!(s.0.exists());
        assert_eq!(event_lines(&buf, "take").len(), 1);
        assert_eq!(event_lines(&buf, "joined").len(), 1);
        assert!(event_lines(&buf, "fail").is_empty());
    }

    #[test]
    fn a_projection_past_four_gib_fails_the_whole_join() {
        let s = Scratch::new("fourgib");
        let o = read_only_out(&s);
        // One entry whose data alone cannot fit the 32 bit fields.
        let got = check_ceilings(&o.writer, 7, u64::from(u32::MAX), None);
        let Err(Whole(reason)) = got else {
            panic!("a 4 GiB entry passed the ceilings");
        };
        assert_eq!(reason, "CBZ would be larger than 4 GiB");
        // The join then fails: no file, no joined, every input gets the copy.
        let (a, b) = (PathBuf::from("a.png"), PathBuf::from("b.png"));
        let mut buf = Vec::new();
        let code = fail_cbz_whole(
            Some(o),
            [a.as_path(), b.as_path()],
            &reason,
            &mut Events::new(&mut buf),
        );
        assert_eq!(code, action::EXIT_FAILED as u8);
        assert!(!s.0.exists(), "the half-written CBZ was kept");
        assert!(event_lines(&buf, "joined").is_empty());
        let f = event_lines(&buf, "fail");
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(
            f.iter()
                .all(|l| l.ends_with("CBZ would be larger than 4 GiB"))
        );
    }

    #[test]
    fn the_rlimit_ceiling_names_its_size_and_a_fit_passes() {
        let s = Scratch::new("cap");
        let o = read_only_out(&s);
        let Err(Whole(r)) = check_ceilings(&o.writer, 7, 2 << 20, Some(1 << 20)) else {
            panic!("over the cap passed");
        };
        assert_eq!(r, "CBZ would be larger than 1 MiB");
        assert!(check_ceilings(&o.writer, 7, 4096, Some(1 << 20)).is_ok());
    }

    #[test]
    fn the_entry_count_has_its_own_copy() {
        let s = Scratch::new("count");
        let mut w = writer(&s);
        // 65 534 empty entries fill the archive's count.
        for i in 0..zip::MAX_ENTRIES {
            add_entry(&mut w, &format!("{i:05}.png"), &mut &b""[..], 0).unwrap();
        }
        let Err(Whole(r)) = check_ceilings(&w, 9, 0, None) else {
            panic!("one entry too many passed");
        };
        assert_eq!(r, "CBZ would have more than 65534 images");
    }

    #[test]
    fn a_disarmed_partial_keeps_its_file() {
        let s = Scratch::new("kept");
        fs::write(&s.0, b"x").unwrap();
        drop(Partial {
            path: s.0.clone(),
            armed: false,
        });
        assert!(s.0.exists());
    }
}
