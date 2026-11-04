//! The two code page decoders (`text.cp1250-to-utf8`,
//! `text.iso88592-to-utf8`): the tables against the Unicode Consortium's
//! mapping files and against glibc's `iconv`, the streaming decoder's
//! rules (strict bytes, control bytes, line ends, Ctrl-Z, the `.srt` byte
//! order mark, the UTF-8 verdicts) and the action end to end.
//!
//! A missing `iconv` skips its test, unless `INACIRCLE_REQUIRE_PDF_TOOLS=1`
//! (the switch for every local test-time tool) makes it a failure.

#[path = "../src/codepage.rs"]
mod codepage;

use std::fs;
use std::io::{self, BufRead, BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use codepage::{BOM, CHUNK, Codepage, CodepageError, decode};

const BOTH: [Codepage; 2] = [Codepage::Cp1250, Codepage::Iso88592];

/// The five bytes `CP1250.TXT` leaves undefined.
const CP1250_UNDEFINED: [u8; 5] = [0x81, 0x83, 0x88, 0x90, 0x98];

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/unicode-mappings")
        .join(name)
}

/// A mapping file's 256 rows: byte -> scalar, `None` for `#UNDEFINED`.
fn mapping_file(name: &str) -> Vec<Option<u32>> {
    let text = fs::read_to_string(data(name)).unwrap();
    let mut t = vec![None; 256];
    let mut seen = [false; 256];
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let mut f = line.split('\t');
        let b = f.next().unwrap().trim();
        let b = usize::from_str_radix(b.strip_prefix("0x").unwrap(), 16).unwrap();
        let u = f.next().unwrap().trim();
        assert!(!seen[b], "{name}: byte {b:#x} twice");
        seen[b] = true;
        t[b] = u
            .strip_prefix("0x")
            .map(|h| u32::from_str_radix(h, 16).unwrap());
    }
    assert!(seen.iter().all(|&s| s), "{name}: not every byte listed");
    t
}

fn run(cp: Codepage, input: &[u8]) -> Result<Vec<u8>, CodepageError> {
    run_srt(cp, input, false)
}

fn run_srt(cp: Codepage, input: &[u8], srt: bool) -> Result<Vec<u8>, CodepageError> {
    let mut out = Vec::new();
    let n = decode(cp, &mut Cursor::new(input), &mut out, srt)?;
    assert_eq!(n, out.len() as u64, "the count is the bytes written");
    Ok(out)
}

fn err(cp: Codepage, input: &[u8]) -> String {
    match run(cp, input) {
        Ok(o) => panic!("decoded: {:?}", String::from_utf8_lossy(&o)),
        Err(e) => e.to_string(),
    }
}

/// Every byte of both tables is what the mapping file says, the lower half
/// ASCII, and every mapped character is in the Basic Multilingual Plane
/// (the decoder's 3-byte bound and the engine's 3 x cap rest on it).
#[test]
fn tables_match_the_unicode_mapping_files() {
    for (cp, file) in [
        (Codepage::Cp1250, "CP1250.TXT"),
        (Codepage::Iso88592, "8859-2.TXT"),
    ] {
        let want = mapping_file(file);
        for b in 0..=255u8 {
            let got = cp.mapping(b).map(u32::from);
            assert_eq!(got, want[usize::from(b)], "{file} byte {b:#04x}");
            if b < 0x80 {
                assert_eq!(got, Some(u32::from(b)), "{file} ASCII {b:#04x}");
            }
            assert!(got.is_none_or(|u| u <= 0xFFFF), "{file} {b:#04x}");
        }
    }
    let undefined: Vec<u8> = (0..=255u8)
        .filter(|&b| Codepage::Cp1250.mapping(b).is_none())
        .collect();
    assert_eq!(undefined, CP1250_UNDEFINED);
    assert!((0..=255u8).all(|b| Codepage::Iso88592.mapping(b).is_some()));
}

/// A test-time tool on `PATH`; missing skips, unless
/// `INACIRCLE_REQUIRE_PDF_TOOLS=1` makes it a failure.
fn have(tool: &str) -> bool {
    let found = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
        .unwrap_or(false);
    if !found {
        if std::env::var_os("INACIRCLE_REQUIRE_PDF_TOOLS").is_some_and(|v| v == "1") {
            panic!("{tool} is required (INACIRCLE_REQUIRE_PDF_TOOLS=1) but not installed");
        }
        eprintln!("skipping: {tool} is not installed");
    }
    found
}

fn iconv(from: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    let mut c = Command::new("iconv")
        .args(["-f", from, "-t", "UTF-8"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(bytes).unwrap();
    let o = c.wait_with_output().unwrap();
    o.status.success().then_some(o.stdout)
}

/// glibc's `iconv` agrees with both tables on every byte: the same UTF-8
/// for every byte a table maps, an error for every byte it leaves
/// undefined. The one stated difference is the decoder's, not the
/// table's: ISO-8859-2 `0x80-0x9F` are C1 controls in glibc and in
/// `8859-2.TXT`, and the decoder refuses them (a C1 control in a text
/// file is a Windows-1250 quote or dash read with the wrong code page).
#[test]
fn tables_match_iconv() {
    if !have("iconv") {
        return;
    }
    for (cp, name) in [
        (Codepage::Cp1250, "CP1250"),
        (Codepage::Iso88592, "ISO-8859-2"),
    ] {
        let mapped: Vec<u8> = (0..=255u8).filter(|&b| cp.mapping(b).is_some()).collect();
        let want: String = mapped.iter().map(|&b| cp.mapping(b).unwrap()).collect();
        let got = iconv(name, &mapped).unwrap_or_else(|| panic!("iconv {name} refused"));
        assert_eq!(String::from_utf8(got).unwrap(), want, "{name}");
        for b in (0..=255u8).filter(|&b| cp.mapping(b).is_none()) {
            assert_eq!(iconv(name, &[b]), None, "{name} byte {b:#04x}");
        }
    }
    // The stated difference: iconv maps ISO-8859-2 0x80-0x9F, we refuse.
    for b in 0x80..=0x9Fu8 {
        assert!(iconv("ISO-8859-2", &[b]).is_some(), "{b:#04x}");
        assert!(run(Codepage::Iso88592, &[b]).is_err(), "{b:#04x}");
    }
}

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

fn trickle(cp: Codepage, input: &[u8], step: usize) -> Result<Vec<u8>, CodepageError> {
    let mut out = Vec::new();
    decode(cp, &mut Trickle { data: input, step }, &mut out, false)?;
    Ok(out)
}

/// The expected output: each byte by the table.
fn reference(cp: Codepage, input: &[u8]) -> Vec<u8> {
    input
        .iter()
        .map(|&b| cp.mapping(b).unwrap())
        .collect::<String>()
        .into_bytes()
}

/// The verdict is the same however the input arrives: whole 64 KiB reads,
/// one byte at a time or odd sizes, for 0 B, 1 B and 64 KiB +- 1, with an
/// accent, a UTF-8 sequence, a refused byte and a Ctrl-Z at the boundary.
#[test]
fn chunk_boundaries_do_not_matter() {
    let cp = Codepage::Cp1250;
    let steps = [1, 7, CHUNK - 1, CHUNK, CHUNK + 1, usize::MAX];
    let same = |input: &[u8]| -> Result<Vec<u8>, String> {
        let whole = run(cp, input).map_err(|e| e.to_string());
        for &s in &steps {
            let got = trickle(cp, input, s).map_err(|e| e.to_string());
            assert_eq!(got, whole, "step {s}, {} bytes", input.len());
        }
        whole
    };
    for len in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK + 3] {
        // Accented text with `ő` (0xF5) on both sides of every boundary.
        let mut input = vec![b'a'; len];
        for at in [CHUNK - 1, CHUNK, 2 * CHUNK] {
            if at < len {
                input[at] = 0xF5;
            }
        }
        assert_eq!(same(&input), Ok(reference(cp, &input)), "{len}");
        // A Ctrl-Z as the last byte is kept wherever the file ends.
        if len > 0 {
            let mut z = input.clone();
            *z.last_mut().unwrap() = 0x1A;
            assert_eq!(same(&z), Ok(reference(cp, &z)), "Ctrl-Z, {len}");
        }
    }
    // A UTF-8 `é` (C3 A9) split by the 64 KiB read: still already UTF-8.
    let mut split = vec![b'a'; CHUNK + 1];
    split[CHUNK - 1] = 0xC3;
    split[CHUNK] = 0xA9;
    assert_eq!(same(&split), Err("already UTF-8".into()));
    // A refused byte and a misplaced Ctrl-Z right at the boundary.
    let mut bad = vec![b'a'; CHUNK + 1];
    bad[CHUNK] = 0x81;
    assert_eq!(
        same(&bad),
        Err(format!("byte 0x81 at {CHUNK} is not Windows-1250"))
    );
    let mut z = vec![b'a'; CHUNK + 1];
    z[CHUNK - 1] = 0x1A;
    assert_eq!(
        same(&z),
        Err(format!("not text: control byte at {}", CHUNK - 1))
    );
}

/// CRLF, LF and a lone CR in one file, an FF, a TAB, and a Ctrl-Z as the
/// very last byte: all come out byte for byte, no byte order mark.
#[test]
fn line_ends_and_ctrl_z_kept() {
    let input = b"\xC1rv\xEDz\r\nt\xFBr\xF5\nf\xFAr\xF3\rg\xE9p\x0C\tv\xE9ge\r\n\x1A";
    let want = "Árvíz\r\ntűrő\nfúró\rgép\x0C\tvége\r\n\x1A";
    for cp in BOTH {
        assert_eq!(run(cp, input).unwrap(), want.as_bytes(), "{cp:?}");
    }
    // Only one Ctrl-Z, and only last: two, or one before the end, fail.
    for cp in BOTH {
        assert_eq!(err(cp, b"ab\x1A\x1A"), "not text: control byte at 2");
        assert_eq!(err(cp, b"ab\x1A\n"), "not text: control byte at 2");
    }
}

/// `.srt` (the caller decides by the extension) starts with a UTF-8 byte
/// order mark; anything else gets none.
#[test]
fn srt_gets_a_bom_others_do_not() {
    for cp in BOTH {
        let with = run_srt(cp, b"1\r\nf\xF5\r\n", true).unwrap();
        assert_eq!(with, [&BOM[..], "1\r\nfő\r\n".as_bytes()].concat());
        let without = run_srt(cp, b"1\r\nf\xF5\r\n", false).unwrap();
        assert_eq!(without, "1\r\nfő\r\n".as_bytes());
    }
}

/// Each of the five undefined Windows-1250 bytes fails with its offset.
#[test]
fn unassigned_cp1250_byte_fails_with_offset() {
    for b in CP1250_UNDEFINED {
        let input = [b"k\xE9s ".as_slice(), &[b], b" v\xE9ge"].concat();
        assert_eq!(
            err(Codepage::Cp1250, &input),
            format!("byte 0x{b:02X} at 4 is not Windows-1250")
        );
    }
}

/// A Windows-1250 file with Hungarian quotes read as ISO-8859-2: the
/// first C1 byte that Windows-1250 assigns points to it.
#[test]
fn iso_on_a_cp1250_file_points_to_cp1250() {
    // „Árvíztűrő” tükörfúrógép, in Windows-1250.
    let input = b"\x84\xC1rv\xEDzt\xFBr\xF5\x94 t\xFCk\xF6rf\xFAr\xF3g\xE9p\r\n";
    assert_eq!(
        err(Codepage::Iso88592, input),
        "byte 0x84 at 0: this looks like Windows-1250"
    );
    // The same file is fine as Windows-1250.
    let out = run(Codepage::Cp1250, input).unwrap();
    assert_eq!(out, "„Árvíztűrő” tükörfúrógép\r\n".as_bytes());
    // Every C1 byte Windows-1250 assigns gives that message.
    for b in (0x80..=0x9Fu8).filter(|&b| Codepage::Cp1250.mapping(b).is_some()) {
        assert_eq!(
            err(Codepage::Iso88592, &[b'x', b]),
            format!("byte 0x{b:02X} at 1: this looks like Windows-1250")
        );
    }
}

/// A C1 byte that neither code page assigns names ISO-8859-2, not
/// Windows-1250 (it would not be right either).
#[test]
fn c1_unassigned_in_both_names_iso() {
    for b in CP1250_UNDEFINED {
        assert_eq!(
            err(Codepage::Iso88592, &[b'x', b'y', b]),
            format!("byte 0x{b:02X} at 2 is not ISO-8859-2")
        );
    }
}

/// A control byte anywhere fails with its offset, however late, and
/// before any refused byte after it; DEL is not one.
#[test]
fn control_byte_late_fails() {
    for cp in BOTH {
        for c in (0..0x20u8).filter(|c| !b"\t\n\x0C\r\x1A".contains(c)) {
            let mut input = vec![b'a'; 3 * CHUNK];
            input[100] = 0xE9;
            let at = 3 * CHUNK - 2;
            input[at] = c;
            assert_eq!(
                err(cp, &input),
                format!("not text: control byte at {at}"),
                "{c:#04x}"
            );
        }
        // NUL first, then a refused byte: the control byte stops at once.
        assert_eq!(err(cp, b"a\0\x81"), "not text: control byte at 1");
        assert_eq!(run(cp, b"a\x7F").unwrap(), b"a\x7F");
    }
}

/// Valid UTF-8 with an accent is refused as already UTF-8, also where its
/// continuation bytes are bytes the code page refuses (`Á` is C3 81).
#[test]
fn already_utf8_is_skipped() {
    for cp in BOTH {
        for text in ["Árvíztűrő tükörfúrógép\n", "Á", "e\u{20AC}", "\u{1F600} ok"] {
            assert_eq!(err(cp, text.as_bytes()), "already UTF-8", "{text}");
        }
        assert!(matches!(
            run(cp, "é".as_bytes()),
            Err(CodepageError::AlreadyUtf8)
        ));
    }
}

/// Mostly UTF-8 with a damaged byte: as many valid multi-byte sequences as
/// damaged bytes (or more) is refused with the first damaged offset; real
/// 8-bit text that happens to hold one valid sequence still converts.
#[test]
fn mostly_utf8_with_damaged_bytes_fails() {
    for cp in BOTH {
        // `é` valid, then a lone 0xE9 at 3: one valid, one damaged.
        let input = b"\xC3\xA9 \xE9 x";
        assert_eq!(
            err(cp, input),
            "looks like UTF-8 with damaged bytes (first at 3)"
        );
        // A sequence cut short counts its bytes as damaged: `E2 82` at 0
        // (2 bytes), then two valid `é`.
        let input = b"\xE2\x82 \xC3\xA9\xC3\xA9";
        assert_eq!(
            err(cp, input),
            "looks like UTF-8 with damaged bytes (first at 0)"
        );
        // A truncated sequence at the very end is damaged too.
        let input = b"\xC3\xA9\xC3";
        assert_eq!(
            err(cp, input),
            "looks like UTF-8 with damaged bytes (first at 2)"
        );
    }
    // Two damaged bytes against one valid sequence: 8-bit text. `\xC9\xA9`
    // (`É©` in Windows-1250) is valid UTF-8 by accident.
    let input = b"\xC9\xA9 t\xFBr\xF5";
    assert_eq!(run(Codepage::Cp1250, input).unwrap(), "É© tűrő".as_bytes());
    // UTF-8 Hungarian text with one stray Windows-1250 byte: every accent
    // and the quotes count, so it is refused at the stray byte.
    let mut input = "„Árvíztűrő tükörfúrógép”\n".as_bytes().to_vec();
    let at = input.len();
    input.extend_from_slice(b"\xE9 ok\n");
    for cp in BOTH {
        assert_eq!(
            err(cp, &input),
            format!("looks like UTF-8 with damaged bytes (first at {at})")
        );
    }
}

/// All-caps Central European text holds valid UTF-8 pairs by accident (a
/// capital's byte followed by a capital or punctuation in `0x80-0xBF`),
/// but they decode to Cyrillic, Hebrew, Arabic or Armenian letters, never
/// to the Latin letters and punctuation UTF-8 Hungarian, Polish or Czech
/// text is made of: they are no evidence of UTF-8, and the text converts.
#[test]
fn capitals_that_look_like_utf8_convert() {
    // Polish `ŁÓŻKO`: `D3 AF` is U+04EF.
    for cp in BOTH {
        assert_eq!(
            run(cp, b"\xA3\xD3\xAFKO\r\n").unwrap(),
            "ŁÓŻKO\r\n".as_bytes()
        );
    }
    // Czech `ÚŽASNÉ`: `DA 8E` / `DA AE` are U+068E / U+06AE.
    assert_eq!(
        run(Codepage::Cp1250, b"\xDA\x8EASN\xC9\r\n").unwrap(),
        "ÚŽASNÉ\r\n".as_bytes()
    );
    assert_eq!(
        run(Codepage::Iso88592, b"\xDA\xAEASN\xC9\r\n").unwrap(),
        "ÚŽASNÉ\r\n".as_bytes()
    );
    // Hungarian `„ANYÓ”`: `D3 94` is U+04D4.
    assert_eq!(
        run(Codepage::Cp1250, b"\x84ANY\xD3\x94\r\n").unwrap(),
        "„ANYÓ”\r\n".as_bytes()
    );
    // Polish `ZAŻÓŁĆ GĘŚLĄ JAŹŃ` (two such pairs) and Hungarian `«NŐ»`
    // (`D5 BB`, U+057B, Armenian).
    assert_eq!(
        run(
            Codepage::Cp1250,
            b"ZA\xAF\xD3\xA3\xC6 G\xCA\x8CL\xA5 JA\x8F\xD1\r\n"
        )
        .unwrap(),
        "ZAŻÓŁĆ GĘŚLĄ JAŹŃ\r\n".as_bytes()
    );
    assert_eq!(
        run(Codepage::Cp1250, b"\xABN\xD5\xBB\r\n").unwrap(),
        "«NŐ»\r\n".as_bytes()
    );
}

/// A UTF-8 byte order mark says the file was written as UTF-8: with any
/// byte that is not UTF-8 after it, the file is damaged UTF-8, never
/// converted (a converted body would start with `ď»ż`).
#[test]
fn a_utf8_bom_with_damaged_bytes_fails() {
    for cp in BOTH {
        assert_eq!(
            err(cp, b"\xEF\xBB\xBFk\xE9s \xE1"),
            "looks like UTF-8 with damaged bytes (first at 4)"
        );
        assert_eq!(
            err(cp, b"\xEF\xBB\xBF\xA3\xD3\xAFKO\r\n"),
            "looks like UTF-8 with damaged bytes (first at 3)"
        );
        // A BOM and nothing but UTF-8: already UTF-8.
        assert_eq!(err(cp, b"\xEF\xBB\xBFok\n"), "already UTF-8");
    }
    // The same bytes later in the file are only U+FEFF, no signal.
    assert_eq!(
        run(Codepage::Cp1250, b"k\xE9s \xE1\xEF\xBB\xBF").unwrap(),
        "kés áď»ż".as_bytes()
    );
}

/// Only ASCII is its own UTF-8: the output is the input, byte for byte
/// (nothing to refuse as already UTF-8: no multi-byte character).
#[test]
fn ascii_only_converts_to_itself() {
    let input: Vec<u8> = (0x20..0x80u8).chain(*b"\t\r\n\x0C").collect();
    for cp in BOTH {
        assert_eq!(run(cp, &input).unwrap(), input);
        assert_eq!(run(cp, b"").unwrap(), b"");
    }
}

/// A small deterministic generator (xorshift64*), so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Any accepted input decoded and encoded back with the table is the
/// input: nothing is lost, so `lossy=no` holds. Random texts over every
/// byte each code page accepts, mixed with ASCII as text is.
#[test]
fn output_is_never_lossy() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for cp in BOTH {
        let alphabet: Vec<u8> = (0..=255u8)
            .filter(|&b| match run(cp, &[b'x', b]) {
                Ok(_) => b != 0x1A,
                Err(_) => false,
            })
            .collect();
        // Windows-1250: 256 - 28 C0 bytes (Ctrl-Z among them: it is only
        // allowed last) - 5 undefined; ISO-8859-2: 256 - 28 - 32 C1 bytes.
        let want = if cp == Codepage::Cp1250 { 223 } else { 196 };
        assert_eq!(alphabet.len(), want, "{cp:?}");
        let back: std::collections::HashMap<char, u8> = (0..=255u8)
            .filter_map(|b| cp.mapping(b).map(|c| (c, b)))
            .collect();
        let mut converted = 0;
        for _ in 0..2000 {
            let len = (rng.next() % 300) as usize;
            let input: Vec<u8> = (0..len)
                .map(|_| {
                    let r = rng.next();
                    if r.is_multiple_of(3) {
                        alphabet[(r >> 8) as usize % alphabet.len()]
                    } else {
                        b'a' + (r >> 8) as u8 % 26
                    }
                })
                .collect();
            match run(cp, &input) {
                Ok(out) => {
                    let text = String::from_utf8(out).unwrap();
                    let again: Vec<u8> = text.chars().map(|c| back[&c]).collect();
                    assert_eq!(again, input);
                    converted += 1;
                }
                // Random bytes can look like UTF-8; never anything else.
                Err(CodepageError::AlreadyUtf8 | CodepageError::DamagedUtf8 { .. }) => {}
                Err(e) => panic!("{e} on {input:?}"),
            }
        }
        assert!(converted > 1900, "{cp:?}: {converted}");
    }
}

/// The exact copy of every error.
#[test]
fn error_texts() {
    let cases = [
        (
            CodepageError::NotThisCodepage {
                cp: Codepage::Cp1250,
                byte: 0x81,
                at: 7,
            },
            "byte 0x81 at 7 is not Windows-1250",
        ),
        (
            CodepageError::NotThisCodepage {
                cp: Codepage::Iso88592,
                byte: 0x9F,
                at: 0,
            },
            "byte 0x9F at 0 is not ISO-8859-2",
        ),
        (
            CodepageError::LooksLikeCp1250 { byte: 0x84, at: 12 },
            "byte 0x84 at 12: this looks like Windows-1250",
        ),
        (
            CodepageError::Control { at: 3 },
            "not text: control byte at 3",
        ),
        (CodepageError::AlreadyUtf8, "already UTF-8"),
        (
            CodepageError::DamagedUtf8 { at: 9 },
            "looks like UTF-8 with damaged bytes (first at 9)",
        ),
    ];
    for (e, want) in cases {
        assert_eq!(e.to_string(), want);
    }
}

/// A write that fails stops the decoder with the error.
#[test]
fn a_failed_write_is_an_error() {
    struct Full;
    impl Write for Full {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(28))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let r = decode(
        Codepage::Cp1250,
        &mut Cursor::new(b"a\xE9"),
        &mut Full,
        false,
    );
    assert!(matches!(r, Err(CodepageError::Io(e)) if e.raw_os_error() == Some(28)));
}

// The action end to end.

const CP1250_LINE: &str = "action\ttext.cp1250-to-utf8\tfamily=text\tin=text/x-8bit\tout=text/plain\text=same\tlabel=Windows-1250\tgroup=encoding\tcost=10\tlossy=no\tbatch=yes\n";
const ISO_LINE: &str = "action\ttext.iso88592-to-utf8\tfamily=text\tin=text/x-8bit\tout=text/plain\text=same\tlabel=ISO-8859-2\tgroup=encoding\tcost=10\tlossy=no\tbatch=yes\n";

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_inacircle-text"))
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn lines<'a>(out: &'a str, kind: &str) -> Vec<Vec<&'a str>> {
    out.lines()
        .map(|l| l.split('\t').collect::<Vec<_>>())
        .filter(|f| f[0] == kind)
        .collect()
}

/// A temporary directory under `base`, removed on drop.
struct Tmp(PathBuf);

impl Tmp {
    fn new(base: &Path, name: &str) -> Tmp {
        let d = base.join(format!("inacircle-codepage-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("work")).unwrap();
        Tmp(d)
    }

    fn work(&self) -> PathBuf {
        self.0.join("work")
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_action(id: &str, work: &Path, inputs: &[&Path]) -> Output {
    bin()
        .args(["run", id, "--work"])
        .arg(work)
        .arg("--")
        .args(inputs)
        .output()
        .unwrap()
}

/// Every file under `dir`, relative to it.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            v.extend(
                files(&p)
                    .into_iter()
                    .map(|f| Path::new(p.file_name().unwrap()).join(f)),
            );
        } else {
            v.push(PathBuf::from(p.file_name().unwrap()));
        }
    }
    v.sort();
    v
}

/// The manifest carries both lines, exactly, after the existing ones.
#[test]
fn manifest_lists_the_code_page_actions() {
    let o = bin().arg("--manifest").output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    let text = stdout(&o);
    assert!(
        text.ends_with(&format!("{CP1250_LINE}{ISO_LINE}")),
        "{text}"
    );
}

/// A Windows-1250 `.srt` becomes a UTF-8 `.srt` under its own name, with a
/// byte order mark, its CRLF kept; the notice line names the code page.
/// A `.txt` gets no byte order mark.
#[test]
fn cp1250_srt_to_utf8_srt() {
    let t = Tmp::new(&std::env::temp_dir(), "srt");
    let srt = t.0.join("Film.hu.srt");
    fs::write(
        &srt,
        b"1\r\n00:00:01,000 --> 00:00:02,000\r\n\x84Gy\xF5zz\xFCnk!\x94\r\n\r\n",
    )
    .unwrap();
    let txt = t.0.join("a.TXT");
    fs::write(&txt, b"\xE1rv\xEDz\n").unwrap();
    let o = run_action("text.cp1250-to-utf8", &t.work(), &[&srt, &txt]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    let done = lines(&out, "done");
    assert_eq!(done.len(), 2, "{out}");
    assert_eq!(done[0][1], srt.to_str().unwrap());
    assert_eq!(done[0][2], "0/Film.hu.srt");
    assert_eq!(done[1][2], "1/a.TXT");
    let got = fs::read(t.work().join("0/Film.hu.srt")).unwrap();
    let want = "1\r\n00:00:01,000 --> 00:00:02,000\r\n„Győzzünk!”\r\n\r\n";
    assert_eq!(got, [&BOM[..], want.as_bytes()].concat());
    assert_eq!(
        fs::read(t.work().join("1/a.TXT")).unwrap(),
        "árvíz\n".as_bytes()
    );
    let warn = lines(&out, "warn");
    assert_eq!(warn.len(), 2, "{out}");
    for w in warn {
        assert_eq!(w[2], "converted from Windows-1250 to UTF-8");
    }
    // ISO-8859-2 names itself; an input without an extension keeps none.
    let bare = t.0.join("README");
    fs::write(&bare, b"\xB5\xE1\n").unwrap();
    let work2 = t.0.join("work2");
    fs::create_dir_all(&work2).unwrap();
    let o = run_action("text.iso88592-to-utf8", &work2, &[&bare]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert_eq!(lines(&out, "done")[0][2], "0/README");
    assert_eq!(fs::read(work2.join("0/README")).unwrap(), "ľá\n".as_bytes());
    assert_eq!(
        lines(&out, "warn")[0][2],
        "converted from ISO-8859-2 to UTF-8"
    );
}

/// The `.srt` byte order mark ignores the extension's case.
#[test]
fn srt_bom_ignores_case() {
    let t = Tmp::new(&std::env::temp_dir(), "srtcase");
    let upper = t.0.join("X.SRT");
    let mixed = t.0.join("x.Srt");
    fs::write(&upper, b"\xF5\r\n").unwrap();
    fs::write(&mixed, b"\xF5\r\n").unwrap();
    let o = run_action("text.cp1250-to-utf8", &t.work(), &[&upper, &mixed]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    let want = [&BOM[..], "ő\r\n".as_bytes()].concat();
    assert_eq!(fs::read(t.work().join("0/X.SRT")).unwrap(), want);
    assert_eq!(fs::read(t.work().join("1/x.Srt")).unwrap(), want);
}

/// A refused input, a control byte, an already UTF-8 file and a mostly
/// UTF-8 one leave nothing in the work folder; the others still convert.
/// Exit 1 when only failures remain, 3 when only skips.
#[test]
fn failure_leaves_nothing() {
    let t = Tmp::new(&std::env::temp_dir(), "fail");
    let mk = |name: &str, bytes: &[u8]| {
        let p = t.0.join(name);
        fs::write(&p, bytes).unwrap();
        p
    };
    let bad = mk("bad.txt", b"ok \xE9\n\x81 late");
    let ctl = mk("ctl.txt", b"\xE9\0");
    let utf = mk("utf.txt", "már UTF-8\n".as_bytes());
    let dmg = mk("dmg.txt", b"\xC3\xA1 \xE1");
    let good = mk("good.txt", b"\xE9\n");
    let o = run_action(
        "text.cp1250-to-utf8",
        &t.work(),
        &[&bad, &ctl, &utf, &dmg, &good],
    );
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    let fail = lines(&out, "fail");
    let reasons: Vec<(&str, &str)> = fail.iter().map(|f| (f[1], f[2])).collect();
    assert_eq!(
        reasons,
        [
            (bad.to_str().unwrap(), "byte 0x81 at 5 is not Windows-1250"),
            (ctl.to_str().unwrap(), "not text: control byte at 1"),
            (
                dmg.to_str().unwrap(),
                "looks like UTF-8 with damaged bytes (first at 3)"
            ),
        ]
    );
    let skip = lines(&out, "skip");
    assert_eq!(skip.len(), 1, "{out}");
    assert_eq!(
        (skip[0][1], skip[0][2]),
        (utf.to_str().unwrap(), "already UTF-8")
    );
    assert_eq!(files(&t.work()), [PathBuf::from("4/good.txt")]);
    assert_eq!(lines(&out, "warn").len(), 1, "{out}");

    let work2 = t.0.join("work2");
    fs::create_dir_all(&work2).unwrap();
    let o = run_action("text.iso88592-to-utf8", &work2, &[&bad, &ctl]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    assert!(files(&work2).is_empty());
    let o = run_action("text.iso88592-to-utf8", &work2, &[&utf]);
    assert_eq!(o.status.code(), Some(3), "{}", stdout(&o));
    assert!(files(&work2).is_empty());
}

/// `VmHWM` of a running child, sampled every millisecond by a thread and
/// once per line it prints (the high-water mark only grows, so the last
/// sample before it exits is the peak; a zombie has none). Not
/// `ru_maxrss`: after `exec` it starts from this test process's peak.
fn peak_kb(child: &mut std::process::Child) -> (u64, String) {
    let pid = child.id();
    let peak = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let sample = {
        let peak = Arc::clone(&peak);
        move || {
            if let Ok(s) = fs::read_to_string(format!("/proc/{pid}/status"))
                && let Some(l) = s.lines().find(|l| l.starts_with("VmHWM:"))
            {
                let kb: u64 = l.split_whitespace().nth(1).unwrap().parse().unwrap();
                peak.fetch_max(kb, Ordering::Relaxed);
            }
        }
    };
    let poller = {
        let (stop, sample) = (Arc::clone(&stop), sample.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                sample();
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        })
    };
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut text = String::new();
    let mut line = String::new();
    while out.read_line(&mut line).unwrap() > 0 {
        sample();
        text.push_str(&line);
        line.clear();
    }
    stop.store(true, Ordering::Relaxed);
    poller.join().unwrap();
    (peak.load(Ordering::Relaxed), text)
}

/// Writes `unit` to `path` as often as fits in `size` bytes, in writes of
/// about 1 MiB; how often it was written.
fn generate(path: &Path, unit: &[u8], size: usize) -> usize {
    let per_block = (1 << 20) / unit.len();
    let block = unit.repeat(per_block);
    let mut f = io::BufWriter::new(fs::File::create(path).unwrap());
    let units = size / unit.len();
    let mut left = units;
    while left > 0 {
        let n = left.min(per_block);
        f.write_all(&block[..n * unit.len()]).unwrap();
        left -= n;
    }
    f.flush().unwrap();
    units
}

/// The input has no size limit, so memory must not follow it: a 200 MiB
/// Hungarian text and a 64 MiB file of nothing but 3-byte characters
/// (`„`, the densest output: 192 MiB) both convert in the same few MiB.
/// The files are on disk under Cargo's target temp folder (`/tmp` may be a
/// small tmpfs).
#[test]
fn large_input_streams_in_constant_memory() {
    let t = Tmp::new(Path::new(env!("CARGO_TARGET_TMPDIR")), "large");
    let cases: [(&str, &[u8], usize); 2] = [
        (
            "large.txt",
            b"\xC1rv\xEDzt\xFBr\xF5 t\xFCk\xF6rf\xFAr\xF3g\xE9p, \x84id\xE9z\xF5jel\x94.\r\n",
            200 << 20,
        ),
        ("dense.txt", b"\x84", 64 << 20),
    ];
    for (name, unit, size) in cases {
        let input = t.0.join(name);
        let units = generate(&input, unit, size);
        let work = t.0.join(format!("work-{name}"));
        fs::create_dir_all(&work).unwrap();
        let mut child = bin()
            .args(["run", "text.cp1250-to-utf8", "--work"])
            .arg(&work)
            .arg("--")
            .arg(&input)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let (peak_kb, text) = peak_kb(&mut child);
        assert!(child.wait().unwrap().success(), "{text}");
        let out = work.join("0").join(name);
        let len = fs::metadata(&out).unwrap().len();
        let chars: usize = unit
            .iter()
            .map(|&b| Codepage::Cp1250.mapping(b).unwrap().len_utf8())
            .sum();
        assert_eq!(len, (units * chars) as u64, "{name}");
        println!(
            "large_input_streams_in_constant_memory: {name} {size} B in, {len} B out, VmHWM {peak_kb} kB"
        );
        // See the measured value in docs/resource-limits.md. 8 MiB is
        // about twice it and still far below any buffering of the input
        // (200 MiB) or output (192 MiB).
        assert!(peak_kb <= 8 * 1024, "{name}: VmHWM {peak_kb} kB");
        fs::remove_file(&input).unwrap();
        fs::remove_file(&out).unwrap();
    }
}
