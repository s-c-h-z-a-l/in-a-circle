//! The SRT reader. It follows what media players accept, which is what ffmpeg
//! accepts too (the differential test in `tests/srt.rs` checks that):
//!
//! - Line breaks: LF, CRLF or a lone CR. A line of spaces is a blank line.
//! - A cue number is ignored. A cue is a timing line, then its text up to a
//!   blank line. A valid timing line also starts a cue when no blank line
//!   comes before it; a line that only contains `-->` is text.
//! - Timing: `H+:MM:SS,mmm` (`.` also accepted). Minutes and seconds take one
//!   or two digits; the milliseconds are read as an integer, so `,5` is 5 ms.
//!   Coordinates after the end time are dropped, a `Position` loss per cue.
//! - Markup: `<i> <b> <u>` and their `{\i1}`-style forms (`0` switches them
//!   off), `{\an7}`/`{\an8}`/`{\an9}` for a top cue, `<font>` (its text stays,
//!   the styling is reported), other `<…>` tags (dropped, text kept), and other
//!   `{\…}` blocks (their effect is reported).
//! - A cue that cannot be used (bad timing, ends before it starts, 100 hours
//!   or more) is skipped, and the rest of the file converts.
//! - Text outside any cue (a title, stray lines between cues) is a block of
//!   lines up to a blank line or a timing line. It is counted once as skipped,
//!   never shown and never joined to a cue. A lone number line is a cue index
//!   and is not counted.
//!
//! Standard library only. Every loop moves forward, so the work is linear in
//! the input; a `<` or `{` with no closer later in its line is text, and a
//! failed search for a closer is remembered so the line is searched once.

use std::collections::BTreeMap;

use crate::tag::opens_tag;
use crate::{Budget, Cue, Line, Loss, MAX_MS, Parsed, ReadError, Run, Skipped, has_text};

const MS_PER_SECOND: u64 = 1_000;
const MS_PER_MINUTE: u64 = 60 * MS_PER_SECOND;
const MS_PER_HOUR: u64 = 60 * MS_PER_MINUTE;

/// Why a cue cannot be used. The messages are what the user sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    Format,
    Late,
    Order,
}

impl Fault {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Fault::Format => "bad timestamp",
            Fault::Late => "time is 100 hours or more",
            Fault::Order => "ends before it starts",
        }
    }
}

/// Reads a whole SRT text (BOM already removed). Cues come out in file order.
pub(crate) fn parse(text: &str, budget: &mut Budget) -> Result<Parsed, ReadError> {
    let mut reader = Reader::default();
    for (i, line) in lines(text).enumerate() {
        reader.line(i + 1, line.trim(), budget)?;
    }
    reader.finish(budget)
}

/// Lines split at LF, CRLF or a lone CR. A final line break ends the last
/// line; it does not start an empty one.
pub(crate) fn lines(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (line, next) = match rest.find(['\r', '\n']) {
            Some(i) => {
                let sep = if rest[i..].starts_with("\r\n") { 2 } else { 1 };
                (&rest[..i], &rest[i + sep..])
            }
            None => (rest, ""),
        };
        rest = next;
        Some(line)
    })
}

#[derive(Default)]
struct Reader<'a> {
    cues: Vec<Cue>,
    losses: BTreeMap<Loss, usize>,
    skipped: Skipped,
    /// The cue being read. Its text runs to the next blank line, or to the
    /// next valid timing line.
    open: Option<Open>,
    /// A line of digits in a cue's text. If a timing line follows it, it was
    /// the next cue's number and is dropped; otherwise it is text.
    index: Option<&'a str>,
    /// Text outside any cue, between cues, being collected.
    stray: Option<Stray>,
}

/// A block of non-blank lines that belongs to no cue.
struct Stray {
    first: usize,
    lines: usize,
    /// The first line is all digits. Alone, such a block is a cue index whose
    /// timing never came, and it is not reported.
    number: bool,
}

/// Reason for a stray block. It is counted once per block, so a file with
/// text the conversion cannot carry says so instead of dropping it silently.
pub(crate) const STRAY: &str = "text outside any cue";

impl<'a> Reader<'a> {
    /// `line` is trimmed; `n` is its 1-based line number.
    fn line(&mut self, n: usize, line: &'a str, budget: &mut Budget) -> Result<(), ReadError> {
        if line.is_empty() {
            self.flush_index(budget)?;
            self.close(budget)?;
            self.flush_stray();
        } else if self.open.is_none() {
            // Between cues a timing line opens a cue, even a broken one, so
            // it is counted as skipped. Any other line is part of a stray block.
            if line.contains("-->") {
                self.flush_stray();
                self.start(n, line);
            } else {
                self.add_stray(n, line);
            }
        } else if line.contains("-->") && !matches!(span(line), Err(Fault::Format)) {
            // Inside a cue only a well-formed timing line starts the next
            // one. `a --> b` and a broken timing are text.
            self.index = None;
            self.close(budget)?;
            self.start(n, line);
        } else {
            self.flush_index(budget)?;
            if number(line).is_some() {
                self.index = Some(line);
            } else {
                self.add(line, budget)?;
            }
        }
        Ok(())
    }

    fn start(&mut self, n: usize, line: &str) {
        let mut open = Open {
            at_line: n,
            span: span(line),
            style: Style::default(),
            top: false,
            drawing: false,
            lost: Vec::new(),
            lines: Vec::new(),
        };
        // Coordinates are the tokens after the end time. They are dropped,
        // and the loss is counted once per cue when the cue is kept.
        if line
            .split_once("-->")
            .is_some_and(|(_, end)| end.split_whitespace().nth(1).is_some())
        {
            open.lose(Loss::Position);
        }
        self.open = Some(open);
    }

    fn flush_index(&mut self, budget: &mut Budget) -> Result<(), ReadError> {
        if let Some(line) = self.index.take() {
            self.add(line, budget)?;
        }
        Ok(())
    }

    fn add(&mut self, line: &str, budget: &mut Budget) -> Result<(), ReadError> {
        if let Some(open) = self.open.as_mut() {
            let runs = markup(line, open, budget)?;
            if !runs.is_empty() {
                budget.charge()?;
                open.lines.push(Line { voice: None, runs });
            }
        }
        Ok(())
    }

    /// Ends the open cue. A cue with no text is not stored: it has nothing to
    /// show, and the writers would drop it too. A drawing removed from it is
    /// still counted, as in the ASS reader.
    fn close(&mut self, budget: &mut Budget) -> Result<(), ReadError> {
        let Some(open) = self.open.take() else {
            return Ok(());
        };
        let (start_ms, end_ms) = match open.span {
            Ok(span) => span,
            Err(fault) => {
                self.skip(open.at_line, fault.message());
                return Ok(());
            }
        };
        if !open.lines.iter().any(has_text) {
            if open.lost.contains(&Loss::Drawing) {
                *self.losses.entry(Loss::Drawing).or_insert(0) += 1;
            }
            return Ok(());
        }
        budget.charge()?;
        for loss in open.lost {
            *self.losses.entry(loss).or_insert(0) += 1;
        }
        self.cues.push(Cue {
            start_ms,
            end_ms,
            top: open.top,
            lines: open.lines,
        });
        Ok(())
    }

    /// Text between cues. A block is not a cue, so it is never shown and never
    /// joined to the previous cue; it is only counted, in `flush_stray`.
    fn add_stray(&mut self, n: usize, line: &str) {
        let stray = self.stray.get_or_insert(Stray {
            first: n,
            lines: 0,
            number: number(line).is_some(),
        });
        stray.lines += 1;
    }

    fn flush_stray(&mut self) {
        let Some(stray) = self.stray.take() else {
            return;
        };
        if stray.lines == 1 && stray.number {
            return;
        }
        self.skip(stray.first, STRAY);
    }

    fn skip(&mut self, line: usize, reason: &'static str) {
        self.skipped.count += 1;
        if self.skipped.first.is_none_or(|(first, _)| line < first) {
            self.skipped.first = Some((line, reason));
        }
    }

    fn finish(mut self, budget: &mut Budget) -> Result<Parsed, ReadError> {
        self.flush_index(budget)?;
        self.close(budget)?;
        self.flush_stray();
        Ok(Parsed {
            cues: self.cues,
            losses: self.losses,
            skipped: self.skipped,
        })
    }
}

/// The cue being read.
struct Open {
    at_line: usize,
    span: Result<(u64, u64), Fault>,
    style: Style,
    top: bool,
    /// Between `\p1` and `\p0` the text is drawing commands, not subtitle text.
    drawing: bool,
    /// Losses in this cue, each reported once per cue.
    lost: Vec<Loss>,
    lines: Vec<Line>,
}

impl Open {
    fn lose(&mut self, loss: Loss) {
        if !self.lost.contains(&loss) {
            self.lost.push(loss);
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Style {
    pub(crate) italic: bool,
    pub(crate) bold: bool,
    pub(crate) underline: bool,
}

/// Search state for one line. Once a search for a closer fails, no later
/// opener on the line can close either, so it is not searched again.
#[derive(Default)]
struct Scan {
    no_gt: bool,
    no_brace: bool,
}

/// `start --> end [coordinates]`. Coordinates after the end time are dropped.
fn span(line: &str) -> Result<(u64, u64), Fault> {
    let (start, end) = line.split_once("-->").ok_or(Fault::Format)?;
    let start = time(start.trim())?;
    let end = time(end.split_whitespace().next().unwrap_or_default())?;
    if end < start {
        return Err(Fault::Order);
    }
    Ok((start, end))
}

/// `H+:MM:SS,mmm`, `.` also accepted before the milliseconds. Hours take any
/// number of digits; minutes and seconds one or two (a minute of 60 carries,
/// as in ffmpeg); milliseconds any count, read as an integer, so `,1234` is
/// 1234 ms, which is what ffmpeg reads too.
fn time(s: &str) -> Result<u64, Fault> {
    let mut fields = s.split(':');
    let (Some(h), Some(m), Some(rest), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err(Fault::Format);
    };
    let Some((sec, ms)) = rest.split_once([',', '.']) else {
        return Err(Fault::Format);
    };
    let hours = number(h).ok_or(Fault::Format)?;
    let minutes = small(m).ok_or(Fault::Format)?;
    let seconds = small(sec).ok_or(Fault::Format)?;
    let millis = number(ms).ok_or(Fault::Format)?;
    // Overflow means the time is far past the limit, so it reports the same
    // fault as a time just past it. minutes and seconds are at most 99, so
    // their products cannot overflow.
    hours
        .checked_mul(MS_PER_HOUR)
        .and_then(|t| t.checked_add(minutes * MS_PER_MINUTE))
        .and_then(|t| t.checked_add(seconds * MS_PER_SECOND))
        .and_then(|t| t.checked_add(millis))
        .filter(|&t| t <= MAX_MS)
        .ok_or(Fault::Late)
}

/// Decimal digits, non-empty. Saturates instead of overflowing, so an absurd
/// field reads as a value past the limit.
pub(crate) fn number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(s.bytes().fold(0, |n, b| {
        n.saturating_mul(10).saturating_add(u64::from(b - b'0'))
    }))
}

/// One or two digits: minutes and seconds.
fn small(s: &str) -> Option<u64> {
    if s.len() <= 2 { number(s) } else { None }
}

/// Splits one text line into runs, applying and removing its markup. The
/// style carries across the lines of a cue, as players do.
fn markup(line: &str, open: &mut Open, budget: &mut Budget) -> Result<Vec<Run>, ReadError> {
    let mut runs = Vec::new();
    let mut scan = Scan::default();
    let mut rest = line;
    while let Some(i) = rest.find(['<', '{']) {
        emit(&mut runs, open, &rest[..i], budget)?;
        let tail = &rest[i..];
        let len = if tail.starts_with('<') {
            tag_len(tail, &mut scan)
        } else {
            block_len(tail, &mut scan)
        };
        match len {
            Some(n) => {
                apply(&tail[..n], open);
                rest = &tail[n..];
            }
            // The opener is text. It is one byte, `<` or `{`.
            None => {
                emit(&mut runs, open, &tail[..1], budget)?;
                rest = &tail[1..];
            }
        }
    }
    emit(&mut runs, open, rest, budget)?;
    Ok(runs)
}

/// Adds text to the line unless the cue is in drawing mode.
fn emit(
    runs: &mut Vec<Run>,
    open: &Open,
    text: &str,
    budget: &mut Budget,
) -> Result<(), ReadError> {
    if !open.drawing {
        push_text(runs, open.style, text, budget)?;
    }
    Ok(())
}

/// Length of a tag starting at `tail`: `<`, an optional `/`, an ASCII letter,
/// then up to and including the next `>`.
fn tag_len(tail: &str, scan: &mut Scan) -> Option<usize> {
    if scan.no_gt || !opens_tag(tail) {
        return None;
    }
    let len = tail.find('>').map(|i| i + 1);
    if len.is_none() {
        scan.no_gt = true;
    }
    len
}

/// Length of an override block starting at `tail`: `{\` up to the next `}`.
/// A `{` not followed by a backslash is text.
fn block_len(tail: &str, scan: &mut Scan) -> Option<usize> {
    if scan.no_brace || !tail.starts_with("{\\") {
        return None;
    }
    let len = tail.find('}').map(|i| i + 1);
    if len.is_none() {
        scan.no_brace = true;
    }
    len
}

/// Applies one whole tag or block (`<…>` or `{\…}`) to the cue.
fn apply(markup: &str, open: &mut Open) {
    let inner = &markup[1..markup.len() - 1];
    if markup.starts_with('<') {
        tag(inner, open);
    } else {
        block(inner, open);
    }
}

/// `inner` is a tag without its angle brackets: `i`, `/i`, `font color="red"`.
fn tag(inner: &str, open: &mut Open) {
    // Self-closing: there is nothing to switch.
    if inner.ends_with('/') {
        return;
    }
    let (closing, body) = match inner.strip_prefix('/') {
        Some(b) => (true, b),
        None => (false, inner),
    };
    let name = body
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default();
    if name.eq_ignore_ascii_case("i") {
        open.style.italic = !closing;
    } else if name.eq_ignore_ascii_case("b") {
        open.style.bold = !closing;
    } else if name.eq_ignore_ascii_case("u") {
        open.style.underline = !closing;
    } else if name.eq_ignore_ascii_case("font") && !closing {
        open.lose(Loss::Styling);
    }
}

/// `inner` is an override block without its braces: `\i1\b1`, `\an8` and so
/// on. Each backslash-separated token is one tag.
fn block(inner: &str, open: &mut Open) {
    for token in inner.split('\\').map(str::trim).filter(|t| !t.is_empty()) {
        match token {
            "i1" => open.style.italic = true,
            "i0" => open.style.italic = false,
            "b1" => open.style.bold = true,
            "b0" => open.style.bold = false,
            "u1" => open.style.underline = true,
            "u0" => open.style.underline = false,
            // Numpad 7, 8, 9: the top row. The bottom and middle rows are the
            // default placement, so their alignment is dropped without a report.
            "an7" | "an8" | "an9" => open.top = true,
            "an1" | "an2" | "an3" | "an4" | "an5" | "an6" => {}
            "p0" => open.drawing = false,
            t if t.starts_with("pos(") || t.starts_with("move(") => open.lose(Loss::Position),
            t if t.starts_with('k') => open.lose(Loss::Karaoke),
            t if t
                .strip_prefix('p')
                .is_some_and(|n| n.parse::<u32>().is_ok_and(|v| v != 0)) =>
            {
                open.drawing = true;
                open.lose(Loss::Drawing);
            }
            _ => open.lose(Loss::Styling),
        }
    }
}

/// Appends text in `style`, merging it into the last run when the style is
/// the same, so a line has one run per style change. A new run is charged to
/// the budget before its text is copied.
pub(crate) fn push_text(
    runs: &mut Vec<Run>,
    style: Style,
    text: &str,
    budget: &mut Budget,
) -> Result<(), ReadError> {
    if text.is_empty() {
        return Ok(());
    }
    match runs.last_mut() {
        Some(run)
            if (run.italic, run.bold, run.underline)
                == (style.italic, style.bold, style.underline) =>
        {
            run.text.push_str(text);
        }
        _ => {
            budget.charge()?;
            runs.push(Run {
                text: text.to_owned(),
                italic: style.italic,
                bold: style.bold,
                underline: style.underline,
            });
        }
    }
    Ok(())
}
