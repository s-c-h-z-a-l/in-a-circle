//! Subtitle cues: the model the subtitle readers fill and the SRT and WebVTT
//! writers print. Standard library only, because the shipped subtitle binary
//! links it and the dependency guard allows nothing else.

use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

mod ass;
mod srt;
mod tag;
mod time;
mod vtt;
mod write;

pub use write::{write_srt, write_vtt};

/// Reads an SRT file as a media player would. Refused past [`INPUT_BYTES`]
/// before anything is parsed, refused when it is not UTF-8, refused past
/// [`ITEM_BUDGET`] items, and refused with [`ReadError::NoCues`] when no cue in
/// it is usable. A byte order mark is skipped. Everything else is read
/// leniently: see `srt.rs` for the rules.
pub fn read_srt(bytes: &[u8]) -> Result<Parsed, ReadError> {
    let text = text_of(bytes)?;
    let parsed = srt::parse(text, &mut Budget::new(ITEM_BUDGET))?;
    if parsed.cues.is_empty() {
        return Err(ReadError::NoCues);
    }
    Ok(parsed)
}

/// Reads a WebVTT file as a browser would. The limits and the byte order mark
/// are as in [`read_srt`]. A file without the `WEBVTT` signature is not WebVTT,
/// so it has no cues and is refused with [`ReadError::NoCues`], as is one whose
/// cues are all unusable. See `vtt.rs` for the rules.
pub fn read_vtt(bytes: &[u8]) -> Result<Parsed, ReadError> {
    let text = text_of(bytes)?;
    let parsed = vtt::parse(text, &mut Budget::new(ITEM_BUDGET))?.ok_or(ReadError::NoCues)?;
    if parsed.cues.is_empty() {
        return Err(ReadError::NoCues);
    }
    Ok(parsed)
}

/// Reads an ASS or SSA file for its text, its times and the styling that
/// carries over. The limits and the byte order mark are as in [`read_srt`]. A
/// file with no usable dialogue is refused with [`ReadError::NoCues`]. See
/// `ass.rs` for the rules and the choices made where the format leaves room.
pub fn read_ass(bytes: &[u8]) -> Result<Parsed, ReadError> {
    let text = text_of(bytes)?;
    let parsed = ass::parse(text, &mut Budget::new(ITEM_BUDGET))?;
    if parsed.cues.is_empty() {
        return Err(ReadError::NoCues);
    }
    Ok(parsed)
}

/// The input as text: refused past [`INPUT_BYTES`], refused when it is not
/// UTF-8, and without its byte order mark.
fn text_of(bytes: &[u8]) -> Result<&str, ReadError> {
    if bytes.len() > INPUT_BYTES {
        return Err(ReadError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|e| ReadError::NotUtf8 {
        at: e.valid_up_to(),
    })?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(text))
}

/// Largest subtitle input, in bytes. 8 MiB is the text family's input budget
/// too, so one size limit covers both families. A file past it fails before
/// parsing starts, which bounds the readers' work and memory by this value.
pub const INPUT_BYTES: usize = 8 << 20;

/// Largest subtitle output, in bytes. 32 MiB is the text family's output
/// budget, so a converted subtitle is never larger than a converted text file.
/// Past it nothing is written: the output is built to the end or not at all.
pub const OUTPUT_BYTES: u64 = 32 << 20;

/// Largest timestamp in milliseconds: 99:59:59.999. Both formats write hours
/// as two digits, so a later cue has no time code to be written with; the
/// reader skips it and says so.
pub const MAX_MS: u64 = 359_999_999;

/// Largest number of items a reader stores: each cue, each line and each run
/// is one. The readers' memory grows with the items, not with the bytes (a
/// 2-byte line is a line and a run), so this count is the memory bound. The
/// densest refused 8 MiB shape (one cue of 2-byte lines) peaked at 216.5 MiB
/// at this budget, about 113 B per item over the whole process, so the budget
/// keeps the peak at about a fifth of the 1 GiB `RLIMIT_DATA` the sandbox sets
/// at its floor. A real 8 MiB film subtitle has about 10,000 items
/// (2,000 cues of two lines of two runs): 200 times under the budget. The
/// densest real 8 MiB file, the smallest SRT cues (27 bytes, three items each),
/// has about 930,000, so it still fits.
pub const ITEM_BUDGET: usize = 2_000_000;

/// The items a reader may still store. Each store is charged before it
/// allocates, so a refused store allocates nothing and the reader stops there.
pub(crate) struct Budget {
    left: usize,
}

impl Budget {
    pub(crate) fn new(items: usize) -> Self {
        Self { left: items }
    }

    /// Charges one item, or refuses it with [`ReadError::TooComplex`], which
    /// leaves the budget as it was.
    pub(crate) fn charge(&mut self) -> Result<(), ReadError> {
        self.left = self.left.checked_sub(1).ok_or(ReadError::TooComplex)?;
        Ok(())
    }
}

/// A run of text with one inline style set. A cue line is a list of runs, so
/// a style that changes mid-line stays with its own text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// Plain text: no markup, no line breaks (the readers split lines).
    pub text: String,
    pub italic: bool,
    pub bold: bool,
    pub underline: bool,
}

/// One line of a cue, with the speaker set on it, if the source named one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// Shared by the lines of one speaker. A copy per line would multiply a
    /// long name by the line count, past the memory bound.
    pub voice: Option<Rc<str>>,
    pub runs: Vec<Run>,
}

/// Whether a line has text: a character that is not white space, the set
/// `str::trim` removes. The readers trim lines, so a line of only white space
/// reads back as a blank line. A cue with no such line has no text: the readers
/// do not keep it and the writers drop it.
pub(crate) fn has_text(line: &Line) -> bool {
    line.runs
        .iter()
        .any(|r| r.text.chars().any(|c| !c.is_whitespace()))
}

/// One subtitle: when it is shown, whether it sits at the top, and its lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    /// The cue is placed at the top of the picture.
    pub top: bool,
    pub lines: Vec<Line>,
}

/// What a conversion could not carry over. The declaration order is the order
/// the warnings are shown in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Loss {
    Styling,
    Position,
    Karaoke,
    Drawing,
    Settings,
    VoiceNames,
    Brackets,
}

impl Loss {
    /// One-sentence English description, suitable for a notification.
    pub fn message(self) -> &'static str {
        match self {
            Loss::Styling => "colours, fonts and sizes removed",
            Loss::Position => "exact positions removed",
            Loss::Karaoke => "karaoke effects removed",
            Loss::Drawing => "drawings removed",
            Loss::Settings => "style and region blocks ignored",
            Loss::VoiceNames => "speaker names written as 'Name:' prefixes",
            Loss::Brackets => "characters that look like markup written as look-alikes",
        }
    }
}

/// Cues a reader could not use, and blocks of text outside any cue. `first` is
/// the 1-based line of the first one and why it was refused.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Skipped {
    pub count: usize,
    pub first: Option<(usize, &'static str)>,
}

/// The result of reading one subtitle file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    /// Usable cues, in the order the file gives them. Callers sort them with
    /// [`sort_cues`].
    pub cues: Vec<Cue>,
    /// Losses found while reading, counted in cues.
    pub losses: BTreeMap<Loss, usize>,
    pub skipped: Skipped,
}

/// Why a subtitle file was refused as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// The input is not UTF-8; `at` is the byte offset of the first bad byte.
    NotUtf8 { at: usize },
    /// The input is larger than [`INPUT_BYTES`].
    TooLarge,
    /// The input needs more than [`ITEM_BUDGET`] items. Nothing is kept: the
    /// readers do not truncate, so a partial result is never converted.
    TooComplex,
    /// The input parsed, but no cue in it is usable.
    NoCues,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::NotUtf8 { at } => write!(
                f,
                "not UTF-8 text (byte at {at}): convert it with Encoding first"
            ),
            ReadError::TooLarge => f.write_str("subtitle file is larger than 8 MiB"),
            ReadError::TooComplex => {
                f.write_str("too many lines and styled runs (more than 2,000,000)")
            }
            ReadError::NoCues => f.write_str("no subtitles found"),
        }
    }
}

impl std::error::Error for ReadError {}

/// Sorts cues by start time. The sort is stable: cues that start together keep
/// the order they had in the file.
pub fn sort_cues(cues: &mut [Cue]) {
    cues.sort_by_key(|c| c.start_ms);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget counts exactly. One cue, one line and one run are three
    /// items, so a budget of three reads the text and a budget of two refuses
    /// it, for each reader.
    #[test]
    fn budget_boundary_is_exact() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nx\n";
        let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nx\n";
        let ass = "[Events]\nFormat: Start, End, Text\nDialogue: 0:00:01.00,0:00:02.00,x\n";
        assert_eq!(
            srt::parse(srt, &mut Budget::new(3)).map(|p| p.cues.len()),
            Ok(1)
        );
        assert_eq!(
            srt::parse(srt, &mut Budget::new(2)).map(|p| p.cues.len()),
            Err(ReadError::TooComplex)
        );
        assert_eq!(
            vtt::parse(vtt, &mut Budget::new(3)).map(|p| p.map(|p| p.cues.len())),
            Ok(Some(1))
        );
        assert_eq!(
            vtt::parse(vtt, &mut Budget::new(2)).map(|p| p.map(|p| p.cues.len())),
            Err(ReadError::TooComplex)
        );
        assert_eq!(
            ass::parse(ass, &mut Budget::new(3)).map(|p| p.cues.len()),
            Ok(1)
        );
        assert_eq!(
            ass::parse(ass, &mut Budget::new(2)).map(|p| p.cues.len()),
            Err(ReadError::TooComplex)
        );
    }
}
