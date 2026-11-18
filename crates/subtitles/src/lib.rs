//! Subtitle cues: the model the subtitle readers fill and the SRT and WebVTT
//! writers print. The standard library, and the `xml` crate for the TTML reader:
//! the shipped subtitle binary links these and the dependency guard allows
//! nothing else.

use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

mod ass;
mod srt;
mod tag;
mod time;
mod ttml;
mod vtt;
mod write;

pub use write::{write_srt, write_vtt};

/// Reads an SRT file as a media player would. Refused past [`INPUT_BYTES`]
/// before anything is parsed, refused when it is not UTF-8, refused past
/// [`ITEM_BUDGET`] items, and refused when no cue in it is usable: with
/// [`ReadError::AllSkipped`] if items were skipped, else [`ReadError::NoCues`].
/// A byte order mark is skipped. Everything else is read leniently: see
/// `srt.rs` for the rules.
pub fn read_srt(bytes: &[u8]) -> Result<Parsed, ReadError> {
    let text = text_of(bytes)?;
    let parsed = srt::parse(text, &mut Budget::new(ITEM_BUDGET))?;
    if parsed.cues.is_empty() {
        return Err(no_cues(parsed.skipped));
    }
    Ok(parsed)
}

/// Reads a WebVTT file as a browser would. The limits and the byte order mark
/// are as in [`read_srt`]. A file without the `WEBVTT` signature is not WebVTT,
/// so it has no cues and is refused with [`ReadError::NoCues`]; one whose cues
/// are all unusable is refused as [`read_srt`] refuses it. See `vtt.rs` for the
/// rules.
pub fn read_vtt(bytes: &[u8]) -> Result<Parsed, ReadError> {
    let text = text_of(bytes)?;
    let parsed = vtt::parse(text, &mut Budget::new(ITEM_BUDGET))?.ok_or(ReadError::NoCues)?;
    if parsed.cues.is_empty() {
        return Err(no_cues(parsed.skipped));
    }
    Ok(parsed)
}

/// Reads an ASS or SSA file for its text, its times and the styling that
/// carries over. The limits and the byte order mark are as in [`read_srt`]. A
/// file with no usable dialogue is refused as [`read_srt`] refuses it. See
/// `ass.rs` for the rules and the choices made where the format leaves room.
pub fn read_ass(bytes: &[u8]) -> Result<Parsed, ReadError> {
    let text = text_of(bytes)?;
    let parsed = ass::parse(text, &mut Budget::new(ITEM_BUDGET))?;
    if parsed.cues.is_empty() {
        return Err(no_cues(parsed.skipped));
    }
    Ok(parsed)
}

/// Reads a TTML file (TTML2, and the DFXP namespace of TTML 1) for its
/// structure and timing: the limits are as in [`read_srt`], and the text is
/// decoded by the `xml` crate. A document that is not well-formed is refused
/// whole, with the line, and so is a time base the reader does not convert (a
/// clock or drop-frame time code). A document whose subtitles are images is
/// refused as [`ReadError::ImageBased`], and one whose items are all skipped as
/// [`ReadError::AllSkipped`], with the first skip. An element the reader does
/// not convert is skipped or counted in its losses. See `ttml/mod.rs` for the
/// accounting.
pub fn read_ttml(bytes: &[u8]) -> Result<Parsed, ReadError> {
    if bytes.len() > INPUT_BYTES {
        return Err(ReadError::TooLarge);
    }
    let parsed = ttml::parse(bytes, &mut Budget::new(ITEM_BUDGET))?;
    if parsed.cues.is_empty() {
        return Err(no_cues(parsed.skipped));
    }
    Ok(parsed)
}

/// The refusal of a file with no usable cue. A skipped item is always kept
/// with its line, so `first` is set exactly when something was skipped: then
/// the count and the first reason are shown, not the bare `NoCues`.
fn no_cues(skipped: Skipped) -> ReadError {
    match skipped.first {
        Some((line, reason)) => ReadError::AllSkipped {
            count: skipped.count,
            line,
            reason,
        },
        None => ReadError::NoCues,
    }
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
/// is one. The TTML reader also charges each element it opens, each text or
/// `br` piece of a paragraph, and each copy of a piece into a cue (one per
/// cue the piece is active in; see `ttml/slice.rs`). The readers' memory grows
/// with the items, not with the bytes (a
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
    /// Elements the reader ignores whole (foreign namespaces, unsupported
    /// vocabulary), and images, which a text file cannot show. Counted in items,
    /// not cues: each ignored element and each image is one. The text around
    /// an image is converted and counted as usual.
    Elements,
    /// Time codes read as continuous time: an SMPTE time base whose marker mode
    /// is discontinuous or absent. Such a time has no media time of its own, so
    /// the reader's conversion is an approximation. Counted in cues.
    Timing,
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
            Loss::Elements => "unsupported elements ignored",
            Loss::Timing => "time codes read as continuous",
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
    /// Losses found while reading: counted in cues, except [`Loss::Elements`],
    /// which counts items.
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
    /// The input has no cue and nothing in it was skipped: it is empty, or it
    /// has no cue at all (no dialogue, no `WEBVTT` signature).
    NoCues,
    /// The XML is not well-formed, at `line` (1-based); `reason` says what.
    Malformed { line: u32, reason: &'static str },
    /// The root is not a `tt` element of a TTML namespace.
    NotTtml,
    /// The document uses a construct the reader does not convert.
    Unsupported(&'static str),
    /// The document has image subtitles (`smpte:backgroundImage`, `image`
    /// elements) and no text cue: the images cannot be converted.
    ImageBased,
    /// The input has items to read, but no cue in it is usable: `count` items
    /// were skipped, the first at `line` (1-based) for `reason`. Shown in place
    /// of `NoCues`, which would hide the reason.
    AllSkipped {
        count: usize,
        line: usize,
        reason: &'static str,
    },
    /// The text copied into cues (a paragraph's text is copied into each slice
    /// it is active in) passes [`OUTPUT_BYTES`]: the output could not be written.
    TextTooLarge,
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
            ReadError::Malformed { line, reason } => {
                write!(f, "not well-formed TTML (line {line}): {reason}")
            }
            ReadError::NotTtml => f.write_str("not a TTML document"),
            ReadError::Unsupported(reason) => write!(f, "unsupported TTML: {reason}"),
            ReadError::ImageBased => f.write_str("no text subtitles found (image-based)"),
            ReadError::AllSkipped {
                count,
                line,
                reason,
            } => write!(
                f,
                "no subtitles found ({count} items skipped, first at line {line}: {reason})"
            ),
            ReadError::TextTooLarge => f.write_str("subtitle text would be larger than 32 MiB"),
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

    /// The TTML count is exact too: the root, the body and the paragraph are
    /// elements (3); the text is a piece (1); the one slice copies it (1); its
    /// line (1) and its cue (1). Seven items; six refuses.
    #[test]
    fn ttml_budget_boundary_is_exact() {
        let ttml = concat!(
            r#"<tt xmlns="http://www.w3.org/ns/ttml"><body>"#,
            r#"<p begin="0s" end="1s">x</p></body></tt>"#
        );
        assert_eq!(
            ttml::parse(ttml.as_bytes(), &mut Budget::new(7)).map(|p| p.cues.len()),
            Ok(1)
        );
        assert_eq!(
            ttml::parse(ttml.as_bytes(), &mut Budget::new(6)).map(|p| p.cues.len()),
            Err(ReadError::TooComplex)
        );
    }

    /// The style table is charged entry by entry, before it is stored. A `style`
    /// is its element and its entry (two items); each reference a style makes is
    /// one more. The least budget that reads the document is therefore exact:
    /// one more style costs two items, one more reference one item, and a budget
    /// one below the least refuses the document as too complex.
    #[test]
    fn ttml_style_table_is_charged_exactly() {
        let styles = |n: usize| {
            let defs: String = (0..n)
                .map(|i| format!(r#"<style xml:id="s{i}" tts:color="red"/>"#))
                .collect();
            format!(
                r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling"><head><styling>{defs}</styling></head><body><div><p begin="0s" end="1s">x</p></div></body></tt>"#
            )
        };
        let refs = |n: usize| {
            let ids = vec!["s0"; n].join(" ");
            format!(
                r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling"><head><styling><style xml:id="s0"/><style xml:id="x" style="{ids}"/></styling></head><body><div><p begin="0s" end="1s">x</p></div></body></tt>"#
            )
        };
        let least = |text: &str| {
            // Refused at the item budget, so there is no least budget: fail here.
            assert!(ttml::parse(text.as_bytes(), &mut Budget::new(ITEM_BUDGET)).is_ok());
            (1..=ITEM_BUDGET)
                .find(|&b| ttml::parse(text.as_bytes(), &mut Budget::new(b)).is_ok())
                .expect("some budget reads the document")
        };
        assert_eq!(
            least(&styles(3)) - least(&styles(2)),
            2,
            "element and entry"
        );
        assert_eq!(
            least(&refs(3)) - least(&refs(2)),
            1,
            "one item per reference"
        );
        let text = styles(5);
        let need = least(&text);
        assert_eq!(
            ttml::parse(text.as_bytes(), &mut Budget::new(need - 1)).map(|p| p.cues.len()),
            Err(ReadError::TooComplex)
        );
    }

    /// A SRT line break is a line: `x<br>y` is one cue, two lines and two runs,
    /// five items. A budget of four refuses it.
    #[test]
    fn srt_line_break_is_charged_as_a_line() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nx<br>y\n";
        assert_eq!(
            srt::parse(srt, &mut Budget::new(5)).map(|p| p.cues.len()),
            Ok(1)
        );
        assert_eq!(
            srt::parse(srt, &mut Budget::new(4)).map(|p| p.cues.len()),
            Err(ReadError::TooComplex)
        );
    }

    /// Losses are counted in cues, so a read with no cue counts none: the
    /// `set` outside any paragraph is styling for every cue there is, and the
    /// paragraph that would be one is skipped (its end is before its begin).
    #[test]
    fn a_document_without_a_cue_counts_no_loss() {
        let ttml = concat!(
            r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling"><body>"#,
            r#"<div><set begin="0s" tts:color="red"/><p begin="2s" end="1s">x</p></div></body></tt>"#
        );
        let parsed = ttml::parse(ttml.as_bytes(), &mut Budget::new(ITEM_BUDGET))
            .expect("the document is read, with nothing usable in it");
        assert!(parsed.cues.is_empty());
        assert!(parsed.losses.is_empty());
    }

    /// An ignored element is not an item (the accounting table charges element
    /// frames, pieces, copies, lines, cues and style entries). A visibility `set`
    /// in a paragraph is ignored, so the least budget that reads a paragraph
    /// with one is the least for the empty paragraph.
    #[test]
    fn a_visibility_set_in_a_paragraph_costs_no_item() {
        let least = |body: &str| {
            let text = format!(
                r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling"><body><div><p begin="0s" end="1s">{body}</p></div></body></tt>"#
            );
            // Refused at the item budget, so there is no least budget: fail here.
            assert!(ttml::parse(text.as_bytes(), &mut Budget::new(ITEM_BUDGET)).is_ok());
            (1..=ITEM_BUDGET)
                .find(|&b| ttml::parse(text.as_bytes(), &mut Budget::new(b)).is_ok())
                .expect("some budget reads the document")
        };
        assert_eq!(least(r#"<set begin="0s" tts:display="none"/>"#), least(""),);
    }

    /// A `br` over a span that lasts no time is no piece (`Doc::line_break`), so
    /// it costs its element frame and nothing more: one item over the empty span.
    #[test]
    fn a_zero_long_break_costs_its_frame_only() {
        let least = |body: &str| {
            let text = format!(
                r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="0s" end="1s">x<span begin="0.5s" end="0.5s">{body}</span></p></div></body></tt>"#
            );
            // Refused at the item budget, so there is no least budget: fail here.
            assert!(ttml::parse(text.as_bytes(), &mut Budget::new(ITEM_BUDGET)).is_ok());
            (1..=ITEM_BUDGET)
                .find(|&b| ttml::parse(text.as_bytes(), &mut Budget::new(b)).is_ok())
                .expect("some budget reads the document")
        };
        assert_eq!(least("<br/>") - least(""), 1);
    }

    /// The animated scope pass charges one item per element it scopes, and one
    /// per IDREF of an `animate` attribute plus its element's entry (see
    /// `animated_scopes`). Each document has the same hidden paragraphs (their
    /// text is not kept, so their cost is the same), and a head animation that
    /// is named is charged the same in both: the difference is the scope alone.
    /// The least budget that is not refused for lack of items is the measure.
    #[test]
    fn the_animated_scope_pass_charges_one_item_per_scope() {
        let least = |text: &str| {
            (1..=ITEM_BUDGET)
                .find(|&b| {
                    !matches!(
                        ttml::parse(text.as_bytes(), &mut Budget::new(b)),
                        Err(ReadError::TooComplex)
                    )
                })
                .expect("some budget reads the document")
        };
        let doc = |head: &str, body: &str| {
            format!(
                r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling"><head>{head}</head><body>{body}</body></tt>"#
            )
        };
        let hidden = r#"<p begin="0s" end="1s" tts:display="none">a</p>"#;
        let set = r#"<set begin="0s" tts:visibility="hidden"/>"#;
        let base = least(&doc("", &format!("<div>{hidden}</div>")));
        assert_eq!(
            least(&doc("", &format!("<div>{set}{hidden}</div>"))) - base,
            1,
            "a division"
        );
        assert_eq!(
            least(&doc("", &format!("{set}<div>{hidden}</div>"))) - base,
            1,
            "the body"
        );
        let body = r#"<div><p begin="0s" end="1s" region="r" tts:display="none">a</p></div>"#;
        let region = least(&doc(r#"<layout><region xml:id="r"/></layout>"#, body));
        assert_eq!(
            least(&doc(
                &format!(r#"<layout><region xml:id="r">{set}</region></layout>"#),
                body
            )) - region,
            1,
            "a region"
        );
        let anim =
            r#"<animation><set xml:id="a1" begin="0s" tts:visibility="hidden"/></animation>"#;
        let named = least(&doc(anim, &format!("<div>{hidden}</div>")));
        let p_named = r#"<div><p begin="0s" end="1s" tts:display="none" animate="a1">a</p></div>"#;
        assert_eq!(
            least(&doc(anim, p_named)) - named,
            2,
            "one IDREF and the paragraph's entry"
        );
    }
}
