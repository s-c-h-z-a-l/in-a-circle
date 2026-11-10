//! Cuts one paragraph into the cues its timed pieces make. The paragraph is
//! split at every time a piece starts or stops, and each slice between two
//! such times holds the pieces active in it, in document order. A slice with
//! no text makes no cue. Neighbouring slices with the same text stay apart:
//! the reader does not merge them.
//!
//! Cost: a piece is copied into every slice it is active in, so each copy is
//! one item of the budget, charged before the copy is made. The copies are
//! bounded by the item budget, and their text by [`OUTPUT_BYTES`]: the cues
//! of a paragraph are the output, and past that size the conversion would be
//! refused anyway.
//!
//! Losses: a cue counts each loss kind once, if any piece active in it carries
//! that kind of declaration (see [`Flags`]), or if the paragraph does (its
//! `extra` flags, which hold for every cue the paragraph is cut into).

use std::collections::{BTreeMap, BTreeSet};

use super::style::{Css, Flags};
use crate::{Budget, Cue, Line, Loss, OUTPUT_BYTES, ReadError, Run, has_text};

/// One stored piece of a paragraph: its active interval in milliseconds, what
/// it holds, the inline style of its text, and the declarations that affect
/// it. An `end` of `None` is the paragraph's own end, known only when the
/// paragraph closes (white space and text with no time of their own, 12.4).
pub(super) struct Piece {
    pub(super) begin: u64,
    pub(super) end: Option<u64>,
    pub(super) content: Content,
    pub(super) style: RunStyle,
    pub(super) flags: Flags,
}

pub(super) enum Content {
    /// Text after [`normalize`], with the white-space mode it was read in.
    Text { text: String, preserve: bool },
    /// A `br`: a forced line break.
    Break,
}

/// The inline styles a run can carry (the `Run` fields).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct RunStyle {
    pub(super) italic: bool,
    pub(super) bold: bool,
    pub(super) underline: bool,
}

impl RunStyle {
    /// The run style of a computed style set. An unset value is not on
    /// (10.2.22 to 10.2.24, 10.2.43: the initial values are `normal` and `none`).
    pub(super) fn of(css: &Css) -> RunStyle {
        RunStyle {
            italic: css.italic == Some(true),
            bold: css.bold == Some(true),
            underline: css.underline == Some(true),
        }
    }

    fn matches(self, run: &Run) -> bool {
        run.italic == self.italic && run.bold == self.bold && run.underline == self.underline
    }
}

/// What a paragraph's cues come to: the cues in document order, the count of
/// each loss kind (once per cue), and the text copied into cues so far.
#[derive(Default)]
pub(super) struct Output {
    pub(super) cues: Vec<Cue>,
    pub(super) losses: BTreeMap<Loss, usize>,
    pub(super) copied: u64,
}

/// The text as TTML's white space rules read it (8.2.10). In `default`, every
/// run of space, tab or line feed is one space (the XSL-FO `treat-as-space`
/// and `white-space-collapse` of the standard); lines are trimmed later. In
/// `preserve`, a tab is a space and a line feed stays a line break
/// (`linefeed-treatment="preserve"`). XML has already made CR LF into LF.
pub(super) fn normalize(raw: &str, preserve: bool) -> String {
    if preserve {
        return raw.replace('\t', " ");
    }
    let mut out = String::with_capacity(raw.len());
    let mut in_space = false;
    for c in raw.chars() {
        if matches!(c, ' ' | '\t' | '\n' | '\r') {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// Writes the cues of the paragraph that runs from `begin` to `end`, both
/// checked by the caller with `begin < end`, into `out`, in time order. `top`
/// is the paragraph's placement; `extra` holds the losses of the paragraph's
/// own ignored elements, which hold for every cue it is cut into.
pub(super) fn cues(
    pieces: &[Piece],
    (begin, end): (u64, u64),
    top: bool,
    extra: Flags,
    budget: &mut Budget,
    out: &mut Output,
) -> Result<(), ReadError> {
    // Each piece starts (true) and stops (false) once. At equal times the
    // order does not matter: every event at a time is applied before its slice.
    // A piece with no end of its own lasts to the paragraph's end; one that
    // begins at or after its end is never active, so it has no events.
    let until = |p: &Piece| p.end.unwrap_or(end);
    let mut events: Vec<(u64, bool, usize)> = pieces
        .iter()
        .enumerate()
        .filter(|(_, p)| p.begin < until(p))
        .flat_map(|(i, p)| [(p.begin, true, i), (until(p), false, i)])
        .collect();
    events.sort_unstable();
    let mut active = BTreeSet::new();
    let mut next = 0;
    let mut at = begin;
    while at < end {
        while let Some(&(time, starts, i)) = events.get(next) {
            if time > at {
                break;
            }
            if starts {
                active.insert(i);
            } else {
                active.remove(&i);
            }
            next += 1;
        }
        // The next event after `at` ends this slice, or the paragraph does.
        let stop = events.get(next).map_or(end, |&(time, _, _)| time.min(end));
        if !active.is_empty()
            && let Some(flags) = slice(pieces, &active, (at, stop), top, budget, out)?
        {
            let flags = flags.join(extra);
            if flags.styling {
                *out.losses.entry(Loss::Styling).or_insert(0) += 1;
            }
            if flags.position {
                *out.losses.entry(Loss::Position).or_insert(0) += 1;
            }
        }
        at = stop;
    }
    Ok(())
}

/// The cue of one slice, pushed to `out`, with the flags of its active pieces;
/// `None` when the active pieces hold no text.
fn slice(
    pieces: &[Piece],
    active: &BTreeSet<usize>,
    (start, stop): (u64, u64),
    top: bool,
    budget: &mut Budget,
    out: &mut Output,
) -> Result<Option<Flags>, ReadError> {
    let mut lines = Vec::new();
    let mut runs: Vec<Run> = Vec::new();
    let mut flags = Flags::default();
    for &i in active {
        // One item per copy: the work of this slice is charged even when the
        // piece holds only white space, so no slice is free.
        budget.charge()?;
        let piece = &pieces[i];
        flags = flags.join(piece.flags);
        match &piece.content {
            Content::Break => finish_line(&mut runs, &mut lines, budget)?,
            Content::Text { text, preserve } => {
                for (n, part) in text.split('\n').enumerate() {
                    if n > 0 {
                        finish_line(&mut runs, &mut lines, budget)?;
                    }
                    append(&mut runs, part, !*preserve, piece.style, &mut out.copied)?;
                }
            }
        }
    }
    finish_line(&mut runs, &mut lines, budget)?;
    if lines.is_empty() {
        return Ok(None);
    }
    budget.charge()?;
    out.cues.push(Cue {
        start_ms: start,
        end_ms: stop,
        top,
        lines,
    });
    Ok(Some(flags))
}

/// Adds text to the current line, in `style`. With `collapse` (the `default`
/// mode), a space that follows a space already on the line is dropped, so two
/// pieces that each end or start with a space give one space between them.
/// Text of the same style joins the last run; a new style starts a new run.
fn append(
    runs: &mut Vec<Run>,
    part: &str,
    collapse: bool,
    style: RunStyle,
    copied: &mut u64,
) -> Result<(), ReadError> {
    let after_space = runs.last().is_some_and(|r| r.text.ends_with(' '));
    let part = match part.strip_prefix(' ') {
        Some(rest) if collapse && after_space => rest,
        _ => part,
    };
    if part.is_empty() {
        return Ok(());
    }
    let bytes = u64::try_from(part.len()).unwrap_or(u64::MAX);
    if copied.saturating_add(bytes) > OUTPUT_BYTES {
        return Err(ReadError::TextTooLarge);
    }
    *copied += bytes;
    match runs.last_mut() {
        Some(run) if style.matches(run) => run.text.push_str(part),
        _ => runs.push(Run {
            text: part.to_owned(),
            italic: style.italic,
            bold: style.bold,
            underline: style.underline,
        }),
    }
    Ok(())
}

/// Ends the current line: its white space at either end is not text (the
/// readers trim lines), and a line with no text is not kept. A kept line is
/// charged before it is stored.
fn finish_line(
    runs: &mut Vec<Run>,
    lines: &mut Vec<Line>,
    budget: &mut Budget,
) -> Result<(), ReadError> {
    if let Some(first) = runs.first_mut() {
        let cut = first.text.len() - first.text.trim_start().len();
        first.text.drain(..cut);
    }
    if let Some(last) = runs.last_mut() {
        let keep = last.text.trim_end().len();
        last.text.truncate(keep);
    }
    runs.retain(|r| !r.text.is_empty());
    let line = Line {
        voice: None,
        runs: std::mem::take(runs),
    };
    if has_text(&line) {
        budget.charge()?;
        lines.push(line);
    }
    Ok(())
}
