use flate::{Crc32, InflateError, adler32, inflate, zlib_inflate};
use miniz_oxide::deflate::{compress_to_vec, compress_to_vec_zlib};
use std::io::{self, Read};

fn run(data: &[u8], limit: u64) -> (Result<flate::Inflated, InflateError>, Vec<u8>) {
    let mut out = Vec::new();
    let r = inflate(data, limit, &mut |c| {
        out.extend_from_slice(c);
        Ok(())
    });
    (r, out)
}

fn run_zlib(data: &[u8], limit: u64) -> (Result<flate::Inflated, InflateError>, Vec<u8>) {
    let mut out = Vec::new();
    let r = zlib_inflate(data, limit, &mut |c| {
        out.extend_from_slice(c);
        Ok(())
    });
    (r, out)
}

/// Deterministic xorshift64* so the data is the same on every run.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// LSB-first bit writer for hand-built streams.
#[derive(Default)]
struct BitW {
    bytes: Vec<u8>,
    nbits: usize,
}
impl BitW {
    fn bits(&mut self, v: u32, n: u32) {
        for i in 0..n {
            if self.nbits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (v >> i) & 1 == 1 {
                *self.bytes.last_mut().unwrap() |= 1 << (self.nbits % 8);
            }
            self.nbits += 1;
        }
    }
    /// A Huffman code is written most significant bit first.
    fn code(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bits((v >> i) & 1, 1);
        }
    }
    /// Fixed-Huffman literal/length symbol (RFC 1951 3.2.6).
    fn fixed_sym(&mut self, s: u32) {
        match s {
            0..=143 => self.code(0x30 + s, 8),
            144..=255 => self.code(0x190 + s - 144, 9),
            256..=279 => self.code(s - 256, 7),
            _ => self.code(0xC0 + s - 280, 8),
        }
    }
    fn fixed_header(&mut self, last: bool) {
        self.bits(u32::from(last), 1);
        self.bits(1, 2);
    }
}

fn stored_block(last: bool, data: &[u8]) -> Vec<u8> {
    let mut v = vec![u8::from(last)];
    v.extend_from_slice(&(data.len() as u16).to_le_bytes());
    v.extend_from_slice(&(!(data.len() as u16)).to_le_bytes());
    v.extend_from_slice(data);
    v
}

fn btype(stream: &[u8]) -> u8 {
    (stream[0] >> 1) & 3
}

#[test]
fn stored_block_round_trips() {
    let s = stored_block(true, b"hello");
    let (r, out) = run(&s, 1 << 20);
    let r = r.unwrap();
    assert_eq!(out, b"hello");
    assert_eq!(r.in_bytes, s.len() as u64);
    assert_eq!(r.out_bytes, 5);
}

#[test]
fn fixed_huffman_known_vector() {
    let want = b"abcabcabcabc";
    let s = compress_to_vec(want, 1);
    assert_eq!(btype(&s), 1, "the vector must be a fixed-Huffman block");
    let (r, out) = run(&s, 1 << 20);
    r.unwrap();
    assert_eq!(out, want);
}

#[test]
fn dynamic_huffman_large_text() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let words: [&[u8]; 6] = [
        b"circle ", b"wheel ", b"slice ", b"action ", b"page ", b"\n",
    ];
    let mut data = Vec::new();
    while data.len() < 1 << 20 {
        if rng.next().is_multiple_of(4) {
            data.push(rng.next() as u8);
        } else {
            data.extend_from_slice(words[(rng.next() % 6) as usize]);
        }
    }
    data.truncate(1 << 20);
    let s = compress_to_vec(&data, 9);
    assert_eq!(btype(&s), 2, "the vector must start with a dynamic block");
    let (r, out) = run(&s, 2 << 20);
    let r = r.unwrap();
    assert!(out == data);
    assert_eq!(r.in_bytes, s.len() as u64);
}

#[test]
fn distance_across_window_boundary() {
    // Hand-built: a stored block of 32768 distinct-ish bytes, then a fixed
    // block copying 10 bytes from distance 32768 (the farthest allowed).
    let mut rng = Rng(77);
    let first: Vec<u8> = (0..32768).map(|_| rng.next() as u8).collect();
    let mut s = stored_block(false, &first);
    let mut w = BitW::default();
    w.fixed_header(true);
    w.fixed_sym(264); // length 10
    w.code(29, 5); // distance code 29: base 24577, 13 extra bits
    w.bits(32768 - 24577, 13);
    w.fixed_sym(256);
    s.extend_from_slice(&w.bytes);
    let (r, out) = run(&s, 1 << 20);
    r.unwrap();
    assert_eq!(out.len(), 32768 + 10);
    assert_eq!(&out[32768..], &first[..10]);

    // The same through the compressor: a repeat exactly one window away.
    let mut data = first.clone();
    data.extend_from_slice(&first[..300]);
    let (r, out) = run(&compress_to_vec(&data, 9), 1 << 20);
    r.unwrap();
    assert_eq!(out, data);
}

#[test]
fn rejects_btype_3() {
    let (r, _) = run(&[0x07, 0, 0, 0], 1 << 20);
    assert!(matches!(r, Err(InflateError::BadBlockType)));
}

#[test]
fn rejects_stored_len_nlen_mismatch() {
    let mut s = stored_block(true, b"hello");
    s[3] ^= 1;
    let (r, _) = run(&s, 1 << 20);
    assert!(matches!(r, Err(InflateError::BadStoredLength)));
}

#[test]
fn rejects_distance_before_start() {
    let mut w = BitW::default();
    w.fixed_header(true);
    w.fixed_sym(u32::from(b'a'));
    w.fixed_sym(257); // length 3
    w.code(1, 5); // distance 2, but only one byte exists
    w.fixed_sym(256);
    let (r, out) = run(&w.bytes, 1 << 20);
    assert!(matches!(r, Err(InflateError::DistanceTooFar)));
    assert!(out.is_empty());
}

#[test]
fn rejects_truncated_stream() {
    let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
    let s = compress_to_vec(&data, 6);
    let (r, _) = run(&s[..s.len() / 2], 1 << 20);
    assert!(matches!(r, Err(InflateError::Truncated)));
    let (r, _) = run(&[], 1 << 20);
    assert!(matches!(r, Err(InflateError::Truncated)));
    // Stored block with fewer bytes than it declares.
    let s = stored_block(true, b"hello");
    let (r, _) = run(&s[..s.len() - 1], 1 << 20);
    assert!(matches!(r, Err(InflateError::Truncated)));
}

#[test]
fn rejects_oversubscribed_code() {
    // Dynamic block, HCLEN = 4: the code length code gets four symbols
    // (16, 17, 18, 0) of length 1 each, which over-subscribes it.
    let mut w = BitW::default();
    w.bits(1, 1);
    w.bits(2, 2);
    w.bits(0, 5); // HLIT
    w.bits(0, 5); // HDIST
    w.bits(0, 4); // HCLEN
    for _ in 0..4 {
        w.bits(1, 3);
    }
    let (r, _) = run(&w.bytes, 1 << 20);
    assert!(matches!(r, Err(InflateError::BadCode)));
}

#[test]
fn output_limit_stops_at_the_limit() {
    const MIB: usize = 1 << 20;
    let s = compress_to_vec(&vec![0u8; 10 * MIB], 9);
    assert!(s.len() < 64 * 1024, "a bomb: tiny input, 10 MiB output");
    let mut sum = 0u64;
    let r = inflate(&s[..], MIB as u64, &mut |c| {
        sum += c.len() as u64;
        Ok(())
    });
    assert!(matches!(r, Err(InflateError::OutputLimit)));
    assert!(sum <= MIB as u64, "{sum} bytes passed to out");

    // Positive controls: exactly the size is fine, one byte less is not.
    let (r, out) = run(&s, 10 * MIB as u64);
    assert_eq!(r.unwrap().out_bytes, 10 * MIB as u64);
    assert_eq!(out.len(), 10 * MIB);
    let (r, out) = run(&s, 10 * MIB as u64 - 1);
    assert!(matches!(r, Err(InflateError::OutputLimit)));
    assert!(out.len() < 10 * MIB);

    // A stored block that would exceed the limit is refused before reading.
    let (r, out) = run(&stored_block(true, &[7; 100]), 99);
    assert!(matches!(r, Err(InflateError::OutputLimit)));
    assert!(out.is_empty());
    // Limit zero: only an empty stream passes.
    let (r, _) = run(&stored_block(true, b""), 0);
    assert_eq!(r.unwrap().out_bytes, 0);
}

#[test]
fn zlib_checks_adler32() {
    let data = b"the quick brown fox jumps over the lazy dog".repeat(20);
    let mut s = compress_to_vec_zlib(&data, 6);
    let (r, out) = run_zlib(&s, 1 << 20);
    let r = r.unwrap();
    assert_eq!(out, data);
    assert_eq!(r.in_bytes, s.len() as u64);
    *s.last_mut().unwrap() ^= 0xFF;
    let (r, _) = run_zlib(&s, 1 << 20);
    assert!(matches!(r, Err(InflateError::Adler32Mismatch)));
    // Trailer cut short.
    s.pop();
    let (r, _) = run_zlib(&s, 1 << 20);
    assert!(matches!(r, Err(InflateError::Truncated)));
}

#[test]
fn rejects_fdict() {
    let fcheck = |cmf: u8, flg: u8| {
        let base = (u16::from(cmf) << 8) | u16::from(flg);
        flg + ((31 - base % 31) % 31) as u8
    };
    let body = stored_block(true, b"x");
    let mk = |cmf: u8, flg: u8| {
        let mut v = vec![cmf, fcheck(cmf, flg)];
        v.extend_from_slice(&body);
        v
    };
    // FDICT set (with a valid FCHECK), bad CM, CINFO 8: all refused.
    for (cmf, flg) in [(0x78, 0x20), (0x79, 0), (0x88, 0)] {
        let (r, _) = run_zlib(&mk(cmf, flg), 1 << 20);
        assert!(matches!(r, Err(InflateError::BadZlibHeader)), "{cmf:x}");
    }
    // Bad FCHECK.
    let mut s = mk(0x78, 0);
    s[1] ^= 1;
    let (r, _) = run_zlib(&s, 1 << 20);
    assert!(matches!(r, Err(InflateError::BadZlibHeader)));
}

#[test]
fn adler32_known_value() {
    assert_eq!(adler32(1, b"Wikipedia"), 0x11E6_0398);
    // Chunked updates and a long input (past the 5552-byte reduction).
    let data = vec![0xFFu8; 100_000];
    let whole = adler32(1, &data);
    let split = adler32(adler32(1, &data[..12_345]), &data[12_345..]);
    assert_eq!(whole, split);
}

#[test]
fn crc32_known_value() {
    let mut c = Crc32::new();
    c.update(b"123456789");
    assert_eq!(c.finish(), 0xCBF4_3926);
    let mut c = Crc32::new();
    c.update(b"1234");
    c.update(b"56789");
    assert_eq!(c.finish(), 0xCBF4_3926);
    assert_eq!(Crc32::new().finish(), 0);
}

/// A reader that returns one byte per call, and one that fails.
struct OneByte<'a>(&'a [u8]);
impl Read for OneByte<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.split_first() {
            Some((&b, rest)) if !buf.is_empty() => {
                buf[0] = b;
                self.0 = rest;
                Ok(1)
            }
            _ => Ok(0),
        }
    }
}
struct Failing;
impl Read for Failing {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("boom"))
    }
}

#[test]
fn short_reads_and_io_errors() {
    let data = b"abcabcabc some more text, abcabcabc".repeat(100);
    let s = compress_to_vec(&data, 6);
    let mut out = Vec::new();
    let r = inflate(OneByte(&s), 1 << 20, &mut |c| {
        out.extend_from_slice(c);
        Ok(())
    })
    .unwrap();
    assert_eq!(out, data);
    assert_eq!(r.in_bytes, s.len() as u64);

    let r = inflate(Failing, 1 << 20, &mut |_| Ok(()));
    assert!(matches!(r, Err(InflateError::Io(_))));
    // An error from `out` surfaces as Io.
    let r = inflate(&s[..], 1 << 20, &mut |_| Err(io::Error::other("sink")));
    assert!(matches!(r, Err(InflateError::Io(_))));
}

#[test]
fn trailing_bytes_are_not_part_of_the_stream() {
    let mut s = compress_to_vec(b"hello hello hello", 6);
    let n = s.len() as u64;
    s.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let (r, out) = run(&s, 1 << 20);
    assert_eq!(r.unwrap().in_bytes, n);
    assert_eq!(out, b"hello hello hello");
}

/// 1 MiB of empty fixed blocks, 10 bits each: per-block cost must not
/// include building the fixed tables. Decodes to nothing and consumes every
/// input byte. (The fuzz target's inputs are shorter than this, so this
/// test is what covers the case; it pins the result.)
#[test]
fn many_empty_fixed_blocks() {
    let mut w = BitW::default();
    let blocks = (1usize << 20) * 8 / 10 - 1;
    for _ in 0..blocks {
        w.bits(0, 1); // BFINAL
        w.bits(1, 2); // BTYPE fixed
        w.bits(0, 7); // end of block
    }
    w.bits(1, 1);
    w.bits(1, 2);
    w.bits(0, 7);
    let (r, out) = run(&w.bytes, 0);
    let r = r.unwrap();
    assert!(out.is_empty());
    assert_eq!(r.out_bytes, 0);
    assert_eq!(r.in_bytes, w.bytes.len() as u64);
    assert_eq!(w.bytes.len(), 1_048_575);
}

/// Every error has its own non-empty message (the zip reader and the pages
/// action show it to the user).
#[test]
fn error_messages_are_stated() {
    let cases = [
        (InflateError::BadBlockType, "invalid deflate block type"),
        (
            InflateError::BadStoredLength,
            "stored block length check failed",
        ),
        (InflateError::BadCode, "invalid Huffman code"),
        (
            InflateError::DistanceTooFar,
            "match distance reaches before the start",
        ),
        (InflateError::Truncated, "stream ends early"),
        (InflateError::OutputLimit, "output exceeds the limit"),
        (InflateError::BadZlibHeader, "invalid zlib header"),
        (InflateError::Adler32Mismatch, "Adler-32 mismatch"),
        (
            InflateError::Io(io::Error::other("boom")),
            "read or write failed: boom",
        ),
    ];
    for (e, want) in cases {
        assert_eq!(e.to_string(), want);
    }
}

/// A reader that reports `Interrupted` before every real read.
struct Interrupting<'a> {
    data: &'a [u8],
    fail_next: bool,
}
impl Read for Interrupting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.fail_next {
            self.fail_next = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        self.fail_next = true;
        let n = buf.len().min(self.data.len()).min(7);
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

#[test]
fn interrupted_reads_are_retried() {
    let data = b"interrupted interrupted interrupted".repeat(20);
    let s = compress_to_vec(&data, 6);
    let mut out = Vec::new();
    let r = inflate(
        Interrupting {
            data: &s,
            fail_next: true,
        },
        1 << 20,
        &mut |c| {
            out.extend_from_slice(c);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(out, data);
    assert_eq!(r.in_bytes, s.len() as u64);
}

/// The decoder asks the reader for 16 KiB at a time: the fixed memory
/// (window plus buffer) is 48 KiB, as documented.
#[test]
fn reads_ask_for_a_16_kib_buffer() {
    struct Spy<'a> {
        data: &'a [u8],
        biggest: usize,
    }
    impl Read for Spy<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.biggest = self.biggest.max(buf.len());
            let n = buf.len().min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }
    let s = stored_block(true, &[1u8; 100]);
    let mut spy = Spy {
        data: &s,
        biggest: 0,
    };
    inflate(&mut spy, 1 << 20, &mut |_| Ok(())).unwrap();
    assert_eq!(spy.biggest, 16 * 1024);
}

/// Short stored blocks that leave whole bytes of the next block in the bit
/// buffer after their data is copied out.
#[test]
fn short_stored_blocks_in_a_row() {
    let mut s = Vec::new();
    for chunk in [&b"ab"[..], b"c", b"def", b"g"] {
        s.extend(stored_block(false, chunk));
    }
    s.extend(stored_block(true, b"hi"));
    let (r, out) = run(&s, 1 << 20);
    assert_eq!(r.unwrap().in_bytes, s.len() as u64);
    assert_eq!(out, b"abcdefghi");
}

/// A reader that fails after delivering a stored stream is not asked
/// again: the decoder takes no input it does not need.
#[test]
fn does_not_read_past_the_end_of_the_stream() {
    struct OnceThenFail<'a>(&'a [u8]);
    impl Read for OnceThenFail<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Err(io::Error::other("read after the end"));
            }
            let n = buf.len().min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }
    // Stored blocks only: a Huffman decoder legitimately tries to fill its
    // bit buffer past the last code, and meets the end of input there.
    let mut streams = vec![stored_block(true, b"x"), stored_block(true, &[3u8; 300])];
    for len in 0..12 {
        streams.push(stored_block(true, &[5u8; 12][..len]));
    }
    for s in streams {
        let mut out = Vec::new();
        let r = inflate(OnceThenFail(&s), 1 << 20, &mut |c| {
            out.extend_from_slice(c);
            Ok(())
        });
        assert_eq!(r.unwrap().in_bytes, s.len() as u64);
    }
}

/// HLIT above 286 and HDIST above 30 are each refused on their own, before
/// the code length codes are read (the stream ends right after the counts,
/// so a decoder that did not refuse them would report `Truncated`).
#[test]
fn dynamic_header_counts_are_limited() {
    for (hlit, hdist) in [(31u32, 0u32), (0, 31), (31, 31)] {
        let mut w = BitW::default();
        w.bits(1, 1); // BFINAL
        w.bits(2, 2); // BTYPE dynamic
        w.bits(hlit, 5);
        w.bits(hdist, 5);
        w.bits(0, 4);
        let (r, _) = run(&w.bytes, 1 << 20);
        assert!(
            matches!(r, Err(InflateError::BadCode)),
            "hlit {hlit} hdist {hdist}"
        );
    }
}
