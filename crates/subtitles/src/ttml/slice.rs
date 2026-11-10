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

use std::collections::BTreeSet;

use crate::{Budget, Cue, Line, OUTPUT_BYTES, ReadError, Run, has_text};

/// One stored piece of a paragraph: its active interval in milliseconds
/// (never empty) and what it holds.
pub(super) struct Piece {
    pub(super) begin: u64,
    pub(super) end: u64,
    pub(super) content: Content,
}

pub(super) enum Content {
    /// Text after [`normalize`], with the white-space mode it was read in.
    Text { text: String, preserve: bool },
    /// A `br`: a forced line break.
    Break,
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
/// checked by the caller with `begin < end`, into `out`, in time order.
pub(super) fn cues(
    pieces: &[Piece],
    begin: u64,
    end: u64,
    budget: &mut Budget,
    copied: &mut u64,
    out: &mut Vec<Cue>,
) -> Result<(), ReadError> {
    // Each piece starts (true) and stops (false) once. At equal times the
    // order does not matter: every event at a time is applied before its slice.
    let mut events: Vec<(u64, bool, usize)> = pieces
        .iter()
        .enumerate()
        .flat_map(|(i, p)| [(p.begin, true, i), (p.end, false, i)])
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
            && let Some(cue) = slice(pieces, &active, at, stop, budget, copied)?
        {
            out.push(cue);
        }
        at = stop;
    }
    Ok(())
}

/// The cue of one slice, or `None` when the active pieces hold no text.
fn slice(
    pieces: &[Piece],
    active: &BTreeSet<usize>,
    start: u64,
    stop: u64,
    budget: &mut Budget,
    copied: &mut u64,
) -> Result<Option<Cue>, ReadError> {
    let mut lines = Vec::new();
    let mut runs = Vec::new();
    for &i in active {
        // One item per copy: the work of this slice is charged even when the
        // piece holds only white space, so no slice is free.
        budget.charge()?;
        match &pieces[i].content {
            Content::Break => finish_line(&mut runs, &mut lines, budget)?,
            Content::Text { text, preserve } => {
                for (n, part) in text.split('\n').enumerate() {
                    if n > 0 {
                        finish_line(&mut runs, &mut lines, budget)?;
                    }
                    append(&mut runs, part, !*preserve, copied)?;
                }
            }
        }
    }
    finish_line(&mut runs, &mut lines, budget)?;
    if lines.is_empty() {
        return Ok(None);
    }
    budget.charge()?;
    Ok(Some(Cue {
        start_ms: start,
        end_ms: stop,
        top: false,
        lines,
    }))
}

/// Adds text to the current line. With `collapse` (the `default` mode), a
/// space that follows a space already on the line is dropped, so two pieces
/// that each end or start with a space give one space between them.
fn append(
    runs: &mut Vec<Run>,
    part: &str,
    collapse: bool,
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
        return Err(ReadError::TooComplex);
    }
    *copied += bytes;
    match runs.last_mut() {
        Some(run) => run.text.push_str(part),
        None => runs.push(Run {
            text: part.to_owned(),
            italic: false,
            bold: false,
            underline: false,
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
