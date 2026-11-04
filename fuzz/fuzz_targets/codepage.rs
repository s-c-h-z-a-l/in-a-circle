#![no_main]
//! The code page decoders (`text.cp1250-to-utf8`, `text.iso88592-to-utf8`)
//! on arbitrary bytes: the shipped `codepage::decode`, included from the
//! action's own source (std only). The first byte picks the code page,
//! the `.srt` byte order mark and a read size, the rest is the file.
//! Asserts what the action promises on every input: the same verdict
//! however the file is read (whole, or in reads of 1 to 253 bytes); a
//! converted file is valid UTF-8, within the engine's cap (3 x the input
//! + 3 B), with the byte order mark exactly when asked, without U+FFFD,
//! and encodes back to the input byte for byte; every error names a byte
//! that really is what it says at that offset; the UTF-8 verdicts agree
//! with `std::str::from_utf8` (the first damaged offset is its
//! `valid_up_to`).

use libfuzzer_sys::fuzz_target;
use std::collections::HashMap;
use std::io::{self, Cursor, Read};

#[path = "../../bins/inacircle-text/src/codepage.rs"]
mod codepage;

use codepage::{BOM, Codepage, CodepageError, decode};

/// A reader that hands out at most `step` bytes per call.
struct Trickle<'a> {
    data: &'a [u8],
    step: usize,
}

impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.data.len().min(self.step).min(buf.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

fn is_control(b: u8) -> bool {
    b < 0x20 && !matches!(b, b'\t' | b'\n' | 0x0C | b'\r')
}

fuzz_target!(|data: &[u8]| {
    let Some((&sel, input)) = data.split_first() else {
        return;
    };
    let cp = if sel & 1 == 0 {
        Codepage::Cp1250
    } else {
        Codepage::Iso88592
    };
    let srt = sel & 2 != 0;
    let step = usize::from(sel >> 2) * 4 + 1;

    let mut whole = Vec::new();
    let got = decode(cp, &mut Cursor::new(input), &mut whole, srt);
    let mut trickled = Vec::new();
    let again = decode(cp, &mut Trickle { data: input, step }, &mut trickled, srt);
    // The same verdict whatever the read size.
    assert_eq!(
        got.as_ref().map_err(|e| e.to_string()),
        again.as_ref().map_err(|e| e.to_string())
    );

    let utf8 = std::str::from_utf8(input);
    match got {
        Ok(n) => {
            assert_eq!(n, whole.len() as u64);
            assert_eq!(whole, trickled);
            assert!(whole.len() as u64 <= 3 * input.len() as u64 + BOM.len() as u64);
            let body = if srt {
                whole
                    .strip_prefix(&BOM[..])
                    .expect("a .srt starts with the BOM")
            } else {
                &whole[..]
            };
            let text = std::str::from_utf8(body).expect("valid UTF-8");
            assert!(!text.contains('\u{FFFD}'));
            // Encoded back with the table: the input, byte for byte.
            let back: HashMap<char, u8> = (0..=255u8)
                .filter_map(|b| cp.mapping(b).map(|c| (c, b)))
                .collect();
            let bytes: Vec<u8> = text.chars().map(|c| back[&c]).collect();
            assert_eq!(bytes, input);
            // Every byte accepted: mapped, and no ISO-8859-2 C1 byte.
            let c1 = |b: u8| cp == Codepage::Iso88592 && (0x80..0xA0).contains(&b);
            assert!(input.iter().all(|&b| cp.mapping(b).is_some() && !c1(b)));
            // Text: no control byte, a Ctrl-Z only last.
            let last = input.len().saturating_sub(1);
            assert!(
                input
                    .iter()
                    .enumerate()
                    .all(|(i, &b)| !is_control(b) || (b == 0x1A && i == last))
            );
            // Only ASCII may be valid UTF-8 and still convert.
            assert!(utf8.is_err() || input.is_ascii());
        }
        Err(CodepageError::NotThisCodepage {
            cp: named,
            byte,
            at,
        }) => {
            assert_eq!(named, cp);
            assert_eq!(input[at as usize], byte);
            let c1 = cp == Codepage::Iso88592 && (0x80..0xA0).contains(&byte);
            assert!(cp.mapping(byte).is_none() || c1);
            assert!(!c1 || Codepage::Cp1250.mapping(byte).is_none());
            assert!(utf8.is_err());
        }
        Err(CodepageError::LooksLikeCp1250 { byte, at }) => {
            assert_eq!(cp, Codepage::Iso88592);
            assert_eq!(input[at as usize], byte);
            assert!((0x80..0xA0).contains(&byte));
            assert!(Codepage::Cp1250.mapping(byte).is_some());
            assert!(utf8.is_err());
        }
        Err(CodepageError::Control { at }) => {
            let at = at as usize;
            let b = input[at];
            assert!(is_control(b));
            assert!(b != 0x1A || at + 1 < input.len());
            // The first: a Ctrl-Z before it would have been one too.
            assert!(!input[..at].iter().any(|&b| is_control(b)));
        }
        Err(CodepageError::AlreadyUtf8) => {
            assert!(utf8.is_ok() && !input.is_ascii());
        }
        Err(CodepageError::DamagedUtf8 { at }) => {
            let e = utf8.expect_err("damaged UTF-8 is not valid UTF-8");
            assert_eq!(at as usize, e.valid_up_to());
        }
        Err(CodepageError::Io(e)) => panic!("I/O error from memory: {e}"),
    }
});
