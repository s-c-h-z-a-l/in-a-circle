//! `text.epub-to-txt`: one EPUB book (foreign input) to one plain-text
//! file. The book's skeleton is read by `epub::read`, then one spine item
//! at a time: its bytes, `xml::decode`, `doc::xhtml_read` under a fresh
//! [`LIMITS`] and the plain-text writer, appended to the output; the
//! chapter's tree is dropped before the next is read, so memory is one
//! chapter. A spine item that cannot be read is a line in its place,
//! `[Chapter N could not be read: <reason>]`; a book that cannot be read,
//! or that turns out to have no chapter or no text, leaves no file.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use action::{Events, Invocation};
use doc::xhtml_read::{self, XhtmlError};
use doc::{Block, Document, Inline, LimitError, Loss};
use epub::read::Book;
use xml::ErrorKind;

use crate::{ConvertError, LIMITS, Outcome, output_cap, too_large};

/// The spine item type that is a picture page (no text, an image loss).
const SVG_TYPE: &str = "image/svg+xml";

/// Longest encoding name a chapter's reason repeats: 40 characters, the
/// longest name the IANA charset registry allows (RFC 2978, section 2.3).
/// A longer declared name is no registered charset; it is cut, with
/// `...`, so a hostile declaration cannot fill the book with its name.
const MAX_ENCODING_NAME: usize = 40;

/// Open the input as a regular file without ever waiting (`O_NONBLOCK`:
/// a FIFO would block a plain open until a writer shows up).
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

/// One book to `<stem>.txt`. Nothing is created before the skeleton
/// reads; on any later failure the file is cut back to empty and removed,
/// so no half book is left for the engine to publish.
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
    // Every whole-book failure of the reader carries its user-facing text.
    let book = match epub::read::open(&file) {
        Ok(b) => b,
        Err(e) => {
            ev.fail(input, &e.to_string());
            return Outcome::Failed;
        }
    };
    let path = match action::output_path(&inv.work, i, input, "txt") {
        Ok(p) => p,
        Err(e) => {
            ev.fail(input, &format!("cannot create output: {e}"));
            return Outcome::Failed;
        }
    };
    // Created new: an existing file is never overwritten.
    let mut out = match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            ev.fail(input, &format!("cannot create output: {e}"));
            return Outcome::Failed;
        }
    };
    match convert(&book, &mut out) {
        Ok(done) => {
            ev.done(input, path.strip_prefix(&inv.work).unwrap_or(&path));
            // One counted line for every chapter that could not be read,
            // then each loss once: at most 1 + 14 kinds, under the
            // supervisor's 16 `warn` per input.
            if done.unread > 0 {
                ev.warn(
                    input,
                    &format!("{} chapter(s) could not be read", done.unread),
                );
            }
            for loss in done.losses {
                ev.warn(input, loss.message());
            }
            Outcome::Done
        }
        Err(reason) => {
            // Cut first: should the removal fail, an empty file is left,
            // never a half book.
            let _ = out.set_len(0);
            drop(out);
            let _ = fs::remove_file(&path);
            ev.fail(input, &reason);
            Outcome::Failed
        }
    }
}

/// What a converted book reports.
struct Converted {
    /// Spine items that could not be read (each has its line in place).
    unread: usize,
    losses: BTreeSet<Loss>,
}

/// The output file and what has been written to it, held to the text
/// family's output budget for the whole book and to the process's
/// `RLIMIT_FSIZE`, both checked before each write.
struct Sink<'a> {
    file: &'a mut File,
    written: usize,
    cap: Option<u64>,
}

impl Sink<'_> {
    fn write(&mut self, s: &str) -> Result<(), String> {
        let next = self.written.saturating_add(s.len());
        // The book's text is held to the output budget of one text file
        // (32 MiB): the whole book is one output.
        if next > LIMITS.output_bytes {
            return Err("book is too large".into());
        }
        if self.cap.is_some_and(|c| next as u64 > c) {
            return Err(reason(too_large("Plain text")));
        }
        self.file.write_all(s.as_bytes()).map_err(|e| {
            if e.raw_os_error() == Some(libc::EFBIG) {
                reason(too_large("Plain text"))
            } else {
                format!("cannot write output: {e}")
            }
        })?;
        self.written = next;
        Ok(())
    }
}

/// A [`ConvertError`] from the shared helpers as a `fail` reason.
fn reason(e: ConvertError) -> String {
    match e {
        ConvertError::Other(r) => r,
        ConvertError::Limit(l) => l.to_string(),
        ConvertError::NotText => "not text".into(),
    }
}

/// Writes the header and every spine item of `book` to `file`.
fn convert(book: &Book<'_>, file: &mut File) -> Result<Converted, String> {
    let cap = output_cap().map_err(reason)?;
    // The size of the text is known only chapter by chapter, so SIGXFSZ is
    // ignored as a net under the check in `Sink::write`: a write past
    // `RLIMIT_FSIZE` then fails with EFBIG instead of killing the process
    // (and the batch).
    // SAFETY: `signal` with SIG_IGN installs no handler code and touches
    // no Rust state; SIGXFSZ is a valid signal number.
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
    }
    let mut sink = Sink {
        file,
        written: 0,
        cap,
    };
    sink.write(&header(book))?;
    // Every book has styles, and none is read.
    let mut losses = BTreeSet::from([Loss::Styles]);
    let (mut unread, mut read, mut text, mut any) = (0usize, 0usize, false, false);
    // One chapter's bytes at a time, the buffer reused.
    let mut raw = Vec::new();
    for c in &book.chapters {
        let part = match book.chapter(c.index, &mut raw) {
            Ok(())
                if book
                    .item(c)
                    .is_some_and(|it| it.media_type.eq_ignore_ascii_case(SVG_TYPE)) =>
            {
                read += 1;
                losses.insert(Loss::Images);
                continue;
            }
            Ok(()) => match chapter(&raw) {
                Ok((mut d, more)) => {
                    read += 1;
                    text |= xhtml_read::has_text(&d);
                    losses.extend(more);
                    unlink_internal(&mut d);
                    let (s, more) = doc::txt::write_limited(&d, LIMITS).map_err(book_limit)?;
                    losses.extend(more);
                    s
                }
                Err(why) => {
                    unread += 1;
                    unreadable(c.index, &why)
                }
            },
            Err(e) => {
                unread += 1;
                unreadable(c.index, &e.reason())
            }
        };
        if part.is_empty() {
            continue;
        }
        // One blank line between chapters (each ends with its line feed).
        if any {
            sink.write("\n")?;
        }
        sink.write(&part)?;
        any = true;
    }
    if read == 0 {
        return Err("no chapter could be read".into());
    }
    if !text {
        return Err("book has no text".into());
    }
    Ok(Converted { unread, losses })
}

/// `<title>\n<author, author>\n\n`: what the book lacks is left out, and
/// with neither, the header and its blank line.
fn header(book: &Book<'_>) -> String {
    let mut h = String::new();
    if let Some(t) = &book.title {
        h.push_str(t);
        h.push('\n');
    }
    if !book.authors.is_empty() {
        h.push_str(&book.authors.join(", "));
        h.push('\n');
    }
    if !h.is_empty() {
        h.push('\n');
    }
    h
}

/// The line that stands in place of spine item `n`.
fn unreadable(n: usize, why: &str) -> String {
    format!("[Chapter {n} could not be read: {why}]\n")
}

/// The writer's budget error for one chapter: its text alone is past the
/// book's output budget, so the book is.
fn book_limit(e: LimitError) -> String {
    match e {
        LimitError::Exceeded(_) => "book is too large".into(),
        LimitError::Allocation => "not enough memory to convert this book".into(),
    }
}

/// One chapter's bytes as a document, with the reader's losses; or why
/// it cannot be read.
fn chapter(raw: &[u8]) -> Result<(Document, BTreeSet<Loss>), String> {
    let text = xml::decode(raw).map_err(|e| xml_reason(&e))?;
    xhtml_read::read_limited(&text, LIMITS).map_err(|e| match e {
        XhtmlError::Xml(e) => xml_reason(&e),
        XhtmlError::Limit(l) => limit_reason(l),
    })
}

/// The reason for an XML error: the line, not the detail, for a chapter
/// that is not well-formed; nesting past the reader's depth is a size
/// limit like the others.
fn xml_reason(e: &xml::Error) -> String {
    match &e.kind {
        ErrorKind::NotWellFormed(_) => format!("not well-formed XML (line {})", e.line),
        ErrorKind::TooDeep => "too large (nesting limit)".into(),
        ErrorKind::UnsupportedEncoding(name) => {
            let mut end = name.len().min(MAX_ENCODING_NAME);
            while !name.is_char_boundary(end) {
                end -= 1;
            }
            let more = if end < name.len() { "..." } else { "" };
            format!("unsupported encoding {}{more}", &name[..end])
        }
        ErrorKind::EntityDeclarations => "entity declarations are not supported".into(),
    }
}

/// The reason for a chapter's budget: the node and text budgets by name;
/// the input budget (a UTF-16 chapter grown past 8 MiB as UTF-8) and an
/// allocation the budget could not make are the chapter's size.
fn limit_reason(e: LimitError) -> String {
    match e {
        LimitError::Exceeded("nodes") => "too large (nodes limit)".into(),
        LimitError::Exceeded("owned text") => "too large (text limit)".into(),
        LimitError::Exceeded(_) | LimitError::Allocation => "too large".into(),
    }
}

/// Whether `dest` starts with a URL scheme (RFC 3986, section 3.1: a
/// letter, then letters, digits, `+`, `-` or `.`, then `:`).
fn has_scheme(dest: &str) -> bool {
    let Some((scheme, _)) = dest.split_once(':') else {
        return false;
    };
    let mut cs = scheme.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphabetic())
        && cs.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// A link to a place in the book (no URL scheme: `notes.xhtml#fn1`, `#x`)
/// means nothing in a text file: its destination is dropped, so the
/// writer prints the text alone (still a links loss). A link with a
/// scheme keeps `text (destination)`. Walked with explicit stacks, no
/// recursion.
fn unlink_internal(d: &mut Document) {
    let mut blocks: Vec<&mut Vec<Block>> = vec![&mut d.blocks];
    let mut inlines: Vec<&mut Vec<Inline>> = Vec::new();
    while let Some(bs) = blocks.pop() {
        for b in bs {
            match b {
                Block::Paragraph(c) | Block::Heading { content: c, .. } => inlines.push(c),
                Block::BlockQuote(inner) => blocks.push(inner),
                Block::List { items, .. } => blocks.extend(items.iter_mut().map(|i| &mut i.blocks)),
                Block::Table { head, rows, .. } => {
                    inlines.extend(head.iter_mut());
                    inlines.extend(rows.iter_mut().flatten());
                }
                Block::CodeBlock { .. } | Block::ThematicBreak | Block::HtmlBlock(_) => {}
            }
        }
    }
    while let Some(is) = inlines.pop() {
        for i in is {
            match i {
                Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => inlines.push(c),
                Inline::Link {
                    dest,
                    content,
                    auto,
                    ..
                } => {
                    if !has_scheme(dest) {
                        dest.clear();
                        // Never the loss-free autolink form: the link is gone.
                        *auto = false;
                    }
                    inlines.push(content);
                }
                Inline::Image { alt, .. } => inlines.push(alt),
                Inline::Text(_)
                | Inline::Code(_)
                | Inline::SoftBreak
                | Inline::HardBreak
                | Inline::Html(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemes_per_rfc_3986() {
        for d in ["http://x", "https://x", "mailto:a@b", "a+b-c.d:x", "X:"] {
            assert!(has_scheme(d), "{d}");
        }
        for d in [
            "",
            "#x",
            "notes.xhtml#fn1",
            "../a.xhtml",
            ":x",
            "1a:x",
            "a b:x",
            "a/b:c",
        ] {
            assert!(!has_scheme(d), "{d}");
        }
    }

    /// Each budget a chapter can run out of has its own reason.
    #[test]
    fn chapter_budget_reasons() {
        assert_eq!(
            limit_reason(LimitError::Exceeded("nodes")),
            "too large (nodes limit)"
        );
        assert_eq!(
            limit_reason(LimitError::Exceeded("owned text")),
            "too large (text limit)"
        );
        assert_eq!(limit_reason(LimitError::Exceeded("input")), "too large");
        assert_eq!(limit_reason(LimitError::Allocation), "too large");
    }

    #[test]
    fn long_encoding_name_is_cut() {
        let e = |name: &str| xml::Error {
            kind: ErrorKind::UnsupportedEncoding(name.to_string()),
            line: 1,
        };
        let forty = "a".repeat(MAX_ENCODING_NAME);
        assert_eq!(
            xml_reason(&e(&forty)),
            format!("unsupported encoding {forty}")
        );
        assert_eq!(
            xml_reason(&e(&format!("{forty}b"))),
            format!("unsupported encoding {forty}...")
        );
    }

    /// Links nested in emphasis, lists, quotes and table cells are all
    /// reached; external ones keep their destination.
    #[test]
    fn internal_links_lose_their_destination_everywhere() {
        let link = |dest: &str| Inline::Link {
            dest: dest.into(),
            title: String::new(),
            content: vec![Inline::Text("t".into())],
            auto: true,
        };
        let para = |d: &str| Block::Paragraph(vec![Inline::Emph(vec![link(d)])]);
        let mut d = Document {
            blocks: vec![
                Block::BlockQuote(vec![para("#a")]),
                Block::List {
                    start: None,
                    tight: true,
                    items: vec![doc::ListItem {
                        checked: None,
                        blocks: vec![para("b.xhtml")],
                    }],
                },
                Block::Table {
                    align: vec![doc::Align::None],
                    head: vec![vec![link("#h")]],
                    rows: vec![vec![vec![Inline::Link {
                        dest: "https://x".into(),
                        title: String::new(),
                        content: vec![Inline::Text("t".into())],
                        auto: false,
                    }]]],
                },
            ],
        };
        unlink_internal(&mut d);
        let (s, l) = doc::txt::write(&d);
        assert_eq!(s, "> t\n\n- t\n\nt\n-------------\nt (https://x)\n");
        assert!(l.contains(&Loss::Links));
    }
}
