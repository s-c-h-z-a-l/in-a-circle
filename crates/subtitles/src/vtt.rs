//! The WebVTT reader. It follows the W3C file parsing algorithm as browsers
//! do (`tests/vtt.rs` checks it against the web-platform-tests files), with
//! these choices of ours:
//!
//! - Signature: `WEBVTT` after the byte order mark, then a space, a tab, a
//!   line end or the end of the file. The rest of that line is ignored. Any
//!   other start is not WebVTT: the file has no cues.
//! - Lines end at LF, CR or CRLF. A block ends at an empty line; a line of
//!   spaces is text.
//! - The header (the lines after the signature, up to the first empty line)
//!   is metadata and is ignored silently. A timing line in it starts a cue.
//! - `NOTE` blocks are comments and are not reported. `STYLE` and `REGION`
//!   blocks are not cues; every cue after one loses its styling, reported as
//!   `Settings`. In a `NOTE`, `STYLE` or `REGION` block, or in a block already
//!   reported, a valid timing line starts a cue, as the standard does: a line
//!   with `-->` ends a block past its second line (or past its first once an
//!   arrow was seen), and the standard reads that line again as the next
//!   block's first line. The one difference is a `-->` that is no timing line:
//!   the standard ends the block at it too, but we keep it in the block. Neither
//!   reading starts a cue from it, and only the timing lines of the block are
//!   read.
//! - A cue is a timing line, after an optional identifier line, then its text.
//!   A line with `-->` ends the text of a cue, as in the standard: a timing
//!   line there starts the next cue. Any other such line is dropped, and the
//!   line after it may be the timing line. The dropped line is counted as
//!   skipped. The writers escape `>` and keep dashes off a speaker's name's
//!   end, so their output has no such line.
//! - Timing: `[H+:]MM:SS.mmm`, hours of any length, minutes and seconds 00 to
//!   59, milliseconds exactly three digits. A cue that ends before it starts,
//!   or is 100 hours or more, is skipped, as in the SRT reader.
//! - A `line:` setting in the top third places the cue at the top (see
//!   `TOP_ROW` and `TOP_PERCENT`). Any other valid placement and every other
//!   setting is a `Position` loss. Unknown setting names are ignored.
//! - Cue text: `<i> <b> <u>` as in SRT. `<v Name>` gives the line its
//!   speaker (`<v.class Name>` too: the classes are not part of the name; the
//!   name is decoded, then trimmed of ASCII white space only), and
//!   `</v>` ends it, so text after `</v>` on a line is a line of its own.
//!   `<c.class>` and `<v.class>` are `Styling`, a timestamp tag is `Karaoke`
//!   (its text stays).
//!   Other tags are dropped and their text kept. `&amp;`, `&lt;`, `&gt;`,
//!   `&quot;`, `&apos;`, `&lrm;`, `&rlm;` and `&nbsp;` are decoded, and so are
//!   numeric references (`&#N;`, `&#xH;`) to any valid character but NUL and
//!   the line breaks; any other `&name;` is text. A NUL is U+FFFD.
//!
//! Standard library only. Every loop moves forward, so the work is linear in
//! the input: a `<` with no `>` later in its line is text, and the failed
//! search for a `>` is remembered so the line is searched once.

use std::collections::BTreeMap;

use std::rc::Rc;

use crate::srt::{Fault, STRAY, Style, lines, number, push_text};
use crate::{Budget, Cue, Line, Loss, MAX_MS, Parsed, ReadError, Run, Skipped, has_text};

const MS_PER_SECOND: u64 = 1_000;
const MS_PER_MINUTE: u64 = 60 * MS_PER_SECOND;
const MS_PER_HOUR: u64 = 60 * MS_PER_MINUTE;

/// A whole-row `line:` value up to this row places the cue at the top. Rows 0
/// to 4 are the top third of a 15-row view, the view the picture is laid out
/// in, so a cue there is a top cue.
const TOP_ROW: u64 = 4;

/// A `line:` percentage up to this value places the cue at the top. 33 % is
/// the top third of the picture, as with [`TOP_ROW`]; a cue lower down cannot
/// be carried over, so it is a `Position` loss. A fraction past it is lower
/// (33.4 % is not top), and a fraction of zero is not.
const TOP_PERCENT: u64 = 33;

/// Reads a whole WebVTT text (byte order mark already removed). `None` when
/// the text does not start with the signature. Cues come out in file order.
pub(crate) fn parse(text: &str, budget: &mut Budget) -> Result<Option<Parsed>, ReadError> {
    let mut all = lines(text);
    let Some(signature) = all.next() else {
        return Ok(None);
    };
    let signed = signature == "WEBVTT"
        || signature.starts_with("WEBVTT ")
        || signature.starts_with("WEBVTT\t");
    if !signed {
        return Ok(None);
    }
    let mut reader = Reader::default();
    for (i, line) in all.enumerate() {
        // The signature is line 1, so the first line after it is line 2.
        reader.line(i + 2, line, budget)?;
    }
    reader.finish(budget).map(Some)
}

/// Where the reader stands in the current block. A block is the lines between
/// two empty lines, and only its first lines decide whether it is a cue.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Block {
    /// The header: the lines after the signature.
    #[default]
    Header,
    /// The next line is the first line of a block.
    Start,
    /// The first line is text or an identifier with no `-->`; the next line
    /// must be the timing line. Holds the block's first line number.
    Id(usize),
    /// The first line has `-->` but is no timing line; the next line may be
    /// the timing line. Holds the block's first line number.
    Dropped(usize),
    /// Nothing more is read from this block: a comment, a style or region,
    /// or a block already reported. A valid timing line in it still starts a
    /// cue (see `line`).
    Ignored,
}

#[derive(Default)]
struct Reader {
    cues: Vec<Cue>,
    losses: BTreeMap<Loss, usize>,
    skipped: Skipped,
    /// The cue being read, from its timing line to its last text line.
    open: Option<Open>,
    block: Block,
    /// A `STYLE` or `REGION` block was read: cues from here on lose styling.
    styled: bool,
}

/// The cue being read.
struct Open {
    at_line: usize,
    span: Result<(u64, u64), Fault>,
    style: Style,
    /// The speaker in effect. It carries across the lines of the cue until
    /// `</v>`, as the standard's cue text does.
    voice: Option<Rc<str>>,
    top: bool,
    /// Losses in this cue, each counted once per cue.
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

/// A timing line split into its parts. `span` is the cue's span, or the fault
/// that keeps it from being used; `settings` is the text after the end time.
struct Timing<'a> {
    span: Result<(u64, u64), Fault>,
    settings: &'a str,
}

impl Reader {
    /// `n` is the 1-based line number and `line` has no line end.
    fn line(&mut self, n: usize, line: &str, budget: &mut Budget) -> Result<(), ReadError> {
        if line.is_empty() {
            return self.blank(budget);
        }
        if self.open.is_some() {
            // Cue text. A line with `-->` ends the cue, and is then read as the
            // first line of a block: a timing line starts the next cue.
            if line.contains("-->") {
                self.close(budget)?;
                self.first_line(n, line);
            } else {
                self.text(line, budget)?;
            }
            return Ok(());
        }
        match self.block {
            Block::Header => {
                if line.contains("-->") {
                    self.first_timing(n, line);
                }
            }
            Block::Start => self.first_line(n, line),
            Block::Id(first) => match timing(line) {
                Some(t) => self.start(n, t),
                None if line.contains("-->") => {
                    self.skip(n, Fault::Format.message());
                    self.block = Block::Ignored;
                }
                None => {
                    self.skip(first, STRAY);
                    self.block = Block::Ignored;
                }
            },
            Block::Dropped(first) => {
                // Counted here whether or not a timing line follows it.
                self.skip(first, Fault::Format.message());
                match timing(line) {
                    Some(t) => self.start(n, t),
                    None => self.block = Block::Ignored,
                }
            }
            // A valid timing line ends an ignored block, so a missing blank
            // line does not swallow the cue after it. Any other line is not read.
            Block::Ignored => {
                if let Some(t) = timing(line) {
                    self.start(n, t);
                }
            }
        }
        Ok(())
    }

    /// The first line of a block that is not the header.
    fn first_line(&mut self, n: usize, line: &str) {
        if line.trim_end_matches([' ', '\t']) == "STYLE" || line == "REGION" {
            self.styled = true;
            self.block = Block::Ignored;
        } else if is_note(line) {
            self.block = Block::Ignored;
        } else {
            match timing(line) {
                Some(t) => self.start(n, t),
                None if line.contains("-->") => self.block = Block::Dropped(n),
                None => self.block = Block::Id(n),
            }
        }
    }

    /// A header line with `-->`: a timing line starts a cue; a broken one is
    /// reported and the rest of the header is not read.
    fn first_timing(&mut self, n: usize, line: &str) {
        match timing(line) {
            Some(t) => self.start(n, t),
            None => {
                self.skip(n, Fault::Format.message());
                self.block = Block::Ignored;
            }
        }
    }

    /// An empty line: it ends the open cue, or the block that was not a cue.
    fn blank(&mut self, budget: &mut Budget) -> Result<(), ReadError> {
        if self.open.is_some() {
            self.close(budget)?;
        } else {
            match self.block {
                Block::Id(first) => self.skip(first, STRAY),
                Block::Dropped(first) => self.skip(first, Fault::Format.message()),
                Block::Header | Block::Start | Block::Ignored => {}
            }
        }
        self.block = Block::Start;
        Ok(())
    }

    fn start(&mut self, n: usize, t: Timing) {
        let mut open = Open {
            at_line: n,
            span: t.span,
            style: Style::default(),
            voice: None,
            top: false,
            lost: Vec::new(),
            lines: Vec::new(),
        };
        if self.styled {
            open.lose(Loss::Settings);
        }
        for token in t.settings.split_ascii_whitespace() {
            setting(token, &mut open);
        }
        self.open = Some(open);
        self.block = Block::Ignored;
    }

    fn text(&mut self, line: &str, budget: &mut Budget) -> Result<(), ReadError> {
        if let Some(open) = self.open.as_mut() {
            markup(line, open, budget)?;
        }
        Ok(())
    }

    /// Ends the open cue. A cue with no text is not stored, and its losses are
    /// not counted: WebVTT has no drawing, and its styling, position and
    /// karaoke took no text.
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

    fn skip(&mut self, line: usize, reason: &'static str) {
        self.skipped.count += 1;
        // Skips are recorded in line order, so the first one is the earliest.
        if self.skipped.first.is_none() {
            self.skipped.first = Some((line, reason));
        }
    }

    fn finish(mut self, budget: &mut Budget) -> Result<Parsed, ReadError> {
        self.blank(budget)?;
        Ok(Parsed {
            cues: self.cues,
            losses: self.losses,
            skipped: self.skipped,
        })
    }
}

fn is_note(line: &str) -> bool {
    line == "NOTE" || line.starts_with("NOTE ") || line.starts_with("NOTE\t")
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\u{c}')
}

/// A timing line: `start --> end [settings]`, whitespace allowed around the
/// arrow. `None` when the line is not one, so its `-->` is text.
fn timing(line: &str) -> Option<Timing<'_>> {
    if !line.contains("-->") {
        return None;
    }
    let (from, rest) = timestamp(line.trim_start_matches(is_space))?;
    let rest = rest.trim_start_matches(is_space).strip_prefix("-->")?;
    let (to, settings) = timestamp(rest.trim_start_matches(is_space))?;
    let span = if from > MAX_MS || to > MAX_MS {
        Err(Fault::Late)
    } else if to < from {
        Err(Fault::Order)
    } else {
        Ok((from, to))
    };
    Some(Timing { span, settings })
}

/// `[H+:]MM:SS.mmm` at the start of `s`, in milliseconds, with the text after
/// it. Hours of any length, saturating; minutes and seconds 00 to 59.
fn timestamp(s: &str) -> Option<(u64, &str)> {
    let len = s
        .bytes()
        .take_while(|b| b.is_ascii_digit() || matches!(b, b':' | b'.'))
        .count();
    let (text, rest) = s.split_at(len);
    let mut fields = text.split(':');
    let (hours, minutes, seconds_millis) =
        match (fields.next(), fields.next(), fields.next(), fields.next()) {
            (Some(m), Some(sm), None, None) => (0, m, sm),
            (Some(h), Some(m), Some(sm), None) => (number(h)?, m, sm),
            _ => return None,
        };
    let (seconds, millis) = seconds_millis.split_once('.')?;
    let minutes = exact(minutes, 2)?;
    let seconds = exact(seconds, 2)?;
    let millis = exact(millis, 3)?;
    if minutes > 59 || seconds > 59 {
        return None;
    }
    let ms = hours
        .saturating_mul(MS_PER_HOUR)
        .saturating_add(minutes * MS_PER_MINUTE + seconds * MS_PER_SECOND + millis);
    Some((ms, rest))
}

/// `s` as a number when it is exactly `digits` decimal digits.
fn exact(s: &str, digits: usize) -> Option<u64> {
    if s.len() == digits { number(s) } else { None }
}

/// Applies one setting token (`name:value`). `line` in the top third is a top
/// cue; any other valid `line` value and the placement settings are a
/// `Position` loss. A value the standard does not accept is ignored.
fn setting(token: &str, open: &mut Open) {
    let Some((name, value)) = token.split_once(':') else {
        return;
    };
    match name {
        "line" => match line_value(value) {
            Some(true) => open.top = true,
            Some(false) => open.lose(Loss::Position),
            None => {}
        },
        "align" | "position" | "size" | "vertical" | "region" if !value.is_empty() => {
            open.lose(Loss::Position);
        }
        _ => {}
    }
}

/// The `line:` value: `None` when it is not valid, else whether it is in the
/// top third. The value is a number or a percentage, optionally followed by
/// `,start`, `,center` or `,end`; the alignment does not move the cue.
fn line_value(value: &str) -> Option<bool> {
    let position = match value.split_once(',') {
        Some((position, align)) => {
            if !matches!(align, "start" | "center" | "end") {
                return None;
            }
            position
        }
        None => value,
    };
    if let Some(percent) = position.strip_suffix('%') {
        let (whole, fraction) = decimal(percent)?;
        // Exact: 33.0 % is the top edge, 33.4 % is past it, with no float.
        let at_edge = whole == TOP_PERCENT && fraction.is_none_or(|f| f.bytes().all(|b| b == b'0'));
        return Some(whole < TOP_PERCENT || at_edge);
    }
    let (negative, digits) = match position.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, position),
    };
    let (whole, fraction) = decimal(digits)?;
    // A negative row counts from the bottom, so only -0 is a top row.
    Some(fraction.is_none() && whole <= TOP_ROW && (!negative || whole == 0))
}

/// `digits` or `digits.digits`: the whole part as a number, and the fraction's
/// digits when there is a point. An empty part, or any other character, is
/// `None`.
fn decimal(s: &str) -> Option<(u64, Option<&str>)> {
    match s.split_once('.') {
        Some((whole, fraction)) => {
            number(fraction)?;
            Some((number(whole)?, Some(fraction)))
        }
        None => Some((number(s)?, None)),
    }
}

/// Search state for one line. Once a search for `>` fails, no later `<` on
/// the line can close either, so it is not searched again.
#[derive(Default)]
struct Scan {
    no_gt: bool,
}

/// The named entities the reader decodes: the markup characters, the two
/// quotes, the two marks and the no-break space. Any other `&name;` is text as
/// written; the full HTML table is not needed for subtitles.
const ENTITIES: [(&str, char); 8] = [
    ("&amp;", '&'),
    ("&lt;", '<'),
    ("&gt;", '>'),
    ("&quot;", '"'),
    ("&apos;", '\''),
    ("&lrm;", '\u{200e}'),
    ("&rlm;", '\u{200f}'),
    ("&nbsp;", '\u{a0}'),
];

/// The most digits a numeric reference may have. Eight cover every value up to
/// U+10FFFF with leading zeros, and fit in `u32`, so the parse cannot overflow.
/// A longer run is text. It also bounds the work: each `&` reads at most this
/// many digits, whatever follows it.
const MAX_REFERENCE_DIGITS: usize = 8;

/// The character the entity at the start of `tail` decodes to, and the
/// entity's length in bytes. A numeric reference is `&#N;`, `&#xH;` or `&#XH;`
/// and decodes to a valid character except NUL and the line breaks, which
/// stay as written: the readers split lines at a line break, and a NUL is not
/// text.
fn entity(tail: &str) -> Option<(char, usize)> {
    if let Some(&(name, c)) = ENTITIES.iter().find(|(name, _)| tail.starts_with(name)) {
        return Some((c, name.len()));
    }
    let body = tail.strip_prefix("&#")?;
    let (digits, radix) = match body.strip_prefix(['x', 'X']) {
        Some(hex) => (hex, 16),
        None => (body, 10),
    };
    let len = digits
        .bytes()
        .take(MAX_REFERENCE_DIGITS + 1)
        .take_while(|b| b.is_ascii_digit() || (radix == 16 && b.is_ascii_hexdigit()))
        .count();
    if len == 0 || len > MAX_REFERENCE_DIGITS || !digits[len..].starts_with(';') {
        return None;
    }
    let value = u32::from_str_radix(&digits[..len], radix).ok()?;
    let c = char::from_u32(value).filter(|c| !matches!(c, '\0' | '\n' | '\r'))?;
    // The text up to the digits (`&#`, and `x` for hex), the digits, the `;`.
    Some((c, tail.len() - digits.len() + len + 1))
}

/// `s` with its entities decoded, for voice names.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let len = match entity(tail) {
            Some((c, len)) => {
                out.push(c);
                len
            }
            None => {
                out.push('&');
                1
            }
        };
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

/// Applies the markup of one cue text line to the open cue. A speaker change
/// ends a line of the output, so each line of the output has one speaker.
fn markup(line: &str, open: &mut Open, budget: &mut Budget) -> Result<(), ReadError> {
    let mut runs = Vec::new();
    let mut scan = Scan::default();
    let mut rest = line;
    while let Some(i) = rest.find(['<', '&']) {
        push(&mut runs, open.style, &rest[..i], budget)?;
        let tail = &rest[i..];
        let len = if tail.starts_with('<') {
            match tag_len(tail, &mut scan) {
                Some(len) => {
                    tag(&tail[..len], open, &mut runs, budget)?;
                    len
                }
                None => {
                    push(&mut runs, open.style, "<", budget)?;
                    1
                }
            }
        } else {
            match entity(tail) {
                Some((c, len)) => {
                    let mut buf = [0; 4];
                    push(&mut runs, open.style, c.encode_utf8(&mut buf), budget)?;
                    len
                }
                None => {
                    push(&mut runs, open.style, "&", budget)?;
                    1
                }
            }
        };
        rest = &tail[len..];
    }
    push(&mut runs, open.style, rest, budget)?;
    flush(&mut runs, open, budget)
}

/// Adds text in `style`. A NUL is U+FFFD, as the standard says.
fn push(
    runs: &mut Vec<Run>,
    style: Style,
    text: &str,
    budget: &mut Budget,
) -> Result<(), ReadError> {
    if text.contains('\0') {
        push_text(runs, style, &text.replace('\0', "\u{fffd}"), budget)
    } else {
        push_text(runs, style, text, budget)
    }
}

/// Ends the current line of the output with the runs collected so far. The
/// line is charged before it is stored. The speaker is shared, not copied.
fn flush(runs: &mut Vec<Run>, open: &mut Open, budget: &mut Budget) -> Result<(), ReadError> {
    if !runs.is_empty() {
        budget.charge()?;
        open.lines.push(Line {
            voice: open.voice.clone(),
            runs: std::mem::take(runs),
        });
    }
    Ok(())
}

/// Length of a tag starting at `tail`: `<`, an optional `/`, a letter or a
/// digit, then up to and including the next `>`.
fn tag_len(tail: &str, scan: &mut Scan) -> Option<usize> {
    let body = tail[1..].strip_prefix('/').unwrap_or(&tail[1..]);
    if scan.no_gt || !body.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return None;
    }
    let len = tail.find('>').map(|i| i + 1);
    if len.is_none() {
        scan.no_gt = true;
    }
    len
}

/// Applies one whole tag (`<…>`) to the cue. Tags with no effect on the
/// output are dropped and their text stays.
fn tag(
    markup: &str,
    open: &mut Open,
    runs: &mut Vec<Run>,
    budget: &mut Budget,
) -> Result<(), ReadError> {
    let inner = &markup[1..markup.len() - 1];
    let (closing, body) = match inner.strip_prefix('/') {
        Some(body) => (true, body),
        None => (false, inner),
    };
    if !closing && timestamp(body).is_some_and(|(_, rest)| rest.is_empty()) {
        open.lose(Loss::Karaoke);
        return Ok(());
    }
    let name_end = body
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(body.len());
    let (name, annotation) = body.split_at(name_end);
    if name.eq_ignore_ascii_case("i") {
        open.style.italic = !closing;
    } else if name.eq_ignore_ascii_case("b") {
        open.style.bold = !closing;
    } else if name.eq_ignore_ascii_case("u") {
        open.style.underline = !closing;
    } else if name.eq_ignore_ascii_case("v") {
        flush(runs, open, budget)?;
        // A class on a voice is styling, as on `<c.class>`.
        if !closing && annotation.starts_with('.') {
            open.lose(Loss::Styling);
        }
        open.voice = if closing {
            None
        } else {
            // The classes (`.a.b`) end at the first white space; the name follows.
            let who = match annotation.strip_prefix('.') {
                Some(classes) => classes
                    .find(|c: char| c.is_ascii_whitespace())
                    .map_or("", |i| &classes[i..]),
                None => annotation,
            };
            // The standard decodes the references first, then trims ASCII white
            // space only: a no-break space is part of the name.
            let who = unescape(who);
            let who = who.trim_matches(|c: char| c.is_ascii_whitespace());
            (!who.is_empty()).then(|| Rc::from(who))
        };
    } else if name.eq_ignore_ascii_case("c") && !closing && annotation.starts_with('.') {
        open.lose(Loss::Styling);
    }
    Ok(())
}
