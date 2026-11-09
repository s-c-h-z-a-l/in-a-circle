//! `inacircle-text`: the text family. Both directions are lossy and report
//! what they dropped (txt -> md may merge blank-line runs, md -> txt drops
//! structure). Contract v0 (docs/architecture.md).

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::UNIX_EPOCH;

use action::{Command, Events, Invocation, parse_args};
use doc::xhtml::{Anchors, Chapter, Mode};
use doc::{Dialect, Document, LimitError, Limits, Loss, ReadError};

/// The structure budgets of one conversion (`document_*` budgets, see
/// [`Limits::DEFAULT`] for the values and their reasons). The input is read
/// at most `input_bytes` (+1 to detect more); past any budget the input
/// fails (`fail`, nothing staged) and the batch goes on. The sandbox's
/// RLIMIT_DATA (>= 1 GiB) is the second net.
const LIMITS: Limits = Limits::DEFAULT;

/// Why a conversion stopped.
enum ConvertError {
    NotText,
    Limit(LimitError),
    /// Any other failure, as the `fail` reason.
    Other(String),
}

impl From<epub::BookError> for ConvertError {
    fn from(e: epub::BookError) -> Self {
        match e {
            epub::BookError::Limit(l) => ConvertError::Limit(l),
            other => ConvertError::Other(other.to_string()),
        }
    }
}

impl From<ReadError> for ConvertError {
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::NotText => ConvertError::NotText,
            ReadError::Limit(l) => ConvertError::Limit(l),
        }
    }
}

impl From<LimitError> for ConvertError {
    fn from(e: LimitError) -> Self {
        ConvertError::Limit(e)
    }
}

/// `n` with thousands separators (`524,288`), for the rejection copy.
fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A byte budget as MiB for a message (budgets are whole MiB; a fraction
/// would round down, never overstate the limit).
fn mib(bytes: usize) -> usize {
    bytes / (1024 * 1024)
}

/// The text of the input-size rejection, from the budget itself.
fn input_too_large() -> String {
    format!(
        "This text file is larger than {} MiB.",
        mib(LIMITS.input_bytes)
    )
}

/// The `fail` reason of a budget error (copies of the shipped budgets).
fn limit_reason(e: LimitError) -> String {
    match e {
        LimitError::Exceeded("input") => input_too_large(),
        LimitError::Exceeded("nodes") => format!(
            "This document has too many text elements (limit {}).",
            grouped(LIMITS.nodes)
        ),
        LimitError::Exceeded("owned text") => format!(
            "This document expands beyond the {} MiB text budget.",
            mib(LIMITS.text_bytes)
        ),
        LimitError::Exceeded("output") => format!(
            "The converted text would be larger than {} MiB.",
            mib(LIMITS.output_bytes)
        ),
        LimitError::Exceeded(other) => format!("This document passes the {other} limit."),
        LimitError::Allocation => "Not enough memory to convert this text file.".to_string(),
    }
}

fn manifest_text() -> String {
    let action = |id: &str, input: &str, out: &str, ext: &str, label: &str, group: &str| {
        manifest::write_line(
            &manifest::Line::new("action")
                .bare(id)
                .pair("family", "text")
                .pair("in", input)
                .pair("out", out)
                .pair("ext", ext)
                .pair("label", label)
                .pair("group", group)
                .pair("cost", "10")
                // Every action here can drop something (structure, markup,
                // images), so all are lossy.
                .pair("lossy", "yes")
                .pair("batch", "yes"),
        )
    };
    let mut s = manifest::header("inacircle-manifest", 0);
    s.push_str(&action(
        "text.txt-to-md",
        "text/plain",
        "text/markdown",
        "md",
        "Markdown",
        "document",
    ));
    s.push_str(&action(
        "text.md-to-txt",
        "text/markdown",
        "text/plain",
        "txt",
        "Plain text",
        "document",
    ));
    for (id, input) in [
        ("text.md-to-epub", "text/markdown"),
        ("text.txt-to-epub", "text/plain"),
    ] {
        s.push_str(&action(
            id,
            input,
            "application/epub+zip",
            "epub",
            "EPUB",
            "book",
        ));
    }
    for (id, input) in [
        ("text.md-to-html", "text/markdown"),
        ("text.txt-to-html", "text/plain"),
    ] {
        s.push_str(&action(id, input, "text/html", "html", "HTML", "document"));
    }
    s
}

#[derive(Clone, Copy)]
enum Conversion {
    TxtToMd,
    MdToTxt,
    MdToEpub,
    TxtToEpub,
    MdToHtml,
    TxtToHtml,
}

impl Conversion {
    fn from_id(id: &str) -> Option<Conversion> {
        match id {
            "text.txt-to-md" => Some(Conversion::TxtToMd),
            "text.md-to-txt" => Some(Conversion::MdToTxt),
            "text.md-to-epub" => Some(Conversion::MdToEpub),
            "text.txt-to-epub" => Some(Conversion::TxtToEpub),
            "text.md-to-html" => Some(Conversion::MdToHtml),
            "text.txt-to-html" => Some(Conversion::TxtToHtml),
            _ => None,
        }
    }

    fn ext(self) -> &'static str {
        match self {
            Conversion::TxtToMd => "md",
            Conversion::MdToTxt => "txt",
            Conversion::MdToEpub | Conversion::TxtToEpub => "epub",
            Conversion::MdToHtml | Conversion::TxtToHtml => "html",
        }
    }

    /// Whether the input is Markdown (else plain text) for a book or a page.
    fn reads_markdown(self) -> bool {
        matches!(self, Conversion::MdToEpub | Conversion::MdToHtml)
    }

    /// Converted text plus every loss from the reader and the writer.
    fn convert(self, bytes: &[u8]) -> Result<(String, BTreeSet<Loss>), ConvertError> {
        match self {
            Conversion::TxtToMd => {
                let (d, losses) = doc::txt::read_limited(bytes, LIMITS)?;
                let (text, more) = doc::md::write_limited(&d, LIMITS)?;
                Ok((text, merge_losses(losses, more)))
            }
            Conversion::MdToTxt => {
                let d = doc::md::read_limited_with(bytes, LIMITS, doc::Dialect::Gfm)?;
                Ok(doc::txt::write_limited(&d, LIMITS)?)
            }
            Conversion::MdToEpub
            | Conversion::TxtToEpub
            | Conversion::MdToHtml
            | Conversion::TxtToHtml => unreachable!("books and pages go through `publish`"),
        }
    }

    /// The parsed input of a book or a page, with the reader's losses.
    fn read(self, bytes: &[u8]) -> Result<(Document, BTreeSet<Loss>), ConvertError> {
        if self.reads_markdown() {
            let d = doc::md::read_limited_with(bytes, LIMITS, Dialect::Gfm)?;
            Ok((d, BTreeSet::new()))
        } else {
            Ok(doc::txt::read_limited(bytes, LIMITS)?)
        }
    }
}

/// The language of the book or page: the engine's `INACIRCLE_LANG`, kept
/// only when it is a well-formed tag, else `und` (undetermined). Fail
/// closed: the value lands in attributes, and readers choose fonts and
/// hyphenation by it.
fn lang() -> String {
    let v = std::env::var("INACIRCLE_LANG").unwrap_or_default();
    epub::lang_tag(&v).to_string()
}

/// The title of a book or page: Markdown's first H1, else (and always for
/// plain text) the file's stem. Never empty, so `<title>` has text.
fn title(conv: Conversion, d: &Document, input: &Path) -> String {
    let h1 = conv.reads_markdown().then(|| doc::book::title(d)).flatten();
    let t = h1.unwrap_or_else(|| {
        input
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let t: String = t.chars().filter(|&c| doc::xhtml::xml_char(c)).collect();
    let t = t.trim();
    if t.is_empty() { "Untitled" } else { t }.to_string()
}

/// The soft `RLIMIT_FSIZE` of this process, the largest file it may write
/// (the engine sets it to 16 x the input, 256 MiB to 16 GiB). `None` is
/// unlimited. A failing `getrlimit` is an error, so the input fails closed
/// instead of writing without a known ceiling. `getrlimit` on itself is
/// allowed by the sandbox's seccomp policy.
fn output_cap() -> Result<Option<u64>, ConvertError> {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rl` is a valid, writable `rlimit` for the whole call, and
    // RLIMIT_FSIZE is a valid resource; the call only writes into `rl`.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut rl) };
    if rc != 0 {
        return Err(ConvertError::Other(format!(
            "cannot read the output size limit: {}",
            io::Error::last_os_error()
        )));
    }
    Ok((rl.rlim_cur != libc::RLIM_INFINITY).then_some(rl.rlim_cur))
}

/// The copy for an output past the ceiling (N rounded down, so it never
/// overstates the limit; in KiB below 1 MiB, so it never reads "0 MiB").
/// If the ceiling cannot be read now, the generic copy.
fn too_large(what: &str) -> ConvertError {
    match output_cap() {
        Ok(Some(c)) if c >= 1 << 20 => {
            ConvertError::Other(format!("{what} would be larger than {} MiB", c >> 20))
        }
        Ok(Some(c)) => ConvertError::Other(format!("{what} would be larger than {} KiB", c >> 10)),
        _ => ConvertError::Other(format!("{what} would be too large")),
    }
}

/// Writes the EPUB or the HTML page of `d`, which `conv.read` made of
/// `bytes` (and lost `losses` doing so), to `out`; what was lost in all.
/// Whatever is left at `out` after an error is the caller's to remove.
fn publish(
    conv: Conversion,
    input: &Path,
    bytes: &[u8],
    (d, mut losses): (Document, BTreeSet<Loss>),
    out: &Path,
) -> Result<BTreeSet<Loss>, ConvertError> {
    let title = title(conv, &d, input);
    let lang = lang();
    let more = match conv {
        Conversion::MdToEpub | Conversion::TxtToEpub => {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(out)
                .map_err(|e| ConvertError::Other(format!("cannot create output: {e}")))?;
            // Seconds since the epoch; a time before it counts backwards.
            // No readable mtime: the epoch, so the book stays reproducible.
            let modified_unix =
                fs::metadata(input)
                    .and_then(|m| m.modified())
                    .map_or(0, |t| match t.duration_since(UNIX_EPOCH) {
                        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
                    });
            let meta = epub::Meta {
                title,
                lang,
                modified_unix,
                // The 16 bytes of the input's digest, not a copy of it.
                id_seed: epub::fnv1a_128(&[bytes]).to_be_bytes().to_vec(),
            };
            // SIGXFSZ would kill the process (and the batch) at the first
            // byte past `RLIMIT_FSIZE`; ignored, that write fails with
            // EFBIG instead and only this input fails. The size of a book
            // is known only once it is packed, so this is the ceiling's
            // one check; the file is cut back to empty by `epub::write`.
            // SAFETY: `signal` with SIG_IGN installs no handler code and
            // touches no Rust state; SIGXFSZ is a valid signal number.
            unsafe {
                libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            }
            epub::write(&d, &meta, file, LIMITS).map_err(|e| {
                if e.write_error().and_then(io::Error::raw_os_error) == Some(libc::EFBIG) {
                    too_large("EPUB")
                } else {
                    e.into()
                }
            })?
        }
        Conversion::MdToHtml | Conversion::TxtToHtml => {
            let ch = Chapter {
                blocks: &d.blocks,
                file: "index.html".to_string(),
            };
            let anchors = Anchors::build(std::slice::from_ref(&ch));
            let (page, more) = doc::xhtml::write_chapter(
                &ch,
                &anchors,
                &Mode::Standalone { lang, title },
                LIMITS,
            )?;
            // The page is whole in memory, so its exact size is checked
            // before the file exists on disk.
            if output_cap()?.is_some_and(|c| page.len() as u64 > c) {
                return Err(too_large("HTML"));
            }
            // Created new, like the book: an existing file is never
            // overwritten.
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(out)
                .map_err(|e| ConvertError::Other(format!("cannot create output: {e}")))?;
            file.write_all(page.as_bytes())
                .map_err(|e| ConvertError::Other(format!("cannot write output: {e}")))?;
            more
        }
        Conversion::TxtToMd | Conversion::MdToTxt => {
            unreachable!("text goes through `convert`")
        }
    };
    losses.extend(more);
    Ok(losses)
}

/// Union of the reader's and the writer's losses: a loss both report is one
/// loss, so it yields one `warn`. `Loss` order keeps the output stable.
fn merge_losses(mut reader: BTreeSet<Loss>, writer: BTreeSet<Loss>) -> BTreeSet<Loss> {
    reader.extend(writer);
    reader
}

enum Outcome {
    Done,
    Unsupported,
    Failed,
}

fn process(
    conv: Conversion,
    inv: &Invocation,
    i: usize,
    input: &Path,
    ev: &mut Events<impl Write>,
) -> Outcome {
    if input.to_str().is_none() {
        ev.skip(input, "non-UTF-8 path");
        return Outcome::Unsupported;
    }
    let bytes = match action::read_file_bounded(input, LIMITS.input_bytes) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            ev.fail(input, &input_too_large());
            return Outcome::Failed;
        }
        Err(e) => {
            ev.fail(input, &e.to_string());
            return Outcome::Failed;
        }
    };
    if !matches!(conv, Conversion::TxtToMd | Conversion::MdToTxt) {
        return document(conv, inv, i, input, &bytes, ev);
    }
    let (text, losses) = match conv.convert(&bytes) {
        Ok(r) => r,
        Err(ConvertError::NotText) => {
            ev.skip(input, "unsupported");
            return Outcome::Unsupported;
        }
        Err(ConvertError::Limit(e)) => {
            ev.fail(input, &limit_reason(e));
            return Outcome::Failed;
        }
        Err(ConvertError::Other(reason)) => {
            ev.fail(input, &reason);
            return Outcome::Failed;
        }
    };
    let out = match action::output_path(&inv.work, i, input, conv.ext()) {
        Ok(p) => p,
        Err(e) => {
            ev.fail(input, &format!("cannot create output: {e}"));
            return Outcome::Failed;
        }
    };
    if let Err(e) = fs::write(&out, text) {
        ev.fail(input, &format!("cannot write output: {e}"));
        return Outcome::Failed;
    }
    ev.done(input, out.strip_prefix(&inv.work).unwrap_or(&out));
    for loss in losses {
        ev.warn(input, loss.message());
    }
    Outcome::Done
}

/// One input to an EPUB or an HTML page. The output is removed on any
/// error, so a half book never stays for the engine to publish.
fn document(
    conv: Conversion,
    inv: &Invocation,
    i: usize,
    input: &Path,
    bytes: &[u8],
    ev: &mut Events<impl Write>,
) -> Outcome {
    // Parse (once) before the output exists: an input nobody reads leaves nothing.
    let read = match conv.read(bytes) {
        Ok(r) => r,
        Err(e) => {
            match e {
                ConvertError::NotText => {
                    ev.skip(input, "unsupported");
                    return Outcome::Unsupported;
                }
                ConvertError::Limit(l) => ev.fail(input, &limit_reason(l)),
                ConvertError::Other(r) => ev.fail(input, &r),
            }
            return Outcome::Failed;
        }
    };
    let out = match action::output_path(&inv.work, i, input, conv.ext()) {
        Ok(p) => p,
        Err(e) => {
            ev.fail(input, &format!("cannot create output: {e}"));
            return Outcome::Failed;
        }
    };
    match publish(conv, input, bytes, read, &out) {
        Ok(losses) => {
            ev.done(input, out.strip_prefix(&inv.work).unwrap_or(&out));
            for loss in losses {
                ev.warn(input, loss.message());
            }
            Outcome::Done
        }
        Err(e) => {
            let _ = fs::remove_file(&out);
            let reason = match e {
                ConvertError::Limit(l) => limit_reason(l),
                ConvertError::Other(r) => r,
                ConvertError::NotText => "not text".to_string(),
            };
            ev.fail(input, &reason);
            Outcome::Failed
        }
    }
}

fn run(inv: &Invocation, conv: Conversion) -> u8 {
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
        match process(conv, inv, i, input, &mut ev) {
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

fn main() -> ExitCode {
    let inv = match parse_args(std::env::args_os().skip(1)) {
        Ok(Command::Manifest) => {
            let _ = io::stdout().write_all(manifest_text().as_bytes());
            return ExitCode::SUCCESS;
        }
        Ok(Command::Run(inv)) => inv,
        Err(msg) => {
            eprintln!("inacircle-text: {msg}");
            return ExitCode::from(action::EXIT_USAGE as u8);
        }
    };
    let Some(conv) = Conversion::from_id(&inv.action) else {
        eprintln!("inacircle-text: unknown action {}", inv.action);
        return ExitCode::from(action::EXIT_USAGE as u8);
    };
    ExitCode::from(run(&inv, conv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_reported_by_reader_and_writer_is_one() {
        let r = BTreeSet::from([Loss::Whitespace, Loss::BlankLines]);
        let w = BTreeSet::from([Loss::Whitespace]);
        let merged: Vec<Loss> = merge_losses(r, w).into_iter().collect();
        assert_eq!(merged, [Loss::BlankLines, Loss::Whitespace]);
    }

    /// Thousands separators sit every three digits from the right, for
    /// lengths that are not a multiple of three too. Kills `-` -> `+`.
    #[test]
    fn grouped_puts_a_comma_every_three_digits_from_the_right() {
        for (n, want) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1,000"),
            (12_345, "12,345"),
            (524_288, "524,288"),
            (1_234_567, "1,234,567"),
            (12_345_678, "12,345,678"),
        ] {
            assert_eq!(grouped(n), want);
        }
    }
}
