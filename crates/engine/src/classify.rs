//! Content classifier: what is this file, as far as the engine can tell.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kind {
    pub mime: Option<String>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classified {
    /// A regular file (after following links) and what it is.
    File(Kind),
    /// Directory, device, FIFO, socket or broken link: never opened.
    NotRegular(Shape),
}

/// What a path that is not a regular file is, as the one stat in
/// [`classify_with`] saw it: callers need no second look at the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Folder,
    /// A dangling link or a link loop.
    BrokenLink,
    /// Device, FIFO, socket.
    Special,
}

/// How much of the file decides the type.
const HEAD: usize = 4096;

/// A text file that is not UTF-8: a candidate for the code page actions,
/// which check the whole file themselves. No other preset takes it.
pub const X8BIT_MIME: &str = "text/x-8bit";

/// A UTF-8 SubRip subtitle: a `.srt` whose 4 KiB head has a `-->` cue
/// timing arrow. The name alone would give a binary `.srt` a subtitle type.
pub const SUBRIP_MIME: &str = "application/x-subrip";

/// A WebVTT subtitle: a `.vtt` whose head opens with the `WEBVTT` signature
/// (after an optional byte order mark).
pub const WEBVTT_MIME: &str = "text/vtt";

/// An ASS or SSA script: a `.ass` or `.ssa` whose head has a `[Script Info]`
/// section (ASCII case-insensitive).
pub const SSA_MIME: &str = "text/x-ssa";

/// How far a text-named file with an all-ASCII head is read on, the head
/// included: a whole film subtitle (a feature-length `.srt` is 50-150 KiB).
/// Valid UTF-8 accents do not stop the read-on; it stops after the chunk
/// that holds the first byte that is not valid UTF-8, or a control byte,
/// at the file's end, or when the window or the drop's budget runs out. A
/// file with no such byte within it stays what its UTF-8 prefix says; its
/// action then reports a later byte.
const WINDOW: usize = 256 * 1024;

/// The read-on budget of one wheel invocation, shared by every file it
/// classifies (up to 1024 dropped files): four whole windows. Past it only
/// the head decides, so a drop of 1024 subtitles reads at most 1 MiB more
/// than the heads before the wheel shows, never 256 MiB.
const PER_DROP: u64 = 1 << 20;

/// One read of the read-on, on the stack: 16 reads cover a window, and
/// nothing on the heap grows with the file.
const CHUNK: usize = 16 * 1024;

/// The longest UTF-8 sequence less one byte: what a chunk's end can cut.
const CUT_MAX: usize = 3;

/// Extensions of text that may be in a legacy 8-bit code page: the plain
/// text and Markdown names, subtitles, tables, logs and configuration.
/// `nfo` is left out: it is usually CP437 art with escape codes, which the
/// Windows-1250 and ISO-8859-2 actions would turn into the wrong letters.
const X8BIT_EXT: &[&str] = &[
    "txt", "text", "md", "markdown", "mdown", "mkd", "srt", "vtt", "ass", "ssa", "sub", "csv",
    "tsv", "log", "ini", "cfg", "conf", "me", "1st", "readme", "diz",
];

/// The SUB byte (Ctrl-Z) DOS editors wrote as an end-of-file mark: allowed
/// once, as a file's very last byte.
const CTRL_Z: u8 = 0x1a;

/// What is left of the read-on budget of one wheel invocation.
pub struct Reread {
    left: u64,
}

impl Reread {
    /// A fresh budget for one drop (one wheel invocation).
    pub fn per_drop() -> Reread {
        Reread { left: PER_DROP }
    }
}

const ZIP_MAGIC: &[u8] = b"PK\x03\x04";

/// The MIME type of a comic book archive (ZIP of page images, `.cbz`).
const CBZ_MIME: &str = "application/vnd.comicbook+zip";

/// The MIME type of an EPUB book (a ZIP, `.epub`).
const EPUB_MIME: &str = "application/epub+zip";

/// Binary signatures: (prefix, mime the engine knows, label). JPEG and PNG
/// are known because the join action `images-to-pdf` takes them; whether a
/// given PNG is one it can place (alpha, interlacing) is the action's call.
const MAGIC: &[(&[u8], Option<&str>, &str)] = &[
    (b"%PDF-", None, "PDF"),
    (b"\x89PNG\r\n\x1a\n", Some("image/png"), "PNG"),
    (b"\xff\xd8\xff", Some("image/jpeg"), "JPEG"),
    (b"GIF87a", None, "GIF"),
    (b"GIF89a", None, "GIF"),
    (ZIP_MAGIC, None, "ZIP"),
    (b"\x1f\x8b", None, "gzip"),
    (
        b"INACIRCLE-TEST\n",
        Some("application/x-inacircle-test"),
        "test",
    ),
];

/// [`classify_with`] with a budget of its own.
pub fn classify(path: &Path) -> io::Result<Classified> {
    classify_with(path, &mut Reread::per_drop())
}

/// Classify by content (first 4096 bytes); the extension only refines text.
/// Follows symlinks; anything that is not a regular file is not read. A
/// text-named file with an all-ASCII head is read on, on the same handle,
/// up to 256 KiB and as far as `reread` allows, to tell 8-bit text from
/// UTF-8; one `Reread` serves every file of a drop.
pub fn classify_with(path: &Path, reread: &mut Reread) -> io::Result<Classified> {
    match open_regular(path)? {
        Ok(file) => classify_open(file, path, reread),
        Err(shape) => Ok(Classified::NotRegular(shape)),
    }
}

/// The path opened for reading if it is a regular file (links followed);
/// else what it is.
fn open_regular(path: &Path) -> io::Result<Result<File, Shape>> {
    let shape = |m: &std::fs::Metadata| {
        if m.is_dir() {
            Shape::Folder
        } else {
            Shape::Special
        }
    };
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => {}
        Ok(m) => return Ok(Err(shape(&m))),
        // A broken link or a symlink loop is not a regular file.
        Err(e) if e.kind() == io::ErrorKind::NotFound && path.is_symlink() => {
            return Ok(Err(Shape::BrokenLink));
        }
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(Err(Shape::BrokenLink)),
        Err(e) => return Err(e),
    }
    // The path may have been swapped for a FIFO since the stat: never block
    // in open, and re-check the type on the handle itself.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    let m = file.metadata()?;
    if !m.is_file() {
        return Ok(Err(shape(&m)));
    }
    Ok(Ok(file))
}

/// Classify an opened regular file; `path` gives only the extension. Every
/// read is on `file`, so a swapped name changes nothing.
fn classify_open(mut file: File, path: &Path, reread: &mut Reread) -> io::Result<Classified> {
    let mut head = Vec::with_capacity(HEAD);
    (&mut file).take(HEAD as u64).read_to_end(&mut head)?;

    // "foo." has an empty extension, which counts as none.
    let ext = path
        .extension()
        .filter(|e| !e.is_empty())
        .map(|e| e.to_string_lossy().to_lowercase());

    for (prefix, mime, label) in MAGIC {
        if head.starts_with(prefix) {
            // A comic book or an EPUB is a ZIP by content and `.cbz` or
            // `.epub` by name: neither alone. Only the head already read
            // and the extension decide; the archive itself is the
            // action's business.
            if *prefix == ZIP_MAGIC {
                let book = match ext.as_deref() {
                    Some("cbz") => Some((CBZ_MIME, "Comic book")),
                    Some("epub") => Some((EPUB_MIME, "EPUB")),
                    _ => None,
                };
                if let Some((mime, label)) = book {
                    return Ok(Classified::File(Kind {
                        mime: Some(mime.to_owned()),
                        label: label.to_owned(),
                    }));
                }
            }
            return Ok(Classified::File(Kind {
                mime: mime.map(str::to_owned),
                label: (*label).to_owned(),
            }));
        }
    }

    // The head's UTF-8 check serves both rules: whether it is UTF-8 text
    // and the 8-bit candidate check.
    let mut scan = Scan::default();
    let cut = scan.utf8(&head);
    let text = utf8_text(&head, &scan, cut) && !head.contains(&0);
    if eight_bit(&mut file, &head, ext.as_deref(), scan, cut, reread)? {
        return Ok(Classified::File(Kind {
            mime: Some(X8BIT_MIME.into()),
            label: "8-bit text".into(),
        }));
    }

    if text && let Some(kind) = subtitle(ext.as_deref(), &head) {
        return Ok(Classified::File(kind));
    }

    let kind = if text {
        match ext.as_deref() {
            None | Some("txt" | "text" | "log" | "1st" | "me" | "readme" | "diz" | "nfo") => Kind {
                mime: Some("text/plain".into()),
                label: "Plain text".into(),
            },
            Some("md" | "markdown" | "mdown" | "mkd") => Kind {
                mime: Some("text/markdown".into()),
                label: "Markdown".into(),
            },
            Some(e) => Kind {
                mime: None,
                label: format!(".{e} text"),
            },
        }
    } else {
        Kind {
            mime: None,
            label: match ext {
                Some(e) => format!(".{e} file"),
                None => "file".into(),
            },
        }
    };
    Ok(Classified::File(kind))
}

/// The subtitle type of a UTF-8 text file, from its extension and head.
/// `None` leaves the file to the plain text rules, so a name alone never
/// makes a subtitle.
fn subtitle(ext: Option<&str>, head: &[u8]) -> Option<Kind> {
    // A byte order mark may precede the WebVTT signature.
    let unbom = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
    let (mime, label) = match ext? {
        "srt" if in_head(head, b"-->") => (SUBRIP_MIME, "SRT subtitles"),
        "vtt" if unbom.starts_with(b"WEBVTT") => (WEBVTT_MIME, "WebVTT subtitles"),
        "ass" | "ssa" if in_head(head, b"[Script Info]") => (SSA_MIME, "ASS subtitles"),
        _ => return None,
    };
    Some(Kind {
        mime: Some(mime.into()),
        label: label.into(),
    })
}

/// Whether `needle` occurs in `head`, ASCII case-insensitively.
fn in_head(head: &[u8], needle: &[u8]) -> bool {
    head.windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

/// Whether the head is valid UTF-8, from its `scan` and `cut`. A
/// multi-byte sequence cut by the end of a full head is allowed; at the end
/// of a shorter (whole) file it is not.
fn utf8_text(head: &[u8], scan: &Scan, cut: usize) -> bool {
    !scan.invalid && (cut == 0 || head.len() == HEAD)
}

/// Whether the file is an 8-bit text candidate: a text extension or none,
/// no control byte, and not UTF-8, judged on the head; a text-named file
/// whose head is all ASCII is judged on what is read on as well. `scan` and
/// `cut` are the head's, checked for UTF-8 only; `file` is positioned
/// right after the head.
fn eight_bit(
    file: &mut File,
    head: &[u8],
    ext: Option<&str>,
    mut scan: Scan,
    mut cut: usize,
    reread: &mut Reread,
) -> io::Result<bool> {
    let text_name = match ext {
        None => false,
        Some(e) if X8BIT_EXT.contains(&e) => true,
        Some(_) => return Ok(false),
    };
    // Only a full, all-ASCII head of a text-named file is read on, and only
    // while the budget lasts (checked first: it is free).
    let read_on =
        text_name && reread.left > 0 && head.len() == HEAD && head[..head.len() - cut].is_ascii();
    // UTF-8 text that is not read on: the head decides, not a candidate,
    // and its control bytes need no look.
    if !read_on && utf8_text(head, &scan, cut) {
        return Ok(false);
    }
    scan.controls(head);
    if scan.control {
        return Ok(false);
    }
    // A shorter head is the whole file.
    if head.len() < HEAD {
        return Ok(scan.finish(true, cut));
    }
    if scan.invalid {
        // Only a Ctrl-Z as the head's last byte needs to know whether the
        // file ends there: one byte more tells.
        let at_end = !scan.pending_ctrl_z || file.read(&mut [0u8; 1])? == 0;
        return Ok(scan.finish(at_end, cut));
    }
    // A full head that is UTF-8 so far, and `read_on` holds.
    let want = (WINDOW - HEAD).min(usize::try_from(reread.left).unwrap_or(usize::MAX));
    let mut buf = [0u8; CHUNK + CUT_MAX];
    buf[..cut].copy_from_slice(&head[head.len() - cut..]);
    let (mut got, mut at_end) = (0, false);
    while got < want && !scan.control && !scan.invalid {
        let room = CHUNK.min(want - got);
        let n = match file.read(&mut buf[cut..cut + room]) {
            Ok(0) => {
                at_end = true;
                break;
            }
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        got += n;
        // `n` <= `want` <= `left`: the budget never goes below zero.
        reread.left -= n as u64;
        let len = cut + n;
        cut = scan.feed(&buf[..len]);
        buf.copy_within(len - cut..len, 0);
    }
    Ok(scan.finish(at_end, cut))
}

/// The 8-bit candidate check over a file's bytes, fed in order.
#[derive(Default)]
struct Scan {
    /// A NUL or C0 control byte other than TAB, LF, CR and FF, or a Ctrl-Z
    /// that is not the file's last byte.
    control: bool,
    /// Not valid UTF-8 (a sequence cut by the end of a buffer aside).
    invalid: bool,
    /// The last byte fed was a Ctrl-Z: fine only if nothing follows.
    pending_ctrl_z: bool,
}

impl Scan {
    /// Feed the next bytes, the previous call's cut sequence first; returns
    /// how many bytes at the end are a sequence cut by the buffer's end, to
    /// be fed again in front of the next bytes.
    fn feed(&mut self, bytes: &[u8]) -> usize {
        self.controls(bytes);
        self.utf8(bytes)
    }

    /// The control byte half of [`Scan::feed`].
    fn controls(&mut self, bytes: &[u8]) {
        let Some((&last, body)) = bytes.split_last() else {
            return;
        };
        // A cut sequence fed again is >= 0x80, never a control byte, and
        // never follows a pending Ctrl-Z (that was the last byte). A fold,
        // not `any`: without the early exit it vectorises, 6x faster on a
        // 256 KiB read-on.
        if self.pending_ctrl_z || body.iter().fold(false, |c, &b| c | control(b)) {
            self.control = true;
        }
        self.pending_ctrl_z = last == CTRL_Z;
        if control(last) && last != CTRL_Z {
            self.control = true;
        }
    }

    /// The UTF-8 half of [`Scan::feed`]: returns the cut sequence's length.
    fn utf8(&mut self, bytes: &[u8]) -> usize {
        if self.invalid {
            return 0;
        }
        match std::str::from_utf8(bytes) {
            Ok(_) => 0,
            Err(e) if e.error_len().is_some() => {
                self.invalid = true;
                0
            }
            Err(e) => bytes.len() - e.valid_up_to(),
        }
    }

    /// The verdict once reading stops; `at_end`: the file ended there.
    fn finish(self, at_end: bool, cut: usize) -> bool {
        let control = self.control || (self.pending_ctrl_z && !at_end);
        let invalid = self.invalid || (cut > 0 && at_end);
        !control && invalid
    }
}

/// A C0 control byte that is not text: everything below a space except
/// TAB, LF, CR and FF (NUL and Ctrl-Z included).
fn control(b: u8) -> bool {
    b < b' ' && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Dir {
            let p = std::env::temp_dir().join(format!("engine-cls-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn kind(p: &Path) -> (Option<String>, String) {
        match classify(p).unwrap() {
            Classified::File(k) => (k.mime, k.label),
            Classified::NotRegular(_) => panic!("not regular: {}", p.display()),
        }
    }
    fn is(mime: Option<&str>, label: &str) -> (Option<String>, String) {
        (mime.map(str::to_owned), label.to_owned())
    }
    fn x8bit() -> (Option<String>, String) {
        is(Some(X8BIT_MIME), "8-bit text")
    }
    fn kind_with(p: &Path, r: &mut Reread) -> (Option<String>, String) {
        match classify_with(p, r).unwrap() {
            Classified::File(k) => (k.mime, k.label),
            Classified::NotRegular(_) => panic!("not regular: {}", p.display()),
        }
    }

    /// "Árvíztűrő tükörfúrógép „x”" in Windows-1250.
    const CP1250: &[u8] = b"\xc1rv\xedzt\xfbr\xf5 t\xfck\xf6rf\xfar\xf3g\xe9p \x84x\x94\r\n";
    /// "Šárka ŁÓDŹ ő" in ISO-8859-2.
    const ISO: &[u8] = b"\xa9\xe1rka \xa3\xd3D\xac \xf5\n";
    /// ő in both code pages; never a byte of UTF-8 (past 0xF4), so it is
    /// invalid wherever it lands, a buffer's end included.
    const ACCENT: u8 = 0xf5;

    /// `len` bytes of ASCII subtitle-like lines with `byte` at `at`.
    fn late(len: usize, at: Option<usize>, byte: u8) -> Vec<u8> {
        let line = b"00:01:02,345 --> 00:01:04,000\r\nSome words here.\r\n\r\n";
        let mut b: Vec<u8> = line.iter().copied().cycle().take(len).collect();
        if let Some(at) = at {
            b[at] = byte;
        }
        b
    }

    #[test]
    fn cp1250_head_is_a_candidate() {
        let d = Dir::new("cp1250");
        for n in ["a.txt", "b.SRT", "c.csv", "d.md", "e.sub", "f.ini", "g.vtt"] {
            let p = d.file(n, CP1250);
            assert_eq!(kind(&p), x8bit(), "{n}");
        }
        // Not valid UTF-8 just past the head's middle: the head decides.
        let mut b = late(4096 * 3, None, 0);
        b[2000] = 0x9a; // š
        let p = d.file("h.ass", &b);
        assert_eq!(kind(&p), x8bit());
    }

    #[test]
    fn iso_head_is_a_candidate() {
        let d = Dir::new("iso");
        for n in [
            "a.text",
            "b.ssa",
            "c.tsv",
            "d.log",
            "e.cfg",
            "f.conf",
            "g.me",
            "h.1st",
            "i.readme",
            "j.diz",
            "k.markdown",
            "l.mdown",
            "m.mkd",
        ] {
            let p = d.file(n, ISO);
            assert_eq!(kind(&p), x8bit(), "{n}");
        }
    }

    #[test]
    fn utf8_text_stays_plain_or_markdown() {
        let d = Dir::new("utf8");
        let t = "Árvíztűrő tükörfúrógép „x”\n".as_bytes();
        let cases = [
            ("a.txt", is(Some("text/plain"), "Plain text")),
            ("README", is(Some("text/plain"), "Plain text")),
            ("b.nfo", is(Some("text/plain"), "Plain text")),
            ("c.md", is(Some("text/markdown"), "Markdown")),
            ("d.srt", is(None, ".srt text")),
            ("e.csv", is(None, ".csv text")),
        ];
        for (n, want) in &cases {
            let p = d.file(n, t);
            assert_eq!(&kind(&p), want, "{n}");
        }
        // A UTF-8 accent read on past an all-ASCII head stays UTF-8.
        let mut b = late(300 * 1024, None, 0);
        b[200 * 1024..200 * 1024 + 2].copy_from_slice("ő".as_bytes());
        // The late fixture's head has a cue arrow, so its `.srt` is a subtitle.
        let srt = is(Some(SUBRIP_MIME), "SRT subtitles");
        for (n, want) in &cases {
            let p = d.file(n, &b);
            let want = if *n == "d.srt" { &srt } else { want };
            assert_eq!(&kind(&p), want, "late {n}");
        }
        // A sequence cut by the head's end is completed by the read-on.
        let mut b = late(8192, None, 0);
        b[4095..4098].copy_from_slice("€".as_bytes());
        let p = d.file("f.txt", &b);
        assert_eq!(kind(&p), is(Some("text/plain"), "Plain text"));
        // A head that is already UTF-8 with accents decides: no read-on.
        let mut b = late(64 * 1024, None, 0);
        b[10..12].copy_from_slice("ő".as_bytes());
        b[20 * 1024] = ACCENT;
        let p = d.file("g.txt", &b);
        assert_eq!(kind(&p), is(Some("text/plain"), "Plain text"));
    }

    /// A valid multi-byte character cut by the end of a read-on chunk is
    /// carried into the next read: `€` and an emoji ending 1, 2 or 3 bytes
    /// past the first and the second chunk boundary of the read-on are
    /// UTF-8 text; the same with a bad last byte is a candidate.
    #[test]
    fn multibyte_across_read_on_chunks_is_utf8() {
        let d = Dir::new("chunks");
        let euro = "€".as_bytes();
        let emoji = "😀".as_bytes();
        for seq in [euro, emoji] {
            for edge in [HEAD + CHUNK, HEAD + 2 * CHUNK] {
                for before in 1..=3 {
                    let at = edge - before;
                    let mut b = late(100 * 1024, None, 0);
                    b[at..at + seq.len()].copy_from_slice(seq);
                    let p = d.file("ok.srt", &b);
                    let what = format!("{} B at {at} (edge {edge})", seq.len());
                    assert_eq!(kind(&p), is(Some(SUBRIP_MIME), "SRT subtitles"), "{what}");
                    b[at + seq.len() - 1] = b'A';
                    let p = d.file("bad.srt", &b);
                    assert_eq!(kind(&p), x8bit(), "bad {what}");
                }
            }
        }
    }

    #[test]
    fn late_accent_within_and_beyond_the_reread() {
        let d = Dir::new("late");
        let len = 300 * 1024;
        let at = |n: &str, pos: usize| d.file(n, &late(len, Some(pos), ACCENT));
        assert_eq!(kind(&at("a.srt", 200 * 1024)), x8bit());
        assert_eq!(
            kind(&at("b.srt", 260 * 1024)),
            is(Some(SUBRIP_MIME), "SRT subtitles")
        );
        assert_eq!(
            kind(&at("c.txt", 260 * 1024)),
            is(Some("text/plain"), "Plain text")
        );
        // The window's last byte and the first one past it.
        assert_eq!(kind(&at("d.srt", WINDOW - 1)), x8bit());
        assert_eq!(
            kind(&at("e.srt", WINDOW)),
            is(Some(SUBRIP_MIME), "SRT subtitles")
        );
        // Right after the head, and the head's own last byte.
        assert_eq!(kind(&at("f.srt", HEAD)), x8bit());
        assert_eq!(kind(&at("g.srt", HEAD - 1)), x8bit());
        // A lead byte cut by the head's end, followed by ASCII: invalid.
        let p = d.file("h.srt", &late(len, Some(HEAD - 1), 0xe9));
        assert_eq!(kind(&p), x8bit());
        // A file shorter than the window just ends.
        let p = d.file("i.srt", &late(20 * 1024, Some(19 * 1024), ACCENT));
        assert_eq!(kind(&p), x8bit());
        // A lead byte as a whole file's last byte is cut at the end: invalid.
        let p = d.file("j.srt", &late(20 * 1024, Some(20 * 1024 - 1), 0xe9));
        assert_eq!(kind(&p), x8bit());
    }

    #[test]
    fn reread_budget_is_shared_by_the_drop() {
        let d = Dir::new("budget");
        let b = late(300 * 1024, Some(200 * 1024), ACCENT);
        let files: Vec<PathBuf> = (0..50).map(|i| d.file(&format!("{i}.srt"), &b)).collect();
        // Alone, each one is a candidate.
        for p in &files {
            assert_eq!(kind(p), x8bit());
        }
        let mut r = Reread::per_drop();
        let total = r.left;
        let verdicts: Vec<_> = files.iter().map(|p| kind_with(p, &mut r)).collect();
        let lit = verdicts.iter().take_while(|v| **v == x8bit()).count();
        // Each candidate needs about 200 KiB read on: the first few fit,
        // the rest fall back to the head.
        assert!((1..=5).contains(&lit), "{lit}");
        let srt = is(Some(SUBRIP_MIME), "SRT subtitles");
        assert!(verdicts[lit..].iter().all(|v| *v == srt));
        assert_eq!(total, PER_DROP);
        assert_eq!(r.left, 0, "the budget is spent, never exceeded");
        // An exhausted budget leaves the head to decide.
        assert_eq!(kind_with(&files[0], &mut r), srt);
        // A file within its head costs nothing.
        let mut r = Reread::per_drop();
        let p = d.file("small.srt", CP1250);
        assert_eq!(kind_with(&p, &mut r), x8bit());
        assert_eq!(r.left, PER_DROP);
        // A read-on charges what it read: a 20 KiB file costs 16 KiB.
        let p = d.file("mid.srt", &late(20 * 1024, None, 0));
        assert_eq!(kind_with(&p, &mut r), srt);
        assert_eq!(r.left, PER_DROP - (20 * 1024 - HEAD) as u64);
    }

    #[test]
    fn binaries_named_as_text_are_not_candidates() {
        let d = Dir::new("bin-as-text");
        let p = d.file("kép.txt", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\xff\xfe");
        assert_eq!(kind(&p), is(Some("image/png"), "PNG"));
        let p = d.file("a.csv", b"PK\x03\x04\x14\0\0\0\x08\0\xe9\xf5");
        assert_eq!(kind(&p), is(None, "ZIP"));
        let p = d.file("z.log", b"\x1f\x8b\x08\0\xe9\xf5");
        assert_eq!(kind(&p), is(None, "gzip"));
        // UTF-16 (both byte orders): NUL bytes.
        let p = d.file("x.txt", b"\xff\xfeh\0\xe9\0\r\0\n\0");
        assert_eq!(kind(&p), is(None, ".txt file"));
        let p = d.file("y.srt", b"\xfe\xff\0h\0\xe9\0\n");
        assert_eq!(kind(&p), is(None, ".srt file"));
        // An executable: control bytes and NUL.
        let p = d.file("e.txt", b"\x7fELF\x02\x01\x01\0\0\0\x03\0>\0\x01");
        assert_eq!(kind(&p), is(None, ".txt file"));
        // High bytes and control bytes without NUL.
        let p = d.file("r.ini", b"\x8a\x02\x91\x05\x99\x1b\xe9\x10");
        assert_eq!(kind(&p), is(None, ".ini file"));
    }

    #[test]
    fn nul_or_control_byte_is_not_a_candidate() {
        let d = Dir::new("ctrl");
        let with = |n: &str, extra: &[u8]| {
            let mut b = CP1250.to_vec();
            b.extend_from_slice(extra);
            b.extend_from_slice(CP1250);
            d.file(n, &b)
        };
        for (i, c) in [0u8, 0x01, 0x07, 0x08, 0x0b, 0x1b, 0x1f, 0x1a]
            .into_iter()
            .enumerate()
        {
            let p = with(&format!("{i}.txt"), &[c]);
            assert_eq!(kind(&p), is(None, ".txt file"), "{c:#x}");
        }
        // TAB, LF, CR and FF are text.
        let p = with("ok.txt", b"\t\n\r\x0c");
        assert_eq!(kind(&p), x8bit());
        // Two control bytes are as bad as one (the head's check folds them).
        let p = with("two.txt", &[0x01, 0x01]);
        assert_eq!(kind(&p), is(None, ".txt file"));
        // DEL and C1 bytes are the code page's business, not control here.
        let p = with("del.txt", b"\x7f\x85");
        assert_eq!(kind(&p), x8bit());
        // A control byte read on past the head, before the accent.
        let mut b = late(64 * 1024, Some(40 * 1024), ACCENT);
        b[30 * 1024] = 0x1b;
        let p = d.file("late-esc.srt", &b);
        assert_eq!(kind(&p), is(Some(SUBRIP_MIME), "SRT subtitles"));
        let mut b = late(64 * 1024, Some(40 * 1024), ACCENT);
        b[50 * 1024] = 0;
        let p = d.file("late-nul.srt", &b);
        assert_eq!(kind(&p), is(Some(SUBRIP_MIME), "SRT subtitles"));
        // An all-ASCII head with a control byte is not read on.
        let mut b = late(64 * 1024, Some(40 * 1024), ACCENT);
        b[100] = 0x1b;
        let mut r = Reread::per_drop();
        let p = d.file("head-esc.srt", &b);
        assert_eq!(
            kind_with(&p, &mut r),
            is(Some(SUBRIP_MIME), "SRT subtitles")
        );
        assert_eq!(r.left, PER_DROP);
    }

    #[test]
    fn trailing_ctrl_z_is_allowed_elsewhere_not() {
        let d = Dir::new("ctrlz");
        let mut b = CP1250.to_vec();
        b.push(0x1a);
        assert_eq!(kind(&d.file("end.txt", &b)), x8bit());
        b.push(0x1a);
        assert_eq!(kind(&d.file("two.txt", &b)), is(None, ".txt file"));
        let mut b = CP1250.to_vec();
        b.push(0x1a);
        b.push(b'\n');
        assert_eq!(kind(&d.file("mid.txt", &b)), is(None, ".txt file"));
        // A file of exactly the head's size, ending in one.
        let mut b = late(HEAD, Some(10), ACCENT);
        b[HEAD - 1] = 0x1a;
        assert_eq!(kind(&d.file("full.txt", &b)), x8bit());
        // The same byte with more after it.
        let mut b = late(HEAD + 1, Some(10), ACCENT);
        b[HEAD - 1] = 0x1a;
        assert_eq!(kind(&d.file("full-more.txt", &b)), is(None, ".txt file"));
        // At the end of a file found by reading on.
        let mut b = late(100 * 1024, Some(50 * 1024), ACCENT);
        *b.last_mut().unwrap() = 0x1a;
        assert_eq!(kind(&d.file("late-end.srt", &b)), x8bit());
        // At the end of a read-on chunk, the file going on.
        let mut b = late(100 * 1024, Some(90 * 1024), ACCENT);
        b[HEAD + CHUNK - 1] = 0x1a;
        assert_eq!(
            kind(&d.file("chunk-end.srt", &b)),
            is(Some(SUBRIP_MIME), "SRT subtitles")
        );
        // The head's last byte, the file going on.
        let mut b = late(100 * 1024, Some(50 * 1024), ACCENT);
        b[HEAD - 1] = 0x1a;
        assert_eq!(
            kind(&d.file("head-end.srt", &b)),
            is(Some(SUBRIP_MIME), "SRT subtitles")
        );
    }

    #[test]
    fn nfo_is_not_a_candidate() {
        let d = Dir::new("nfo");
        let p = d.file("x.nfo", CP1250);
        assert_eq!(kind(&p), is(None, ".nfo file"));
        let p = d.file("Y.NFO", CP1250);
        assert_eq!(kind(&p), is(None, ".nfo file"));
        // A UTF-8 one stays plain text, as before.
        let p = d.file("z.nfo", "ő\n".as_bytes());
        assert_eq!(kind(&p), is(Some("text/plain"), "Plain text"));
    }

    #[test]
    fn no_extension_is_a_candidate() {
        let d = Dir::new("noext");
        for n in ["README", "notes", ".hidden", "foo."] {
            let p = d.file(n, CP1250);
            assert_eq!(kind(&p), x8bit(), "{n}");
        }
        // Without a text extension the head alone decides: no read-on.
        let mut r = Reread::per_drop();
        let p = d.file("LATE", &late(64 * 1024, Some(40 * 1024), ACCENT));
        assert_eq!(kind_with(&p, &mut r), is(Some("text/plain"), "Plain text"));
        assert_eq!(r.left, PER_DROP);
    }

    #[test]
    fn unknown_extension_is_not_a_candidate() {
        let d = Dir::new("unk");
        for n in ["a.rs", "b.bin", "c.html", "d.dat", "e.srtx", "f.txt.bak"] {
            let p = d.file(n, CP1250);
            let e = Path::new(n).extension().unwrap().to_str().unwrap();
            assert_eq!(kind(&p), is(None, &format!(".{e} file")), "{n}");
        }
        // Nor is it read on.
        let mut r = Reread::per_drop();
        let p = d.file("x.rs", &late(64 * 1024, Some(40 * 1024), ACCENT));
        assert_eq!(kind_with(&p, &mut r), is(None, ".rs text"));
        assert_eq!(r.left, PER_DROP);
    }

    #[test]
    fn reread_never_follows_a_fifo_or_symlink_swap() {
        let d = Dir::new("swap");
        let b = late(100 * 1024, Some(80 * 1024), ACCENT);
        // Opened, then the name becomes a FIFO: the read-on stays on the
        // file that was opened and never blocks.
        let p = d.file("a.srt", &b);
        let f = open_regular(&p).unwrap().unwrap();
        let fifo = d.0.join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        std::fs::rename(&fifo, &p).unwrap();
        let k = classify_open(f, &p, &mut Reread::per_drop()).unwrap();
        assert_eq!(
            k,
            Classified::File(Kind {
                mime: Some(X8BIT_MIME.into()),
                label: "8-bit text".into()
            })
        );
        assert_eq!(
            classify(&p).unwrap(),
            Classified::NotRegular(Shape::Special)
        );
        // Opened, then the name becomes a link to an all-ASCII file.
        let p = d.file("b.srt", &b);
        let f = open_regular(&p).unwrap().unwrap();
        let ascii = d.file("ascii.txt", &late(100 * 1024, None, 0));
        let tmp = d.0.join("link");
        std::os::unix::fs::symlink(&ascii, &tmp).unwrap();
        std::fs::rename(&tmp, &p).unwrap();
        let k = classify_open(f, &p, &mut Reread::per_drop()).unwrap();
        assert_eq!(
            k,
            Classified::File(Kind {
                mime: Some(X8BIT_MIME.into()),
                label: "8-bit text".into()
            })
        );
        assert_eq!(kind(&p), is(Some(SUBRIP_MIME), "SRT subtitles"));
    }

    #[test]
    fn cbz_zip_is_a_comic_book() {
        let d = Dir::new("cbz");
        for n in ["a.cbz", "B.CBZ", "c.Cbz"] {
            let p = d.file(n, b"PK\x03\x04rest");
            assert_eq!(
                kind(&p),
                is(Some("application/vnd.comicbook+zip"), "Comic book"),
                "{n}"
            );
        }
    }

    #[test]
    fn zip_without_known_extension_has_no_type() {
        let d = Dir::new("zipext");
        for n in ["a.zip", "a.cbr", "a", "a.cbz.zip", "a.epub.zip", "a.epubx"] {
            let p = d.file(n, b"PK\x03\x04rest");
            assert_eq!(kind(&p), is(None, "ZIP"), "{n}");
        }
    }

    #[test]
    fn epub_zip_is_an_epub() {
        let d = Dir::new("epub");
        for n in ["a.epub", "B.EPUB", "c.Epub"] {
            let p = d.file(n, b"PK\x03\x04rest");
            assert_eq!(kind(&p), is(Some("application/epub+zip"), "EPUB"), "{n}");
        }
    }

    #[test]
    fn epub_extension_without_zip_magic_is_not_an_epub() {
        let d = Dir::new("epubfake");
        let p = d.file("notes.epub", b"just words\n");
        assert_eq!(kind(&p), is(None, ".epub text"));
        let p = d.file("rar.epub", b"Rar!\x1a\x07\x00\x01\x02");
        assert_eq!(kind(&p), is(None, ".epub file"));
    }

    #[test]
    fn cbz_extension_without_zip_magic_is_not_a_comic_book() {
        let d = Dir::new("cbzfake");
        let p = d.file("notes.cbz", b"just words\n");
        assert_eq!(kind(&p), is(None, ".cbz text"));
        let p = d.file("rar.cbz", b"Rar!\x1a\x07\x00\x01\x02");
        assert_eq!(kind(&p), is(None, ".cbz file"));
    }

    #[test]
    fn plain_aliases_and_no_extension() {
        let d = Dir::new("plain");
        for n in [
            "a.TXT", "README", "x.nfo", "b.text", "c.log", "d.1st", "e.me", "f.readme", "g.diz",
        ] {
            let p = d.file(n, b"hello\n");
            assert_eq!(kind(&p), is(Some("text/plain"), "Plain text"), "{n}");
        }
    }

    #[test]
    fn markdown_aliases() {
        let d = Dir::new("md");
        for n in ["a.md", "b.markdown", "c.MDOWN", "d.mkd"] {
            let p = d.file(n, b"# hi\n");
            assert_eq!(kind(&p), is(Some("text/markdown"), "Markdown"), "{n}");
        }
    }

    #[test]
    fn srt_with_arrow_is_subrip() {
        let d = Dir::new("srt");
        let p = d.file("a.srt", b"1\r\n00:00:01,000 --> 00:00:02,000\r\nHi\r\n");
        assert_eq!(kind(&p), is(Some(SUBRIP_MIME), "SRT subtitles"));
        let p = d.file(
            "b.SRT",
            b"\xef\xbb\xbf1\n00:00:01,000 --> 00:00:02,000\nHi\n",
        );
        assert_eq!(kind(&p), is(Some(SUBRIP_MIME), "SRT subtitles"));
        // The arrow counts anywhere in the 4 KiB head.
        let mut b = "x\n".repeat(1000).into_bytes();
        b.extend_from_slice(b"00:00:01,000 --> 00:00:02,000\n");
        let p = d.file("c.srt", &b);
        assert_eq!(kind(&p), is(Some(SUBRIP_MIME), "SRT subtitles"));
    }

    #[test]
    fn srt_without_arrow_stays_unknown_text() {
        let d = Dir::new("srt-no");
        let p = d.file("a.srt", b"just some text\n");
        assert_eq!(kind(&p), is(None, ".srt text"));
        // An arrow only past the head does not count: the head decides.
        let mut b = "x\n".repeat(3000).into_bytes();
        b.extend_from_slice(b"00:00:01,000 --> 00:00:02,000\n");
        let p = d.file("b.srt", &b);
        assert_eq!(kind(&p), is(None, ".srt text"));
    }

    #[test]
    fn vtt_with_signature_is_webvtt() {
        let d = Dir::new("vtt");
        let p = d.file("a.vtt", b"WEBVTT\n\n00:00.000 --> 00:01.000\nHi\n");
        assert_eq!(kind(&p), is(Some(WEBVTT_MIME), "WebVTT subtitles"));
        let p = d.file(
            "b.VTT",
            b"\xef\xbb\xbfWEBVTT\n\n00:00.000 --> 00:01.000\nHi\n",
        );
        assert_eq!(kind(&p), is(Some(WEBVTT_MIME), "WebVTT subtitles"));
    }

    #[test]
    fn vtt_without_signature_stays_unknown_text() {
        let d = Dir::new("vtt-no");
        let p = d.file("a.vtt", b"00:00.000 --> 00:01.000\nHi\n");
        assert_eq!(kind(&p), is(None, ".vtt text"));
        // The signature must open the file: a leading line or space defeats it.
        let p = d.file("b.vtt", b"\nWEBVTT\n");
        assert_eq!(kind(&p), is(None, ".vtt text"));
        let p = d.file("c.vtt", b" WEBVTT\n");
        assert_eq!(kind(&p), is(None, ".vtt text"));
    }

    #[test]
    fn ass_and_ssa_with_script_info_are_ssa() {
        let d = Dir::new("ass");
        for n in ["a.ass", "b.ASS", "c.ssa", "d.SSA"] {
            let p = d.file(n, b"[Script Info]\nTitle: x\n\n[Events]\n");
            assert_eq!(kind(&p), is(Some(SSA_MIME), "ASS subtitles"), "{n}");
        }
        let p = d.file("e.ass", b"[SCRIPT info]\r\n");
        assert_eq!(kind(&p), is(Some(SSA_MIME), "ASS subtitles"));
    }

    #[test]
    fn ass_without_script_info_stays_unknown_text() {
        let d = Dir::new("ass-no");
        let p = d.file("a.ass", b"[Events]\nDialogue: 0\n");
        assert_eq!(kind(&p), is(None, ".ass text"));
        let p = d.file("b.ssa", b"Script Info\n");
        assert_eq!(kind(&p), is(None, ".ssa text"));
    }

    #[test]
    fn non_utf8_subtitle_is_still_8bit() {
        let d = Dir::new("sub8");
        let mut srt = b"1\n00:00:01,000 --> 00:00:02,000\n".to_vec();
        srt.extend_from_slice(CP1250);
        let p = d.file("a.srt", &srt);
        assert_eq!(kind(&p), x8bit());
        let mut vtt = b"WEBVTT\n\n".to_vec();
        vtt.extend_from_slice(ISO);
        let p = d.file("b.vtt", &vtt);
        assert_eq!(kind(&p), x8bit());
        let mut ass = b"[Script Info]\n".to_vec();
        ass.extend_from_slice(CP1250);
        let p = d.file("c.ass", &ass);
        assert_eq!(kind(&p), x8bit());
    }

    #[test]
    fn subtitle_names_on_binaries_are_not_subtitles() {
        let d = Dir::new("sub-bin");
        // A PNG keeps its type whatever its name and whatever bytes follow.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(b"00:00:01,000 --> 00:00:02,000 WEBVTT [Script Info]");
        let p = d.file("a.srt", &png);
        assert_eq!(kind(&p), is(Some("image/png"), "PNG"));
        // A NUL makes the file not text, so no subtitle type either.
        let p = d.file("b.vtt", b"WEBVTT\0binary");
        assert_eq!(kind(&p), is(None, ".vtt file"));
        let p = d.file("c.ass", b"[Script Info]\0binary");
        assert_eq!(kind(&p), is(None, ".ass file"));
    }

    #[test]
    fn txt_and_md_are_unchanged() {
        let d = Dir::new("txt-sub");
        let p = d.file("a.txt", b"00:00:01,000 --> 00:00:02,000\n");
        assert_eq!(kind(&p), is(Some("text/plain"), "Plain text"));
        let p = d.file("b.txt", b"WEBVTT\n");
        assert_eq!(kind(&p), is(Some("text/plain"), "Plain text"));
        let p = d.file("c.md", b"[Script Info]\n");
        assert_eq!(kind(&p), is(Some("text/markdown"), "Markdown"));
    }

    #[test]
    fn utf8_cut_at_4096_is_text() {
        let d = Dir::new("cut");
        let mut b = vec![b'a'; 4095];
        b.extend_from_slice("€".as_bytes()); // bytes 4095..4098, cut after the first
        let p = d.file("x.txt", &b);
        assert_eq!(kind(&p), is(Some("text/plain"), "Plain text"));
    }

    #[test]
    fn nul_is_not_text() {
        let d = Dir::new("nul");
        let p = d.file("x.txt", b"ab\0cd");
        assert_eq!(kind(&p), is(None, ".txt file"));
        let p = d.file("noext", b"ab\0cd");
        assert_eq!(kind(&p), is(None, "file"));
    }

    #[test]
    fn invalid_utf8_is_not_text() {
        let d = Dir::new("bad");
        let p = d.file("x.bin", b"ab\xffcd");
        assert_eq!(kind(&p), is(None, ".bin file"));
        // A bad sequence in the middle is not forgiven just because the
        // buffer also ends in a cut one.
        let mut b = b"\xc3\x28".to_vec();
        b.extend(std::iter::repeat_n(b'a', 4094));
        let p = d.file("y.bin", &b);
        assert_eq!(kind(&p), is(None, ".bin file"));
        // Under a text name the same bytes are an 8-bit candidate.
        let p = d.file("y.txt", &b);
        assert_eq!(kind(&p), x8bit());
    }

    #[test]
    fn magic_beats_extension() {
        let d = Dir::new("magic");
        let p = d.file("kép.txt", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
        assert_eq!(kind(&p), is(Some("image/png"), "PNG"));
        let cases: [(&str, &[u8], Option<&str>, &str); 5] = [
            ("a.bin", b"%PDF-1.7\n", None, "PDF"),
            ("b.bin", b"\xff\xd8\xff\xe0", Some("image/jpeg"), "JPEG"),
            ("c.bin", b"GIF89a..", None, "GIF"),
            ("d.bin", b"PK\x03\x04..", None, "ZIP"),
            ("e.bin", b"\x1f\x8b\x08", None, "gzip"),
        ];
        for (n, b, m, l) in cases {
            let p = d.file(n, b);
            assert_eq!(kind(&p), is(m, l), "{n}");
        }
        let p = d.file("f.bin", b"GIF87a");
        assert_eq!(kind(&p), is(None, "GIF"));
    }

    #[test]
    fn test_magic() {
        let d = Dir::new("tm");
        let p = d.file("t.dat", b"INACIRCLE-TEST\nbody");
        assert_eq!(kind(&p), is(Some("application/x-inacircle-test"), "test"));
    }

    #[test]
    fn other_text_extension() {
        let d = Dir::new("rs");
        let p = d.file("x.rs", b"fn main() {}\n");
        assert_eq!(kind(&p), is(None, ".rs text"));
    }

    #[test]
    fn unknown_binary_and_empty() {
        let d = Dir::new("bin");
        let p = d.file("x.bin", b"\x00\x01\x02");
        assert_eq!(kind(&p), is(None, ".bin file"));
        // An empty file has no NUL and is valid UTF-8: text by extension.
        let p = d.file("e.txt", b"");
        assert_eq!(kind(&p), is(Some("text/plain"), "Plain text"));
    }

    #[test]
    fn directory_and_fifo_and_broken_link_are_not_regular() {
        let d = Dir::new("nr");
        let sub = d.0.join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(
            classify(&sub).unwrap(),
            Classified::NotRegular(Shape::Folder)
        );

        let fifo = d.0.join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path that outlives the call.
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(rc, 0);
        // Must return without blocking (no open/read on a FIFO).
        assert_eq!(
            classify(&fifo).unwrap(),
            Classified::NotRegular(Shape::Special)
        );

        let link = d.0.join("broken");
        std::os::unix::fs::symlink(d.0.join("missing"), &link).unwrap();
        assert_eq!(
            classify(&link).unwrap(),
            Classified::NotRegular(Shape::BrokenLink)
        );
    }

    #[test]
    fn missing_file_is_an_error() {
        let d = Dir::new("miss");
        assert!(classify(&d.0.join("nope")).is_err());
    }

    #[test]
    fn symlink_to_file_follows() {
        let d = Dir::new("link");
        let t = d.file("real.md", b"# x");
        let l = d.0.join("alias.bin");
        std::os::unix::fs::symlink(&t, &l).unwrap();
        // Content from the target; extension from the link name.
        assert_eq!(kind(&l), is(None, ".bin text"));
        let l2 = d.0.join("alias.md");
        std::os::unix::fs::symlink(&t, &l2).unwrap();
        assert_eq!(kind(&l2), is(Some("text/markdown"), "Markdown"));
    }

    #[test]
    fn nul_beyond_the_head_is_still_text() {
        let d = Dir::new("far-nul");
        let mut b = vec![b'a'; 5000];
        b[4500] = 0;
        let p = d.file("x.txt", &b);
        assert_eq!(kind(&p), is(Some("text/plain"), "Plain text"));
    }

    #[test]
    fn cut_sequence_at_end_of_short_file_is_not_text() {
        let d = Dir::new("short-cut");
        let mut b = b"abc".to_vec();
        b.extend_from_slice(&"€".as_bytes()[..2]);
        let p = d.file("x.rs", &b);
        assert_eq!(kind(&p), is(None, ".rs file"));
        // A text name makes it an 8-bit candidate (`â‚` in Windows-1250).
        let p = d.file("x.txt", &b);
        assert_eq!(kind(&p), x8bit());
        // A short file is whole: a spent read-on budget changes nothing.
        let mut r = Reread { left: 0 };
        assert_eq!(kind_with(&p, &mut r), x8bit());
    }

    /// The read-on stops at the window's end: a valid sequence cut there
    /// (`é` across the last byte of the window and the first one past it)
    /// is not judged, so the file stays what its UTF-8 prefix says.
    #[test]
    fn sequence_cut_by_the_window_is_not_judged() {
        let d = Dir::new("window-cut");
        let mut b = late(300 * 1024, None, 0);
        b[WINDOW - 1..WINDOW + 1].copy_from_slice("é".as_bytes());
        let mut r = Reread::per_drop();
        assert_eq!(
            kind_with(&d.file("x.srt", &b), &mut r),
            is(Some(SUBRIP_MIME), "SRT subtitles")
        );
        assert_eq!(r.left, PER_DROP - (WINDOW - HEAD) as u64);
    }

    /// A read error during the read-on is the caller's error, never retried:
    /// only `Interrupted` is. A handle opened for writing only fails every
    /// read with EBADF; a retry would spin for ever, hence the deadline.
    #[test]
    fn read_on_error_is_returned() {
        let d = Dir::new("read-on-error");
        let p = d.file("x.srt", b"");
        let head = late(HEAD, None, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut file = File::create(&p).unwrap();
            let mut scan = Scan::default();
            let cut = scan.utf8(&head);
            let mut r = Reread::per_drop();
            let got = eight_bit(&mut file, &head, Some("srt"), scan, cut, &mut r);
            tx.send(got.map_err(|e| e.raw_os_error())).unwrap();
        });
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the read-on did not return");
        assert_eq!(got, Err(Some(libc::EBADF)));
    }

    #[test]
    fn hidden_file_and_trailing_dot_have_no_extension() {
        let d = Dir::new("hidden");
        let p = d.file(".bashrc", b"export A=1\n");
        assert_eq!(kind(&p), is(Some("text/plain"), "Plain text"));
        let p = d.file("foo.", b"text\n");
        assert_eq!(kind(&p), is(Some("text/plain"), "Plain text"));
        let p = d.file("bar.", b"\x01\0");
        assert_eq!(kind(&p), is(None, "file"));
    }

    #[test]
    fn symlink_loop_is_not_regular() {
        let d = Dir::new("loop");
        let (a, b) = (d.0.join("a"), d.0.join("b"));
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        assert!(matches!(classify(&a).unwrap(), Classified::NotRegular(_)));
    }

    #[test]
    fn unreadable_regular_file_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: getuid cannot fail.
        if unsafe { libc::getuid() } == 0 {
            return; // root reads anything
        }
        let d = Dir::new("perm");
        let p = d.file("secret.txt", b"x");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        let e = classify(&p).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }
}
