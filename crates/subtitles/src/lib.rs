//! Subtitle cues: the model the subtitle readers fill and the SRT and WebVTT
//! writers print. Standard library only, because the shipped subtitle binary
//! links it and the dependency guard allows nothing else.

use std::collections::BTreeMap;
use std::fmt;

mod srt;
mod time;
mod write;

pub use write::{write_srt, write_vtt};

/// Reads an SRT file as a media player would. Refused past [`INPUT_BYTES`]
/// before anything is parsed, refused when it is not UTF-8, and refused with
/// [`ReadError::NoCues`] when no cue in it is usable. A byte order mark is
/// skipped. Everything else is read leniently: see `srt.rs` for the rules.
pub fn read_srt(bytes: &[u8]) -> Result<Parsed, ReadError> {
    if bytes.len() > INPUT_BYTES {
        return Err(ReadError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|e| ReadError::NotUtf8 {
        at: e.valid_up_to(),
    })?;
    let parsed = srt::parse(text.strip_prefix('\u{feff}').unwrap_or(text));
    if parsed.cues.is_empty() {
        return Err(ReadError::NoCues);
    }
    Ok(parsed)
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
    pub voice: Option<String>,
    pub runs: Vec<Run>,
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
