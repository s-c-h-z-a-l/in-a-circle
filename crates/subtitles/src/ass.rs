//! The ASS and SSA reader. It follows libass for the text, the times and the
//! styling that carries over. The choices of
//! ours are listed here; the differential test in `tests/ass.rs` checks the
//! shared behaviour against ffmpeg:
//!
//! - Sections: `[Script Info]`, `[V4+ Styles]`, `[V4 Styles]` and `[Events]`,
//!   matched without regard to case or trailing spaces. Other sections are
//!   read past. `WrapStyle` in `[Script Info]` decides what `\n` means.
//! - Columns follow each section's `Format:` line, in whatever order it gives.
//!   Without one, the usual order is used. An Events `Format:` must name
//!   `Start` and `End` and end with `Text`; a Dialogue under one that does not
//!   is skipped and counted, since its text cannot be found.
//! - `Dialogue:` lines are cues; the text is the last column with its commas.
//!   `Comment:` lines are dropped silently. `Picture`, `Sound`, `Movie` and
//!   `Command` lines are not text and are read past. Any other line in
//!   `[Events]` is text outside any cue, counted once per line.
//! - Times: `H+:MM:SS.cc`, minutes and seconds one or two digits (a minute of
//!   60 carries, as in the SRT reader), and one or two digits of centiseconds.
//! - Styles: `Bold`, `Italic` and `Underline` take `-1`, `0` or `1`; a bold
//!   weight of 600 or more is bold. The alignment is the cue's position: on
//!   V4+ 7, 8 and 9 are the top row; in `[V4 Styles]` (and `\a`) 5, 6 and 7 are.
//!   An unknown style name falls back to `Default`, then to no style.
//! - Overrides: `\i \b \u` switch styling, `\an` and `\a` name a top row,
//!   `\r` and `\r<Style>` return to a style. A cue is one position, so it is
//!   top when any part of it is at the top, as in the SRT reader; a bottom
//!   override does not move a top cue down. `\N`
//!   breaks the line; `\n` breaks it with `WrapStyle: 2` and is a space
//!   otherwise; `\h` is a no-break space. `\k` is karaoke (the text stays);
//!   `\p1` to `\p0` is drawing (its text is dropped); `\pos`, `\move` and
//!   `\org` are `Position`; every other tag is `Styling`. A `{` block with no
//!   closing `}` is text.
//!
//! Standard library only. Every loop moves forward, so the work is linear in
//! the input: the Format columns are found once per Format line, not once per
//! line they describe, and a `{` or `\` is never searched for twice.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use crate::srt::{Fault, STRAY, Style, lines, number, push_text};
use crate::{Budget, Cue, Line, Loss, MAX_MS, Parsed, ReadError, Run, Skipped};

const MS_PER_SECOND: u64 = 1_000;
const MS_PER_MINUTE: u64 = 60 * MS_PER_SECOND;
const MS_PER_HOUR: u64 = 60 * MS_PER_MINUTE;
/// One unit of an ASS centisecond field.
const MS_PER_CENTISECOND: u64 = 10;

/// A font weight from this up reads as bold. ASS and SSA files carry no fixed
/// bold weight, and 600 is the usual cut-off of renderers (700 is the usual
/// bold weight itself).
const BOLD_WEIGHT: i64 = 600;

/// The style a Dialogue names but the file does not define falls back to this.
const DEFAULT_STYLE: &str = "Default";

/// The column order a `[V4+ Styles]` section has when its `Format:` line is
/// missing, as libass assumes.
const STYLE_FORMAT_V4_PLUS: &str = "Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding";

/// The column order of a `[V4 Styles]` section without a `Format:` line.
const STYLE_FORMAT_V4: &str = "Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, TertiaryColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding";

/// The column order of `[Events]` without a `Format:` line.
const EVENT_FORMAT: &str =
    "Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text";

/// The alignment numbers of each scheme. V4+ and `\an` use the numpad (1 to 9);
/// SSA v4 and `\a` use their own (1 to 11). The top row is 7 to 9 in the
/// numpad and 5 to 7 in SSA.
const NUMPAD: RangeInclusive<u8> = 1..=9;
const NUMPAD_TOP: RangeInclusive<u8> = 7..=9;
const SSA: RangeInclusive<u8> = 1..=11;
const SSA_TOP: RangeInclusive<u8> = 5..=7;

/// The section a line is read in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    /// Any section this reader does not read, and the lines before the first.
    Other,
    Info,
    /// `ssa` is true for `[V4 Styles]`, whose alignment uses the SSA scheme.
    Styles {
        ssa: bool,
    },
    Events,
}

/// Where each field of a `Style:` line is, from its section's `Format:` line.
#[derive(Clone, Copy)]
struct StyleColumns {
    name: Option<usize>,
    bold: Option<usize>,
    italic: Option<usize>,
    underline: Option<usize>,
    alignment: Option<usize>,
}

impl StyleColumns {
    fn of(format: &str) -> Self {
        Self {
            name: column(format, "Name"),
            bold: column(format, "Bold"),
            italic: column(format, "Italic"),
            underline: column(format, "Underline"),
            alignment: column(format, "Alignment"),
        }
    }
}

/// Where each field of a `Dialogue:` line is. The text is the last of the
/// `count` columns, and everything after the last comma belongs to it.
#[derive(Clone, Copy)]
struct EventColumns {
    count: usize,
    start: Option<usize>,
    end: Option<usize>,
    style: Option<usize>,
    text_last: bool,
}

impl EventColumns {
    fn of(format: &str) -> Self {
        let count = format.split(',').count();
        Self {
            count,
            start: column(format, "Start"),
            end: column(format, "End"),
            style: column(format, "Style"),
            text_last: column(format, "Text") == Some(count - 1),
        }
    }

    /// Without these, a Dialogue line cannot be read: its times or its text
    /// have no column to come from.
    fn usable(self) -> bool {
        self.text_last && self.start.is_some() && self.end.is_some()
    }
}

/// The position of a name in a comma-separated `Format:` value, if any.
fn column(format: &str, name: &str) -> Option<usize> {
    format
        .split(',')
        .position(|c| c.trim().eq_ignore_ascii_case(name))
}

/// A style as far as cues are concerned: its styling, and whether it sits at
/// the top.
#[derive(Clone, Copy, Default)]
struct Base {
    style: Style,
    top: bool,
}

/// Reads a whole ASS or SSA text (byte order mark already removed). Cues come
/// out in file order.
pub(crate) fn parse(text: &str, budget: &mut Budget) -> Result<Parsed, ReadError> {
    let mut reader = Reader::new();
    for (i, line) in lines(text).enumerate() {
        reader.line(i + 1, line.trim(), budget)?;
    }
    Ok(Parsed {
        cues: reader.cues,
        losses: reader.losses,
        skipped: reader.skipped,
    })
}

struct Reader<'a> {
    section: Section,
    /// `WrapStyle: 2`: `\n` breaks the line.
    wrap_newlines: bool,
    styles: BTreeMap<&'a str, Base>,
    style_columns: StyleColumns,
    event_columns: EventColumns,
    cues: Vec<Cue>,
    losses: BTreeMap<Loss, usize>,
    skipped: Skipped,
}

impl<'a> Reader<'a> {
    fn new() -> Self {
        Self {
            section: Section::Other,
            wrap_newlines: false,
            styles: BTreeMap::new(),
            style_columns: StyleColumns::of(STYLE_FORMAT_V4_PLUS),
            event_columns: EventColumns::of(EVENT_FORMAT),
            cues: Vec::new(),
            losses: BTreeMap::new(),
            skipped: Skipped::default(),
        }
    }

    /// `line` is trimmed; `n` is its 1-based line number.
    fn line(&mut self, n: usize, line: &'a str, budget: &mut Budget) -> Result<(), ReadError> {
        if let Some(name) = section_name(line) {
            self.section = section_of(name);
            // Each section starts with its default columns; its own Format line
            // replaces them.
            match self.section {
                Section::Styles { ssa } => {
                    let format = if ssa {
                        STYLE_FORMAT_V4
                    } else {
                        STYLE_FORMAT_V4_PLUS
                    };
                    self.style_columns = StyleColumns::of(format);
                }
                Section::Events => self.event_columns = EventColumns::of(EVENT_FORMAT),
                Section::Other | Section::Info => {}
            }
            return Ok(());
        }
        match self.section {
            Section::Info => {
                if let Some(value) = after_key(line, "WrapStyle") {
                    self.wrap_newlines = value.trim() == "2";
                }
            }
            Section::Styles { ssa } => {
                if let Some(format) = after_key(line, "Format") {
                    self.style_columns = StyleColumns::of(format);
                } else if let Some(body) = after_key(line, "Style") {
                    self.style(body, ssa);
                }
            }
            Section::Events => {
                if let Some(format) = after_key(line, "Format") {
                    self.event_columns = EventColumns::of(format);
                } else if let Some(body) = after_key(line, "Dialogue") {
                    self.dialogue(n, body, budget)?;
                } else if !is_known_event_line(line) && !line.is_empty() {
                    self.skip(n, STRAY);
                }
            }
            Section::Other => {}
        }
        Ok(())
    }

    /// One `Style:` line, read by the section's columns. A field that is not
    /// a valid value leaves the style plain, so the style still exists.
    fn style(&mut self, body: &'a str, ssa: bool) {
        let c = self.style_columns;
        let mut name = "";
        let mut base = Base::default();
        for (i, field) in body.split(',').map(str::trim).enumerate() {
            if c.name == Some(i) {
                name = field;
            }
            if c.bold == Some(i) {
                base.style.bold = switch(field, true).unwrap_or(false);
            }
            if c.italic == Some(i) {
                base.style.italic = switch(field, false).unwrap_or(false);
            }
            if c.underline == Some(i) {
                base.style.underline = switch(field, false).unwrap_or(false);
            }
            if c.alignment == Some(i) {
                base.top = field.parse().is_ok_and(|a| top_row(a, ssa));
            }
        }
        self.styles.insert(name, base);
    }

    /// One `Dialogue:` line. `body` is the text after the key.
    fn dialogue(&mut self, n: usize, body: &str, budget: &mut Budget) -> Result<(), ReadError> {
        let c = self.event_columns;
        if !c.usable() {
            self.skip(n, "unusable Events Format line");
            return Ok(());
        }
        let (mut start, mut end, mut style, mut text) = (None, None, "", None);
        for (i, field) in body.splitn(c.count, ',').enumerate() {
            if c.start == Some(i) {
                start = Some(field.trim());
            }
            if c.end == Some(i) {
                end = Some(field.trim());
            }
            if c.style == Some(i) {
                style = field.trim();
            }
            if i + 1 == c.count {
                text = Some(field);
            }
        }
        let (Some(start), Some(end), Some(text)) = (start, end, text) else {
            self.skip(n, "too few fields");
            return Ok(());
        };
        let (start_ms, end_ms) = match span(start, end) {
            Ok(span) => span,
            Err(fault) => {
                self.skip(n, fault.message());
                return Ok(());
            }
        };
        let home = self.base(style);
        let scene = self.scene(text, home, budget)?;
        // A cue with no text is not stored: it has nothing to show. A drawing
        // removed from it is still text the conversion dropped, so it is
        // counted. Styling, position and karaoke took no text, so they are not.
        if scene.lines.is_empty() {
            if scene.lost.contains(&Loss::Drawing) {
                *self.losses.entry(Loss::Drawing).or_insert(0) += 1;
            }
            return Ok(());
        }
        budget.charge()?;
        for loss in scene.lost {
            *self.losses.entry(loss).or_insert(0) += 1;
        }
        self.cues.push(Cue {
            start_ms,
            end_ms,
            top: scene.top,
            lines: scene.lines,
        });
        Ok(())
    }

    /// The style a name refers to: the named one, else `Default`, else plain.
    fn base(&self, name: &str) -> Base {
        self.styles
            .get(name)
            .or_else(|| self.styles.get(DEFAULT_STYLE))
            .copied()
            .unwrap_or_default()
    }

    /// Splits the text of one Dialogue line into its lines and runs, applying
    /// its overrides. `home` is the cue's own style, which `\r` returns to.
    fn scene(&self, text: &str, home: Base, budget: &mut Budget) -> Result<Scene, ReadError> {
        let mut scene = Scene::new(home, self.wrap_newlines);
        // Once a search for `}` fails, no later `{` can close either.
        let mut no_close = false;
        let mut rest = text;
        while let Some(i) = rest.find(['{', '\\']) {
            scene.emit(&rest[..i], budget)?;
            let tail = &rest[i..];
            if let Some(after) = tail.strip_prefix('{') {
                if !no_close {
                    if let Some(end) = tail.find('}') {
                        for_each_tag(&tail[1..end], |tag| self.tag(tag, &mut scene));
                        rest = &tail[end + 1..];
                        continue;
                    }
                    no_close = true;
                }
                scene.emit("{", budget)?;
                rest = after;
                continue;
            }
            // `\N` breaks the line; `\n` breaks it only with WrapStyle 2 (a soft
            // break) and is a space otherwise; `\h` is a no-break space. Any
            // other backslash is text.
            match tail.as_bytes().get(1) {
                Some(b'N') => {
                    scene.break_line(budget)?;
                    rest = &tail[2..];
                }
                Some(b'n') => {
                    if scene.wrap {
                        scene.break_line(budget)?;
                    } else {
                        scene.emit(" ", budget)?;
                    }
                    rest = &tail[2..];
                }
                Some(b'h') => {
                    scene.emit("\u{a0}", budget)?;
                    rest = &tail[2..];
                }
                _ => {
                    scene.emit("\\", budget)?;
                    rest = &tail[1..];
                }
            }
        }
        scene.emit(rest, budget)?;
        scene.break_line(budget)?;
        Ok(scene)
    }

    /// One override tag (without its backslash) applied to the scene.
    fn tag(&self, tag: &str, scene: &mut Scene) {
        // `\r` is the only tag whose name starts with an `r`, so the rest of the
        // tag is a style name.
        if let Some(name) = tag.strip_prefix('r') {
            let name = name.trim();
            let base = if name.is_empty() {
                scene.home
            } else {
                self.base(name)
            };
            scene.style = base.style;
            scene.top |= base.top;
            return;
        }
        let (name, value) = split_name(tag);
        match name {
            "i" | "b" | "u" => match switch(value, name == "b") {
                Some(on) => match name {
                    "i" => scene.style.italic = on,
                    "b" => scene.style.bold = on,
                    _ => scene.style.underline = on,
                },
                None => scene.lose(Loss::Styling),
            },
            "an" => match value.parse::<u8>() {
                Ok(a) if NUMPAD.contains(&a) => scene.top |= NUMPAD_TOP.contains(&a),
                _ => scene.lose(Loss::Styling),
            },
            "a" => match value.parse::<u8>() {
                Ok(a) if SSA.contains(&a) => scene.top |= SSA_TOP.contains(&a),
                _ => scene.lose(Loss::Styling),
            },
            "p" => match value.parse::<u32>() {
                Ok(0) => scene.drawing = false,
                Ok(_) => {
                    scene.drawing = true;
                    scene.lose(Loss::Drawing);
                }
                Err(_) => scene.lose(Loss::Styling),
            },
            "pos" | "move" | "org" => scene.lose(Loss::Position),
            // `\k`, `\kf`, `\ko` and `\K` are the karaoke timings.
            n if n.starts_with(['k', 'K']) => scene.lose(Loss::Karaoke),
            _ => scene.lose(Loss::Styling),
        }
    }

    fn skip(&mut self, line: usize, reason: &'static str) {
        self.skipped.count += 1;
        if self.skipped.first.is_none_or(|(first, _)| line < first) {
            self.skipped.first = Some((line, reason));
        }
    }
}

/// The state of one Dialogue line while its text is read.
struct Scene {
    /// The cue's own style, which `\r` returns to.
    home: Base,
    wrap: bool,
    style: Style,
    /// The cue is at the top at some point in its text, so it stays top.
    top: bool,
    /// Between `\p1` and `\p0` the text is drawing commands, not subtitle text.
    drawing: bool,
    lost: Vec<Loss>,
    lines: Vec<Line>,
    runs: Vec<Run>,
}

impl Scene {
    fn new(home: Base, wrap: bool) -> Self {
        Self {
            home,
            wrap,
            style: home.style,
            top: home.top,
            drawing: false,
            lost: Vec::new(),
            lines: Vec::new(),
            runs: Vec::new(),
        }
    }

    fn emit(&mut self, text: &str, budget: &mut Budget) -> Result<(), ReadError> {
        if !self.drawing {
            push_text(&mut self.runs, self.style, text, budget)?;
        }
        Ok(())
    }

    fn break_line(&mut self, budget: &mut Budget) -> Result<(), ReadError> {
        if !self.runs.is_empty() {
            budget.charge()?;
            self.lines.push(Line {
                voice: None,
                runs: std::mem::take(&mut self.runs),
            });
        }
        Ok(())
    }

    /// Each loss is counted once per cue, so it is recorded once here.
    fn lose(&mut self, loss: Loss) {
        if !self.lost.contains(&loss) {
            self.lost.push(loss);
        }
    }
}

/// The section a bracketed line names, without its brackets, or `None`.
fn section_name(line: &str) -> Option<&str> {
    line.strip_prefix('[')?.strip_suffix(']')
}

fn section_of(name: &str) -> Section {
    let name = name.trim();
    if name.eq_ignore_ascii_case("Script Info") {
        Section::Info
    } else if name.eq_ignore_ascii_case("V4+ Styles") {
        Section::Styles { ssa: false }
    } else if name.eq_ignore_ascii_case("V4 Styles") {
        Section::Styles { ssa: true }
    } else if name.eq_ignore_ascii_case("Events") {
        Section::Events
    } else {
        Section::Other
    }
}

/// The text after `key:` when the line starts with that key, in any case.
fn after_key<'l>(line: &'l str, key: &str) -> Option<&'l str> {
    let head = line.get(..key.len())?;
    if !head.eq_ignore_ascii_case(key) {
        return None;
    }
    line[key.len()..].strip_prefix(':')
}

/// Lines in `[Events]` that are not text: a comment, and the picture, sound,
/// movie and command events, which carry no subtitle text.
fn is_known_event_line(line: &str) -> bool {
    ["Comment", "Picture", "Sound", "Movie", "Command"]
        .iter()
        .any(|key| after_key(line, key).is_some())
}

/// An on/off value: `-1` or `1` is on, `0` is off. For bold (`weight`), any
/// other whole number is a font weight and is bold from [`BOLD_WEIGHT`] up.
/// `None` for anything else, so the caller reports it.
fn switch(value: &str, weight: bool) -> Option<bool> {
    match value.parse::<i64>().ok()? {
        -1 | 1 => Some(true),
        0 => Some(false),
        w if weight => Some(w >= BOLD_WEIGHT),
        _ => None,
    }
}

/// Whether a `Style` alignment puts the cue at the top. `ssa` is the scheme of
/// its section: `[V4 Styles]` uses SSA, `[V4+ Styles]` the numpad.
fn top_row(alignment: u8, ssa: bool) -> bool {
    if ssa {
        SSA_TOP.contains(&alignment)
    } else {
        NUMPAD_TOP.contains(&alignment)
    }
}

/// Splits a tag into its name, the leading letters, and its value. A tag that
/// starts with a digit (`1c`) has an empty name, which the styling arm takes.
fn split_name(tag: &str) -> (&str, &str) {
    let letters = tag.bytes().take_while(|b| b.is_ascii_alphabetic()).count();
    tag.split_at(letters)
}

/// Calls `f` with each tag of an override block, without its backslash. Text
/// before the first backslash is not a tag. A backslash inside parentheses
/// (`\t(\fs20\bord2)`) belongs to its tag.
fn for_each_tag(inner: &str, mut f: impl FnMut(&str)) {
    let mut depth = 0usize;
    let mut start = None;
    for (i, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '\\' if depth == 0 => {
                if let Some(s) = start {
                    f(&inner[s..i]);
                }
                start = Some(i + 1);
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        f(&inner[s..]);
    }
}

/// `start` and `end` as a span in milliseconds. A span that ends before it
/// starts, or runs past the limit, is a fault.
fn span(start: &str, end: &str) -> Result<(u64, u64), Fault> {
    let from = clock(start)?;
    let to = clock(end)?;
    if to < from {
        Err(Fault::Order)
    } else {
        Ok((from, to))
    }
}

/// `H+:MM:SS.cc`. Hours take any number of digits, saturating like the SRT
/// reader's, so an absurd hour reads as past the limit. Minutes and seconds
/// take one or two digits. Centiseconds take one or two: three digits are
/// refused, because ffmpeg refuses them and a cue we cannot place exactly is
/// counted instead of guessed.
fn clock(s: &str) -> Result<u64, Fault> {
    let mut fields = s.split(':');
    let (Some(h), Some(m), Some(rest), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err(Fault::Format);
    };
    let Some((sec, cs)) = rest.split_once('.') else {
        return Err(Fault::Format);
    };
    let hours = number(h).ok_or(Fault::Format)?;
    let minutes = digits(m, 2).ok_or(Fault::Format)?;
    let seconds = digits(sec, 2).ok_or(Fault::Format)?;
    let centis = digits(cs, 2).ok_or(Fault::Format)?;
    // Minutes, seconds and centiseconds are at most 99, so their products
    // cannot overflow; the hours are checked.
    hours
        .checked_mul(MS_PER_HOUR)
        .and_then(|t| t.checked_add(minutes * MS_PER_MINUTE))
        .and_then(|t| t.checked_add(seconds * MS_PER_SECOND))
        .and_then(|t| t.checked_add(centis * MS_PER_CENTISECOND))
        .filter(|&t| t <= MAX_MS)
        .ok_or(Fault::Late)
}

/// A number of at most `max` digits.
fn digits(s: &str, max: usize) -> Option<u64> {
    if s.len() <= max { number(s) } else { None }
}
