//! `inacircle-text`: the text family. Both directions are lossy and report
//! what they dropped (txt -> md may merge blank-line runs, md -> txt drops
//! structure). Contract v0 (docs/architecture.md).

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use action::{Command, Events, Invocation, parse_args};
use doc::{LimitError, Limits, Loss, ReadError};

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
    let action = |id: &str, input: &str, out: &str, ext: &str, label: &str, lossy: &str| {
        manifest::write_line(
            &manifest::Line::new("action")
                .bare(id)
                .pair("family", "text")
                .pair("in", input)
                .pair("out", out)
                .pair("ext", ext)
                .pair("label", label)
                .pair("group", "document")
                .pair("cost", "10")
                .pair("lossy", lossy)
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
        "yes",
    ));
    s.push_str(&action(
        "text.md-to-txt",
        "text/markdown",
        "text/plain",
        "txt",
        "Plain text",
        "yes",
    ));
    s
}

#[derive(Clone, Copy)]
enum Conversion {
    TxtToMd,
    MdToTxt,
}

impl Conversion {
    fn from_id(id: &str) -> Option<Conversion> {
        match id {
            "text.txt-to-md" => Some(Conversion::TxtToMd),
            "text.md-to-txt" => Some(Conversion::MdToTxt),
            _ => None,
        }
    }

    fn ext(self) -> &'static str {
        match self {
            Conversion::TxtToMd => "md",
            Conversion::MdToTxt => "txt",
        }
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
                let d = doc::md::read_limited(bytes, LIMITS)?;
                Ok(doc::txt::write_limited(&d, LIMITS)?)
            }
        }
    }
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
