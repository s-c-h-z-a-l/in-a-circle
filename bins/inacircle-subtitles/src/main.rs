//! `inacircle-subtitles`: the subtitle family. An SRT, WebVTT or ASS/SSA file
//! becomes an SRT or a WebVTT file (`crates/subtitles` reads and writes them).
//! A cue that cannot be used is skipped and counted, and what the target
//! cannot carry is reported as `warn`. Contract v0 (docs/architecture.md).

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::process::ExitCode;

use action::{Command, Events, Invocation, parse_args};
use subtitles::{Cue, Loss, Parsed, ReadError};

/// Bytes in one MiB, for the size copy of the messages.
const MIB: u64 = 1 << 20;

/// The format of an input.
#[derive(Clone, Copy)]
enum Format {
    Srt,
    Vtt,
    Ass,
}

impl Format {
    fn read(self, bytes: &[u8]) -> Result<Parsed, ReadError> {
        match self {
            Format::Srt => subtitles::read_srt(bytes),
            Format::Vtt => subtitles::read_vtt(bytes),
            Format::Ass => subtitles::read_ass(bytes),
        }
    }
}

/// The format of an output.
#[derive(Clone, Copy)]
enum Target {
    Srt,
    Vtt,
}

impl Target {
    fn ext(self) -> &'static str {
        match self {
            Target::Srt => "srt",
            Target::Vtt => "vtt",
        }
    }

    /// The writer's losses (counted in cues); the caller adds them to the reader's.
    fn write(self, cues: &[Cue], out: &mut impl Write) -> io::Result<BTreeMap<Loss, usize>> {
        match self {
            Target::Srt => subtitles::write_srt(cues, out),
            Target::Vtt => subtitles::write_vtt(cues, out),
        }
    }
}

/// One action: the format it reads and the format it writes.
#[derive(Clone, Copy)]
struct Conversion {
    from: Format,
    to: Target,
}

impl Conversion {
    fn from_id(id: &str) -> Option<Conversion> {
        let (from, to) = match id {
            "subtitles.srt-to-vtt" => (Format::Srt, Target::Vtt),
            "subtitles.vtt-to-srt" => (Format::Vtt, Target::Srt),
            "subtitles.ass-to-srt" => (Format::Ass, Target::Srt),
            "subtitles.ass-to-vtt" => (Format::Ass, Target::Vtt),
            _ => return None,
        };
        Some(Conversion { from, to })
    }
}

fn manifest_text() -> String {
    let mut s = manifest::header("inacircle-manifest", 0);
    // (id, input MIME, output MIME, ext, label of the output).
    for (id, input, out, ext, label) in [
        (
            "subtitles.srt-to-vtt",
            "application/x-subrip",
            "text/vtt",
            "vtt",
            "VTT",
        ),
        (
            "subtitles.vtt-to-srt",
            "text/vtt",
            "application/x-subrip",
            "srt",
            "SRT",
        ),
        (
            "subtitles.ass-to-srt",
            "text/x-ssa",
            "application/x-subrip",
            "srt",
            "SRT",
        ),
        (
            "subtitles.ass-to-vtt",
            "text/x-ssa",
            "text/vtt",
            "vtt",
            "VTT",
        ),
    ] {
        s.push_str(&manifest::write_line(
            &manifest::Line::new("action")
                .bare(id)
                .pair("family", "subtitles")
                .pair("in", input)
                .pair("out", out)
                .pair("ext", ext)
                .pair("label", label)
                .pair("group", "subtitles")
                .pair("cost", "10")
                // A drawing is dropped and a speaker's name becomes text, so
                // the text can change.
                .pair("lossy", "yes")
                .pair("batch", "yes"),
        ));
    }
    s
}

/// What became of one input; the batch rule of the exit code counts them.
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
    // Read whole, and refused past the reader's own input limit before any
    // parsing: `read_file_bounded` reads at most that many bytes plus one.
    let bytes = match action::read_file_bounded(input, subtitles::INPUT_BYTES) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            ev.fail(input, &ReadError::TooLarge.to_string());
            return Outcome::Failed;
        }
        Err(e) => {
            ev.fail(input, &e.to_string());
            return Outcome::Failed;
        }
    };
    // Parsed before the output exists: an input nobody can read leaves nothing.
    let mut parsed = match conv.from.read(&bytes) {
        Ok(p) => p,
        Err(e) => {
            ev.fail(input, &e.to_string());
            return Outcome::Failed;
        }
    };
    subtitles::sort_cues(&mut parsed.cues);
    let out = match action::output_path(&inv.work, i, input, conv.to.ext()) {
        Ok(p) => p,
        Err(e) => {
            ev.fail(input, &format!("cannot create output: {e}"));
            return Outcome::Failed;
        }
    };
    match write_output(conv.to, &parsed.cues, &out) {
        Ok(writer_losses) => {
            ev.done(input, out.strip_prefix(&inv.work).unwrap_or(&out));
            for message in warnings(&parsed, writer_losses) {
                ev.warn(input, &message);
            }
            Outcome::Done
        }
        Err(reason) => {
            ev.fail(input, &reason);
            Outcome::Failed
        }
    }
}

/// Writes `cues` to `out` as `to` writes them, through a ceiling of
/// [`subtitles::OUTPUT_BYTES`]. The file is created new, so an existing one
/// is never overwritten; any error after that removes it. Returns the
/// writer's losses, or the `fail` reason.
fn write_output(to: Target, cues: &[Cue], out: &Path) -> Result<BTreeMap<Loss, usize>, String> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)
        .map_err(|e| format!("cannot create output: {e}"))?;
    // SIGXFSZ would kill the process (and the batch) at the first byte past
    // `RLIMIT_FSIZE`; ignored, that write fails with EFBIG and only this input
    // fails. The engine's ceiling (16 x the input, at least 256 MiB) is above
    // the output ceiling, so this is a net behind `Capped`.
    // SAFETY: `signal` with SIG_IGN installs no handler code and touches no
    // Rust state; SIGXFSZ is a valid signal number.
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
    }
    let mut w = BufWriter::new(Capped::new(file, subtitles::OUTPUT_BYTES));
    let written = to
        .write(cues, &mut w)
        .and_then(|losses| w.flush().map(|()| losses));
    match written {
        Ok(losses) => Ok(losses),
        Err(e) => {
            drop(w);
            let _ = fs::remove_file(out);
            Err(output_error(&e))
        }
    }
}

/// The `fail` reason of a failed write: past the ceiling (ours, or the
/// process's `RLIMIT_FSIZE` through EFBIG) the size copy; any other write
/// error keeps its own text.
fn output_error(e: &io::Error) -> String {
    let too_big = e.raw_os_error() == Some(libc::EFBIG)
        || e.get_ref().is_some_and(|inner| inner.is::<OverCap>());
    if too_big {
        format!(
            "subtitle output would be larger than {} MiB",
            subtitles::OUTPUT_BYTES / MIB
        )
    } else {
        format!("cannot write output: {e}")
    }
}

/// A writer that refuses to pass `cap` bytes in total. The write that would
/// pass it is refused whole, so the bytes that reach the file never exceed
/// the cap.
struct Capped<W> {
    inner: W,
    cap: u64,
    written: u64,
}

impl<W> Capped<W> {
    fn new(inner: W, cap: u64) -> Capped<W> {
        Capped {
            inner,
            cap,
            written: 0,
        }
    }
}

/// The error of a [`Capped`] write past its ceiling.
#[derive(Debug)]
struct OverCap;

impl fmt::Display for OverCap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("output over its size ceiling")
    }
}

impl std::error::Error for OverCap {}

impl<W: Write> Write for Capped<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // `written` never passes `cap`, so the room is never negative.
        let room = self.cap - self.written;
        if buf.len() as u64 > room {
            return Err(io::Error::other(OverCap));
        }
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The `warn` lines of one converted input: the skipped cues first (the count
/// and the first one's line and reason), then one line per kind of loss, in
/// `Loss` order, counted in cues. The reader's and the writer's counts of a
/// kind are summed. At most eight lines (seven kinds and the skipped line),
/// under the engine's cap of 16 `warn` per input.
fn warnings(parsed: &Parsed, writer: BTreeMap<Loss, usize>) -> Vec<String> {
    let mut lines = Vec::new();
    let skipped = &parsed.skipped;
    if skipped.count > 0 {
        lines.push(match skipped.first {
            Some((line, why)) => format!(
                "{} items skipped (first at line {line}: {why})",
                skipped.count
            ),
            None => format!("{} items skipped", skipped.count),
        });
    }
    let mut losses = parsed.losses.clone();
    for (loss, n) in writer {
        *losses.entry(loss).or_insert(0) += n;
    }
    for (loss, n) in losses {
        lines.push(format!("{} ({n} cues)", loss.message()));
    }
    lines
}

fn run(inv: &Invocation, conv: Conversion) -> u8 {
    if !inv.work.is_dir() {
        eprintln!("work directory does not exist: {}", inv.work.display());
        return action::EXIT_FAILED as u8;
    }
    let mut ev = Events::new(io::stdout().lock());
    let n = inv.inputs.len();
    // The `progress 0` line before any input work: the first line the engine
    // reads, which marks the child's entry for the measurement log.
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
            eprintln!("inacircle-subtitles: {msg}");
            return ExitCode::from(action::EXIT_USAGE as u8);
        }
    };
    let Some(conv) = Conversion::from_id(&inv.action) else {
        eprintln!("inacircle-subtitles: unknown action {}", inv.action);
        return ExitCode::from(action::EXIT_USAGE as u8);
    };
    ExitCode::from(run(&inv, conv))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ceiling is exact: writes that fill it are accepted, one byte more
    /// is refused, and nothing past the ceiling reaches the inner writer.
    #[test]
    fn capped_accepts_up_to_the_ceiling_and_no_more() {
        let mut w = Capped::new(Vec::new(), 10);
        w.write_all(&[1; 6]).unwrap();
        w.write_all(&[2; 4]).unwrap();
        let e = w.write_all(&[3]).unwrap_err();
        assert!(e.get_ref().is_some_and(|x| x.is::<OverCap>()));
        assert_eq!(w.inner.len(), 10);
    }

    /// A write that would pass the ceiling is refused whole, even when part of
    /// it would fit: no partial write leaves a truncated file behind.
    #[test]
    fn capped_refuses_a_write_that_crosses_the_ceiling_whole() {
        let mut w = Capped::new(Vec::new(), 10);
        w.write_all(&[1; 8]).unwrap();
        assert!(w.write_all(&[2; 3]).is_err());
        assert_eq!(w.inner.len(), 8);
    }

    /// Reader and writer counts of one kind are summed, skipped cues come
    /// first, and the kinds follow the `Loss` order whatever order they came in.
    #[test]
    fn warnings_sum_reader_and_writer_counts_in_loss_order() {
        let parsed = Parsed {
            cues: Vec::new(),
            losses: BTreeMap::from([(Loss::Karaoke, 2), (Loss::Styling, 1)]),
            skipped: subtitles::Skipped {
                count: 3,
                first: Some((7, "bad timestamp")),
            },
        };
        let writer = BTreeMap::from([(Loss::VoiceNames, 5), (Loss::Styling, 4)]);
        assert_eq!(
            warnings(&parsed, writer),
            [
                "3 items skipped (first at line 7: bad timestamp)",
                "colours, fonts and sizes removed (5 cues)",
                "karaoke effects removed (2 cues)",
                "speaker names written as 'Name:' prefixes (5 cues)",
            ]
        );
    }
}
