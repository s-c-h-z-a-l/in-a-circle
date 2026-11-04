//! Windows-1250 and ISO-8859-2 to UTF-8, streamed. Strict: a byte the
//! code page does not assign, or a control byte, gives no output and an
//! error with the byte's offset (from 0); nothing is ever replaced by
//! U+FFFD. The upper 128 bytes map by the Unicode Consortium's files
//! (`VENDORS/MICSFT/WINDOWS/CP1250.TXT`, `ISO8859/8859-2.TXT`), written
//! out below and checked byte for byte against copies of the files by
//! `tests/codepage.rs`.
//!
//! A file that is already UTF-8 is refused too: valid UTF-8 with at least
//! one multi-byte character is `AlreadyUtf8` (decoding it would turn `é`
//! into `Ă©`), and so is a file that is mostly UTF-8 with a few damaged
//! bytes (`DamagedUtf8`): over the whole input, as many valid multi-byte
//! UTF-8 sequences as bytes outside valid UTF-8, or more. Real 8-bit text
//! almost never forms one: its accented letter is followed by ASCII, which
//! is never a UTF-8 continuation byte. That verdict needs the whole input,
//! so the output is written as the input streams and the caller throws it
//! away on any error.

use std::fmt;
use std::io::{self, Read, Write};

/// The read size: 64 KiB, so the decoder's memory is this buffer plus an
/// output buffer of three times it (one byte of either code page is at
/// most three UTF-8 bytes), about 256 KiB whatever the input's size. A
/// larger chunk would only cut the number of read and write calls, which
/// at this size are already a small share of the per-byte work.
pub const CHUNK: usize = 64 * 1024;

/// The most UTF-8 bytes one input byte becomes: every character of both
/// code pages is in the Basic Multilingual Plane (at most U+FFFF).
const MAX_UTF8: usize = 3;

/// The UTF-8 byte order mark a `.srt` output starts with (players on old
/// TVs and Windows read a subtitle without one as the local code page).
pub const BOM: &[u8; 3] = b"\xEF\xBB\xBF";

/// Ctrl-Z, the old DOS end-of-file mark: allowed once, as the input's very
/// last byte, and kept (U+001A).
const CTRL_Z: u8 = 0x1A;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codepage {
    Cp1250,
    Iso88592,
}

impl Codepage {
    /// The name the user sees (the slice label, the error and notice copy).
    pub fn name(self) -> &'static str {
        match self {
            Codepage::Cp1250 => "Windows-1250",
            Codepage::Iso88592 => "ISO-8859-2",
        }
    }

    /// The character the mapping file gives `byte`, `None` for a byte it
    /// leaves undefined. The lower half is ASCII in both. This is the
    /// file's mapping, not what the decoder accepts: ISO-8859-2 maps
    /// `0x80-0x9F` to the C1 controls, which the decoder refuses.
    pub fn mapping(self, byte: u8) -> Option<char> {
        if byte < 0x80 {
            return Some(char::from(byte));
        }
        let table = match self {
            Codepage::Cp1250 => &CP1250,
            Codepage::Iso88592 => &ISO8859_2,
        };
        // NUL marks an undefined byte: no upper byte maps to U+0000.
        Some(table[usize::from(byte - 0x80)]).filter(|&c| c != '\0')
    }
}

/// Why an input gave no output.
#[derive(Debug)]
pub enum CodepageError {
    /// A byte the code page does not assign.
    NotThisCodepage {
        cp: Codepage,
        byte: u8,
        at: u64,
    },
    /// An ISO-8859-2 C1 byte (`0x80-0x9F`) that Windows-1250 assigns: the
    /// file is almost surely Windows-1250 (its quotes and dashes are there).
    LooksLikeCp1250 {
        byte: u8,
        at: u64,
    },
    /// NUL or a C0 control other than TAB, LF, FF and CR (and a Ctrl-Z
    /// that is not the last byte): not text.
    Control {
        at: u64,
    },
    /// Valid UTF-8 with a multi-byte character: nothing to convert.
    AlreadyUtf8,
    /// Mostly UTF-8 with damaged bytes; `at` is the first damaged byte.
    DamagedUtf8 {
        at: u64,
    },
    Io(io::Error),
}

impl fmt::Display for CodepageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodepageError::NotThisCodepage { cp, byte, at } => {
                write!(f, "byte 0x{byte:02X} at {at} is not {}", cp.name())
            }
            CodepageError::LooksLikeCp1250 { byte, at } => {
                write!(f, "byte 0x{byte:02X} at {at}: this looks like Windows-1250")
            }
            CodepageError::Control { at } => write!(f, "not text: control byte at {at}"),
            CodepageError::AlreadyUtf8 => f.write_str("already UTF-8"),
            CodepageError::DamagedUtf8 { at } => {
                write!(f, "looks like UTF-8 with damaged bytes (first at {at})")
            }
            CodepageError::Io(e) => write!(f, "{e}"),
        }
    }
}

/// A C0 control byte that text does not carry: all of `0x00-0x1F` but TAB,
/// LF, FF and CR (Ctrl-Z is decided by its place, by the caller).
fn is_control(b: u8) -> bool {
    b < 0x20 && !matches!(b, b'\t' | b'\n' | 0x0C | b'\r')
}

/// An incremental UTF-8 checker (Unicode Table 3-7, well-formed byte
/// sequences: no overlongs, no surrogates, nothing past U+10FFFF) that
/// counts the valid multi-byte sequences and the bytes outside any valid
/// sequence, across chunk boundaries.
#[derive(Default)]
struct Utf8Census {
    /// Continuation bytes still needed by the open sequence.
    need: u8,
    /// The range the next continuation byte must be in.
    lo: u8,
    hi: u8,
    /// Where the open sequence started.
    start: u64,
    valid: u64,
    invalid: u64,
    first_invalid: Option<u64>,
}

impl Utf8Census {
    fn bad(&mut self, at: u64, n: u64) {
        self.invalid += n;
        self.first_invalid.get_or_insert(at);
    }

    fn push(&mut self, b: u8, at: u64) {
        if self.need > 0 {
            if (self.lo..=self.hi).contains(&b) {
                self.need -= 1;
                (self.lo, self.hi) = (0x80, 0xBF);
                if self.need == 0 {
                    self.valid += 1;
                }
                return;
            }
            // The open sequence is broken: its bytes are damaged, and `b`
            // may start the next one.
            self.bad(self.start, at - self.start);
            self.need = 0;
        }
        let (need, lo, hi) = match b {
            0x00..=0x7F => return,
            0xC2..=0xDF => (1, 0x80, 0xBF),
            0xE0 => (2, 0xA0, 0xBF),
            0xED => (2, 0x80, 0x9F),
            0xE1..=0xEF => (2, 0x80, 0xBF),
            0xF0 => (3, 0x90, 0xBF),
            0xF4 => (3, 0x80, 0x8F),
            0xF1..=0xF3 => (3, 0x80, 0xBF),
            // A continuation byte alone, an overlong lead or past U+10FFFF.
            _ => return self.bad(at, 1),
        };
        (self.need, self.lo, self.hi, self.start) = (need, lo, hi, at);
    }

    /// At the end of `len` bytes: a sequence still open is damaged.
    fn finish(&mut self, len: u64) {
        if self.need > 0 {
            self.bad(self.start, len - self.start);
            self.need = 0;
        }
    }
}

/// Decodes `input` from `cp` to UTF-8 into `output`, streamed in
/// [`CHUNK`]s, with [`BOM`] first when `srt_bom`; the bytes written. Line
/// ends and a final Ctrl-Z are kept byte for byte. On any error the bytes
/// already written are the caller's to throw away (the verdict on a
/// mostly-UTF-8 file comes only at the end). A control byte or an I/O
/// error stops at once; a byte the code page refuses stops the output but
/// the rest is still read for the UTF-8 verdict, which wins over it: valid
/// UTF-8 such as `Á` (`C3 81`) is `AlreadyUtf8`, not a refused `0x81`.
pub fn decode(
    cp: Codepage,
    input: &mut impl Read,
    output: &mut impl Write,
    srt_bom: bool,
) -> Result<u64, CodepageError> {
    let mut buf = vec![0u8; CHUNK];
    let mut out: Vec<u8> = Vec::with_capacity(CHUNK * MAX_UTF8);
    let mut written = 0u64;
    if srt_bom {
        output.write_all(BOM).map_err(CodepageError::Io)?;
        written += BOM.len() as u64;
    }
    let mut census = Utf8Census::default();
    let mut refused: Option<CodepageError> = None;
    let mut ctrl_z: Option<u64> = None;
    let mut pos = 0u64;
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(CodepageError::Io(e)),
        };
        for &b in &buf[..n] {
            let at = pos;
            pos += 1;
            // A Ctrl-Z with anything after it is not the end mark.
            if let Some(z) = ctrl_z {
                return Err(CodepageError::Control { at: z });
            }
            if b == CTRL_Z {
                ctrl_z = Some(at);
            } else if is_control(b) {
                return Err(CodepageError::Control { at });
            }
            census.push(b, at);
            if refused.is_some() {
                continue;
            }
            if b < 0x80 {
                out.push(b);
                continue;
            }
            let accepted = match cp {
                Codepage::Iso88592 if b < 0xA0 => None,
                _ => cp.mapping(b),
            };
            match accepted {
                Some(c) => {
                    let mut tmp = [0u8; MAX_UTF8];
                    out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                }
                None => {
                    refused = Some(match cp {
                        Codepage::Iso88592 if Codepage::Cp1250.mapping(b).is_some() => {
                            CodepageError::LooksLikeCp1250 { byte: b, at }
                        }
                        _ => CodepageError::NotThisCodepage { cp, byte: b, at },
                    });
                    out.clear();
                }
            }
        }
        if !out.is_empty() {
            output.write_all(&out).map_err(CodepageError::Io)?;
            written += out.len() as u64;
            out.clear();
        }
    }
    census.finish(pos);
    if census.invalid == 0 && census.valid > 0 {
        return Err(CodepageError::AlreadyUtf8);
    }
    if census.invalid > 0 && census.valid >= census.invalid {
        let at = census.first_invalid.unwrap_or(0);
        return Err(CodepageError::DamagedUtf8 { at });
    }
    if let Some(e) = refused {
        return Err(e);
    }
    output.flush().map_err(CodepageError::Io)?;
    Ok(written)
}

/// `0x80-0xFF` of `CP1250.TXT`; NUL marks the five bytes it leaves
/// undefined (`0x81 0x83 0x88 0x90 0x98`).
const CP1250: [char; 128] = [
    '\u{20AC}', '\0', '\u{201A}', '\0', '\u{201E}', '\u{2026}', '\u{2020}',
    '\u{2021}', // 0x80
    '\0', '\u{2030}', '\u{0160}', '\u{2039}', '\u{015A}', '\u{0164}', '\u{017D}',
    '\u{0179}', // 0x88
    '\0', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}',
    '\u{2014}', // 0x90
    '\0', '\u{2122}', '\u{0161}', '\u{203A}', '\u{015B}', '\u{0165}', '\u{017E}',
    '\u{017A}', // 0x98
    '\u{00A0}', '\u{02C7}', '\u{02D8}', '\u{0141}', '\u{00A4}', '\u{0104}', '\u{00A6}',
    '\u{00A7}', // 0xA0
    '\u{00A8}', '\u{00A9}', '\u{015E}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}',
    '\u{017B}', // 0xA8
    '\u{00B0}', '\u{00B1}', '\u{02DB}', '\u{0142}', '\u{00B4}', '\u{00B5}', '\u{00B6}',
    '\u{00B7}', // 0xB0
    '\u{00B8}', '\u{0105}', '\u{015F}', '\u{00BB}', '\u{013D}', '\u{02DD}', '\u{013E}',
    '\u{017C}', // 0xB8
    '\u{0154}', '\u{00C1}', '\u{00C2}', '\u{0102}', '\u{00C4}', '\u{0139}', '\u{0106}',
    '\u{00C7}', // 0xC0
    '\u{010C}', '\u{00C9}', '\u{0118}', '\u{00CB}', '\u{011A}', '\u{00CD}', '\u{00CE}',
    '\u{010E}', // 0xC8
    '\u{0110}', '\u{0143}', '\u{0147}', '\u{00D3}', '\u{00D4}', '\u{0150}', '\u{00D6}',
    '\u{00D7}', // 0xD0
    '\u{0158}', '\u{016E}', '\u{00DA}', '\u{0170}', '\u{00DC}', '\u{00DD}', '\u{0162}',
    '\u{00DF}', // 0xD8
    '\u{0155}', '\u{00E1}', '\u{00E2}', '\u{0103}', '\u{00E4}', '\u{013A}', '\u{0107}',
    '\u{00E7}', // 0xE0
    '\u{010D}', '\u{00E9}', '\u{0119}', '\u{00EB}', '\u{011B}', '\u{00ED}', '\u{00EE}',
    '\u{010F}', // 0xE8
    '\u{0111}', '\u{0144}', '\u{0148}', '\u{00F3}', '\u{00F4}', '\u{0151}', '\u{00F6}',
    '\u{00F7}', // 0xF0
    '\u{0159}', '\u{016F}', '\u{00FA}', '\u{0171}', '\u{00FC}', '\u{00FD}', '\u{0163}',
    '\u{02D9}', // 0xF8
];

/// `0x80-0xFF` of `8859-2.TXT` (all defined; `0x80-0x9F` are the C1
/// controls U+0080-U+009F).
const ISO8859_2: [char; 128] = [
    '\u{0080}', '\u{0081}', '\u{0082}', '\u{0083}', '\u{0084}', '\u{0085}', '\u{0086}',
    '\u{0087}', // 0x80
    '\u{0088}', '\u{0089}', '\u{008A}', '\u{008B}', '\u{008C}', '\u{008D}', '\u{008E}',
    '\u{008F}', // 0x88
    '\u{0090}', '\u{0091}', '\u{0092}', '\u{0093}', '\u{0094}', '\u{0095}', '\u{0096}',
    '\u{0097}', // 0x90
    '\u{0098}', '\u{0099}', '\u{009A}', '\u{009B}', '\u{009C}', '\u{009D}', '\u{009E}',
    '\u{009F}', // 0x98
    '\u{00A0}', '\u{0104}', '\u{02D8}', '\u{0141}', '\u{00A4}', '\u{013D}', '\u{015A}',
    '\u{00A7}', // 0xA0
    '\u{00A8}', '\u{0160}', '\u{015E}', '\u{0164}', '\u{0179}', '\u{00AD}', '\u{017D}',
    '\u{017B}', // 0xA8
    '\u{00B0}', '\u{0105}', '\u{02DB}', '\u{0142}', '\u{00B4}', '\u{013E}', '\u{015B}',
    '\u{02C7}', // 0xB0
    '\u{00B8}', '\u{0161}', '\u{015F}', '\u{0165}', '\u{017A}', '\u{02DD}', '\u{017E}',
    '\u{017C}', // 0xB8
    '\u{0154}', '\u{00C1}', '\u{00C2}', '\u{0102}', '\u{00C4}', '\u{0139}', '\u{0106}',
    '\u{00C7}', // 0xC0
    '\u{010C}', '\u{00C9}', '\u{0118}', '\u{00CB}', '\u{011A}', '\u{00CD}', '\u{00CE}',
    '\u{010E}', // 0xC8
    '\u{0110}', '\u{0143}', '\u{0147}', '\u{00D3}', '\u{00D4}', '\u{0150}', '\u{00D6}',
    '\u{00D7}', // 0xD0
    '\u{0158}', '\u{016E}', '\u{00DA}', '\u{0170}', '\u{00DC}', '\u{00DD}', '\u{0162}',
    '\u{00DF}', // 0xD8
    '\u{0155}', '\u{00E1}', '\u{00E2}', '\u{0103}', '\u{00E4}', '\u{013A}', '\u{0107}',
    '\u{00E7}', // 0xE0
    '\u{010D}', '\u{00E9}', '\u{0119}', '\u{00EB}', '\u{011B}', '\u{00ED}', '\u{00EE}',
    '\u{010F}', // 0xE8
    '\u{0111}', '\u{0144}', '\u{0148}', '\u{00F3}', '\u{00F4}', '\u{0151}', '\u{00F6}',
    '\u{00F7}', // 0xF0
    '\u{0159}', '\u{016F}', '\u{00FA}', '\u{0171}', '\u{00FC}', '\u{00FD}', '\u{0163}',
    '\u{02D9}', // 0xF8
];
