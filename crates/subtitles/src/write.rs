use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::rc::Rc;

use crate::srt::is_timing_line;
use crate::tag::look_alikes;
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

/// The text as `write_text` writes it: each CR or LF is one space.
fn spaced(text: &str) -> Cow<'_, str> {
    if text.contains(['\r', '\n']) {
        Cow::Owned(text.replace(['\r', '\n'], " "))
    } else {
        Cow::Borrowed(text)
    }
}

/// The line as the SRT reader reads it back. Each `<` the reader would take as a
/// tag and each `{` it would take as an override block becomes a look-alike
/// that no reader takes for markup (`tag::look_alikes`, the reader's own rules).
/// The decision is made on the line's plain text as the writer writes it (line
/// breaks as spaces, see [`spaced`]), the speaker's `Name: ` prefix included, so
/// a bracket at the end of a run is judged by the text after it in the next
/// run. The writer's style tags are not in that text, so at a style boundary a
/// bracket the reader would keep may be rewritten (`tag::look_alikes`); the
/// writer never leaves one the reader would take as markup. The line is
/// borrowed when no bracket needs changing.
fn srt_safe(line: &Line) -> Cow<'_, Line> {
    let mut pieces: Vec<Cow<str>> = Vec::new();
    if let Some(name) = &line.voice {
        pieces.push(spaced(name));
        pieces.push(Cow::Borrowed(": "));
    }
    pieces.extend(line.runs.iter().map(|r| spaced(&r.text)));
    let plain = pieces.concat();
    let looks = look_alikes(&plain);
    if looks.is_empty() {
        return Cow::Borrowed(line);
    }
    // Each piece starts at `at` in the plain text; its brackets are looked up there.
    let mut at = 0;
    let mut next = |text: &str| {
        let fixed = text
            .char_indices()
            .map(|(i, c)| {
                let found = looks.binary_search_by_key(&(at + i), |&(p, _)| p);
                found.map_or(c, |k| looks[k].1)
            })
            .collect::<String>();
        at += text.len();
        fixed
    };
    let voice: Option<Rc<str>> = line
        .voice
        .as_deref()
        .map(|name| Rc::from(next(&spaced(name))));
    if line.voice.is_some() {
        next(": ");
    }
    let runs = line
        .runs
        .iter()
        .map(|r| Run {
            text: next(&spaced(&r.text)),
            italic: r.italic,
            bold: r.bold,
            underline: r.underline,
        })
        .collect();
    Cow::Owned(Line { voice, runs })
}

/// Writes one text line of an SRT cue, its `{\an8}` marker first when it is the
/// top cue's first line. The line is rendered first, so the reader's own timing
/// test sees what the file will hold (marker, speaker and tags included). A
/// line the reader would take for a timing line starts a cue on read back, so
/// the `>` of each `-->` in it is written as U+FF1E, a look-alike. Returns
/// whether it did that.
fn write_srt_line(out: &mut impl Write, line: &Line, top: bool) -> io::Result<bool> {
    let mut buf = Vec::new();
    if top {
        buf.extend_from_slice(b"{\\an8}");
    }
    if let Some(name) = &line.voice {
        write_text(&mut buf, name, false)?;
        buf.extend_from_slice(b": ");
    }
    write_runs(&mut buf, line, false)?;
    // Everything in `buf` was written from a `&str` or is ASCII, so it is
    // UTF-8 and the lossy conversion never changes a byte.
    let text = String::from_utf8_lossy(&buf);
    let timing = is_timing_line(text.trim());
    if timing {
        out.write_all(text.replace("-->", "--\u{ff1e}").as_bytes())?;
    } else {
        out.write_all(&buf)?;
    }
    out.write_all(b"\r\n")?;
    Ok(timing)
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
            has_voice |= safe.voice.is_some();
            looked |= write_srt_line(out, &safe, cue.top && i == 0)?;
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
                // The `>` that closes the tag follows the name, so trailing
                // dashes would make `-->` and end the cue: they are written
                // as `&#45;`, which reads back as `-`.
                let bare = name.trim_end_matches('-');
                write_text(out, bare, true)?;
                for _ in bare.len()..name.len() {
                    out.write_all(b"&#45;")?;
                }
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
