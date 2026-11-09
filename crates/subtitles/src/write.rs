use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::rc::Rc;

use crate::tag::look_alike_at;
use crate::time::write_time;
use crate::{Cue, Line, Loss, Run, has_text};

/// UTF-8 byte order mark. Older TVs and Windows tools read an SRT as UTF-8
/// only when it starts with one; WebVTT is written without it, as its
/// standard says.
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Writes text with each line break as a space (a blank line would end the
/// cue) and, for WebVTT, escapes `&`, `<` and `>`. Escaping `>` also keeps
/// `-->` out of the text, where it would read as a timing line. A NUL is
/// written as U+FFFD: WebVTT's parse rule reads it so, and the SRT reader
/// would keep the raw byte, which no text file can show.
fn write_text(out: &mut impl Write, text: &str, vtt: bool) -> io::Result<()> {
    let mut start = 0;
    for (i, c) in text.char_indices() {
        let replacement = match c {
            '\r' | '\n' => " ",
            '\0' => "\u{fffd}",
            '&' if vtt => "&amp;",
            '<' if vtt => "&lt;",
            '>' if vtt => "&gt;",
            _ => continue,
        };
        out.write_all(&text.as_bytes()[start..i])?;
        out.write_all(replacement.as_bytes())?;
        start = i + c.len_utf8();
    }
    out.write_all(&text.as_bytes()[start..])
}

/// Writes a line's runs. Tags open in the order i, b, u and close in reverse,
/// so they nest whatever the mix. A run without text gets no tags.
fn write_runs(out: &mut impl Write, line: &Line, vtt: bool) -> io::Result<()> {
    for run in line.runs.iter().filter(|r| !r.text.is_empty()) {
        if run.italic {
            out.write_all(b"<i>")?;
        }
        if run.bold {
            out.write_all(b"<b>")?;
        }
        if run.underline {
            out.write_all(b"<u>")?;
        }
        write_text(out, &run.text, vtt)?;
        if run.underline {
            out.write_all(b"</u>")?;
        }
        if run.bold {
            out.write_all(b"</b>")?;
        }
        if run.italic {
            out.write_all(b"</i>")?;
        }
    }
    Ok(())
}

/// The line as the SRT reader reads it back. Each `<` that opens a tag and each
/// `{` that opens an override block (the reader's own rules, `tag.rs`) becomes
/// a look-alike that no reader takes for markup. The decision is made on the
/// line's plain text, the speaker's `Name: ` prefix included, so a bracket at
/// the end of a run is judged by the next run's first characters, as the reader
/// will see them. The line is borrowed when no bracket needs changing.
fn srt_safe(line: &Line) -> Cow<'_, Line> {
    let mut pieces: Vec<&str> = Vec::new();
    if let Some(name) = &line.voice {
        pieces.push(name);
        pieces.push(": ");
    }
    pieces.extend(line.runs.iter().map(|r| r.text.as_str()));
    let plain = pieces.concat();
    if !plain
        .char_indices()
        .any(|(i, _)| look_alike_at(&plain, i).is_some())
    {
        return Cow::Borrowed(line);
    }
    // Each piece starts at `at` in the plain text; its brackets are judged there.
    let mut at = 0;
    let mut next = |text: &str| {
        let fixed = text
            .char_indices()
            .map(|(i, c)| look_alike_at(&plain, at + i).unwrap_or(c))
            .collect::<String>();
        at += text.len();
        fixed
    };
    let voice: Option<Rc<str>> = line.voice.as_deref().map(|name| Rc::from(next(name)));
    if line.voice.is_some() {
        next(": ");
    }
    let runs = line
        .runs
        .iter()
        .map(|r| Run {
            text: next(&r.text),
            italic: r.italic,
            bold: r.bold,
            underline: r.underline,
        })
        .collect();
    Cow::Owned(Line { voice, runs })
}

/// Writes cues as SRT: a BOM, CRLF, numbers from 1 over the cues written, a
/// speaker as a `Name: ` prefix, and `{\an8}` before the first line of a top
/// cue. Cues are written in the given order. Cues with no text write nothing,
/// and when none has text the output is empty, not even the BOM.
///
/// Returns the loss counts: the cues that had a speaker, since the names
/// became prefixes. The caller passes a buffered writer, as this issues many
/// small writes.
pub fn write_srt(cues: &[Cue], out: &mut impl Write) -> io::Result<BTreeMap<Loss, usize>> {
    let mut voiced = 0;
    let mut brackets = 0;
    let mut number = 0;
    for cue in cues.iter().filter(|c| c.lines.iter().any(has_text)) {
        if number == 0 {
            out.write_all(BOM)?;
        }
        number += 1;
        write!(out, "{number}\r\n")?;
        write_time(out, cue.start_ms, ',')?;
        out.write_all(b" --> ")?;
        write_time(out, cue.end_ms, ',')?;
        out.write_all(b"\r\n")?;
        let mut has_voice = false;
        let mut looked = false;
        for (i, line) in cue.lines.iter().filter(|l| has_text(l)).enumerate() {
            let safe = srt_safe(line);
            looked |= matches!(safe, Cow::Owned(_));
            let line = &*safe;
            if cue.top && i == 0 {
                out.write_all(b"{\\an8}")?;
            }
            if let Some(name) = &line.voice {
                has_voice = true;
                write_text(out, name, false)?;
                out.write_all(b": ")?;
            }
            write_runs(out, line, false)?;
            out.write_all(b"\r\n")?;
        }
        out.write_all(b"\r\n")?;
        voiced += usize::from(has_voice);
        brackets += usize::from(looked);
    }
    let mut losses = BTreeMap::new();
    if voiced > 0 {
        losses.insert(Loss::VoiceNames, voiced);
    }
    if brackets > 0 {
        losses.insert(Loss::Brackets, brackets);
    }
    Ok(losses)
}

/// Writes cues as WebVTT: a `WEBVTT` header, no BOM, LF, and a blank line
/// after each cue. A top cue gets ` line:0`, and a speaker is a `<v Name>` tag
/// around the line. Cues are written in the given order. Nothing is lost
/// writing WebVTT, so the returned map is empty.
pub fn write_vtt(cues: &[Cue], out: &mut impl Write) -> io::Result<BTreeMap<Loss, usize>> {
    out.write_all(b"WEBVTT\n\n")?;
    for cue in cues.iter().filter(|c| c.lines.iter().any(has_text)) {
        write_time(out, cue.start_ms, '.')?;
        out.write_all(b" --> ")?;
        write_time(out, cue.end_ms, '.')?;
        if cue.top {
            out.write_all(b" line:0")?;
        }
        out.write_all(b"\n")?;
        for line in cue.lines.iter().filter(|l| has_text(l)) {
            if let Some(name) = &line.voice {
                out.write_all(b"<v ")?;
                write_text(out, name, true)?;
                out.write_all(b">")?;
            }
            write_runs(out, line, true)?;
            if line.voice.is_some() {
                out.write_all(b"</v>")?;
            }
            out.write_all(b"\n")?;
        }
        out.write_all(b"\n")?;
    }
    Ok(BTreeMap::new())
}
