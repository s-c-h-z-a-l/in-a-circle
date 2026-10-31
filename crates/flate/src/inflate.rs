use std::fmt;
use std::io::{self, Read};

/// What a finished stream consumed and produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inflated {
    /// Bytes of the stream (header and trailer included for zlib), not the
    /// read-ahead of the buffer.
    pub in_bytes: u64,
    /// Bytes handed to `out`.
    pub out_bytes: u64,
}

#[derive(Debug)]
pub enum InflateError {
    BadBlockType,
    BadStoredLength,
    BadCode,
    DistanceTooFar,
    Truncated,
    OutputLimit,
    BadZlibHeader,
    Adler32Mismatch,
    Io(io::Error),
}

impl fmt::Display for InflateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadBlockType => f.write_str("invalid deflate block type"),
            Self::BadStoredLength => f.write_str("stored block length check failed"),
            Self::BadCode => f.write_str("invalid Huffman code"),
            Self::DistanceTooFar => f.write_str("match distance reaches before the start"),
            Self::Truncated => f.write_str("stream ends early"),
            Self::OutputLimit => f.write_str("output exceeds the limit"),
            Self::BadZlibHeader => f.write_str("invalid zlib header"),
            Self::Adler32Mismatch => f.write_str("Adler-32 mismatch"),
            Self::Io(e) => write!(f, "read or write failed: {e}"),
        }
    }
}

impl std::error::Error for InflateError {}

impl From<io::Error> for InflateError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Deflate's back-reference window: RFC 1951 section 2 (distances up to
/// 32768). Must be a power of two (indices are masked).
const WINDOW: usize = 32 * 1024;
/// Read buffer: one page-sized syscall granularity that keeps the fixed
/// memory (window + buffer) at 48 KiB; larger buys nothing for a decoder
/// that is bit-serial anyway.
const BUF: usize = 16 * 1024;
/// Longest Huffman code (RFC 1951 section 3.2.2).
const MAX_BITS: usize = 15;

/// Buffered LSB-first bit reader. Bytes are pulled from the buffer into
/// `bitbuf` greedily, but a byte is only requested from `R` when the buffer
/// is empty, and end of input is reported only when a bit is really needed.
pub(crate) struct Bits<R> {
    r: R,
    buf: Box<[u8; BUF]>,
    pos: usize,
    len: usize,
    bitbuf: u64,
    nbits: u32,
    /// Bytes moved from the buffer into `bitbuf` (or handed out directly).
    pulled: u64,
}

impl<R: Read> Bits<R> {
    pub(crate) fn new(r: R) -> Self {
        Bits {
            r,
            buf: Box::new([0; BUF]),
            pos: 0,
            len: 0,
            bitbuf: 0,
            nbits: 0,
            pulled: 0,
        }
    }

    /// Stream bytes consumed so far; a partly used byte counts as consumed,
    /// whole bytes still in `bitbuf` do not.
    pub(crate) fn consumed(&self) -> u64 {
        self.pulled - u64::from(self.nbits / 8)
    }

    /// Make `buf` non-empty; `false` at end of input.
    fn refill(&mut self) -> Result<bool, InflateError> {
        if self.pos < self.len {
            return Ok(true);
        }
        loop {
            match self.r.read(&mut self.buf[..]) {
                Ok(n) => {
                    self.pos = 0;
                    self.len = n;
                    return Ok(n > 0);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(InflateError::Io(e)),
            }
        }
    }

    /// Top `bitbuf` up to at least `n` bits (n <= 32) if the input has
    /// them; end of input is not an error here.
    fn try_fill(&mut self, n: u32) -> Result<(), InflateError> {
        while self.nbits < n {
            if !self.refill()? {
                break;
            }
            // Greedy: take buffered bytes while a whole byte fits.
            while self.nbits <= 56 && self.pos < self.len {
                self.bitbuf |= u64::from(self.buf[self.pos]) << self.nbits;
                self.pos += 1;
                self.pulled += 1;
                self.nbits += 8;
            }
        }
        Ok(())
    }

    fn bits(&mut self, n: u32) -> Result<u32, InflateError> {
        if n == 0 {
            return Ok(0);
        }
        self.try_fill(n)?;
        if self.nbits < n {
            return Err(InflateError::Truncated);
        }
        let v = (self.bitbuf & ((1u64 << n) - 1)) as u32;
        self.bitbuf >>= n;
        self.nbits -= n;
        Ok(v)
    }

    /// Drop the bits up to the next byte boundary.
    pub(crate) fn align(&mut self) {
        let drop = self.nbits % 8;
        self.bitbuf >>= drop;
        self.nbits -= drop;
    }

    /// One whole byte; the reader must be byte-aligned or at the start.
    pub(crate) fn byte(&mut self) -> Result<u8, InflateError> {
        debug_assert_eq!(self.nbits % 8, 0);
        Ok(self.bits(8)? as u8)
    }

    /// Copy `dst.len()` bytes of a stored block (byte-aligned).
    fn copy_into(&mut self, dst: &mut [u8]) -> Result<(), InflateError> {
        let mut i = 0;
        while i < dst.len() && self.nbits >= 8 {
            dst[i] = self.bitbuf as u8;
            self.bitbuf >>= 8;
            self.nbits -= 8;
            i += 1;
        }
        while i < dst.len() {
            if !self.refill()? {
                return Err(InflateError::Truncated);
            }
            let n = (dst.len() - i).min(self.len - self.pos);
            dst[i..i + n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            self.pulled += n as u64;
            i += n;
        }
        Ok(())
    }
}

/// Canonical Huffman decoder (counts per length and symbols in code order),
/// the construction of RFC 1951 section 3.2.2.
struct Huffman {
    count: [u16; MAX_BITS + 1],
    symbol: [u16; 288],
}

impl Huffman {
    /// Oversubscribed codes are rejected. An incomplete code is accepted
    /// only with at most one symbol (RFC 1951: a single distance code of
    /// one bit; an empty distance tree for literal-only blocks), like the
    /// reference decoders; decoding an unassigned code is `BadCode`.
    /// This is broader than zlib, which accepts an incomplete code only as
    /// a single 1-bit code and never for the code-length code; it matches
    /// miniz (the differential reference). No table index can go out of
    /// range either way (the counts sum to at most the table size).
    fn new(lengths: &[u8]) -> Result<Self, InflateError> {
        let mut h = Huffman {
            count: [0; MAX_BITS + 1],
            symbol: [0; 288],
        };
        for &l in lengths {
            h.count[usize::from(l)] += 1;
        }
        let used = lengths.len() - usize::from(h.count[0]);
        let mut left: i32 = 1;
        for len in 1..=MAX_BITS {
            left = (left << 1) - i32::from(h.count[len]);
            if left < 0 {
                return Err(InflateError::BadCode);
            }
        }
        if left > 0 && used > 1 {
            return Err(InflateError::BadCode);
        }
        let mut offs = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offs[len + 1] = offs[len] + h.count[len];
        }
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                h.symbol[usize::from(offs[usize::from(l)])] = sym as u16;
                offs[usize::from(l)] += 1;
            }
        }
        Ok(h)
    }

    fn decode<R: Read>(&self, bits: &mut Bits<R>) -> Result<u16, InflateError> {
        bits.try_fill(MAX_BITS as u32)?;
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        let mut bb = bits.bitbuf;
        let avail = bits.nbits;
        for len in 1..=MAX_BITS {
            if len as u32 > avail {
                return Err(InflateError::Truncated);
            }
            code |= (bb & 1) as i32;
            bb >>= 1;
            let count = i32::from(self.count[len]);
            if code - count < first {
                bits.bitbuf >>= len;
                bits.nbits -= len as u32;
                return Ok(self.symbol[(index + (code - first)) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(InflateError::BadCode)
    }
}

/// Length codes 257..=285: base lengths and extra bits, RFC 1951 3.2.5.
const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
/// Distance codes 0..=29: base distances and extra bits, RFC 1951 3.2.5.
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// Order of the code length code lengths, RFC 1951 3.2.7.
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// The window plus the accounting of what was produced. Output reaches
/// `out` when the window wraps and at the end; the limit is checked before
/// a byte or a match is written, so nothing past it is ever produced.
struct Window<'a> {
    buf: Box<[u8; WINDOW]>,
    pos: usize,
    flushed: usize,
    total: u64,
    limit: u64,
    out: &'a mut dyn FnMut(&[u8]) -> io::Result<()>,
}

impl Window<'_> {
    fn flush(&mut self) -> Result<(), InflateError> {
        if self.pos > self.flushed {
            (self.out)(&self.buf[self.flushed..self.pos])?;
        }
        self.flushed = self.pos & (WINDOW - 1);
        Ok(())
    }

    fn reserve(&mut self, n: usize) -> Result<(), InflateError> {
        if n as u64 > self.limit - self.total {
            return Err(InflateError::OutputLimit);
        }
        Ok(())
    }

    fn put(&mut self, b: u8) -> Result<(), InflateError> {
        self.buf[self.pos] = b;
        self.pos += 1;
        self.total += 1;
        if self.pos == WINDOW {
            self.flush()?;
            self.pos = 0;
        }
        Ok(())
    }

    fn copy_match(&mut self, len: usize, dist: usize) -> Result<(), InflateError> {
        if dist as u64 > self.total {
            return Err(InflateError::DistanceTooFar);
        }
        self.reserve(len)?;
        let mut left = len;
        while left > 0 {
            let src = (self.pos + WINDOW - dist) & (WINDOW - 1);
            // Stay inside one linear stretch of the ring on both sides.
            let n = left.min(WINDOW - self.pos).min(WINDOW - src);
            if dist >= n {
                self.buf.copy_within(src..src + n, self.pos);
            } else {
                // The match overlaps its own output (dist < n, so src is
                // behind pos in this stretch): repeat the pattern by
                // doubling. Each step copies at most what is already
                // written after src, so source and destination are
                // disjoint ranges.
                let mut done = 0;
                while done < n {
                    let c = (n - done).min(dist + done);
                    self.buf.copy_within(src..src + c, self.pos + done);
                    done += c;
                }
            }
            self.pos += n;
            self.total += n as u64;
            left -= n;
            if self.pos == WINDOW {
                self.flush()?;
                self.pos = 0;
            }
        }
        Ok(())
    }
}

/// Stored block (RFC 1951 3.2.4): checks LEN against NLEN, then reserves LEN
/// bytes of the output limit before any data byte is read.
fn stored<R: Read>(bits: &mut Bits<R>, w: &mut Window) -> Result<(), InflateError> {
    bits.align();
    let len = bits.bits(16)?;
    let nlen = bits.bits(16)?;
    if len != !nlen & 0xFFFF {
        return Err(InflateError::BadStoredLength);
    }
    let mut left = len as usize;
    // Checked before reading: a lying length costs nothing past the limit.
    w.reserve(left)?;
    while left > 0 {
        let n = left.min(WINDOW - w.pos);
        bits.copy_into(&mut w.buf[w.pos..w.pos + n])?;
        w.pos += n;
        w.total += n as u64;
        left -= n;
        if w.pos == WINDOW {
            w.flush()?;
            w.pos = 0;
        }
    }
    Ok(())
}

fn codes<R: Read>(
    bits: &mut Bits<R>,
    w: &mut Window,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<(), InflateError> {
    loop {
        let sym = usize::from(lit.decode(bits)?);
        match sym {
            0..=255 => {
                w.reserve(1)?;
                w.put(sym as u8)?;
            }
            256 => return Ok(()),
            257..=285 => {
                let i = sym - 257;
                let len = usize::from(LEN_BASE[i]) + bits.bits(u32::from(LEN_EXTRA[i]))? as usize;
                let d = usize::from(dist.decode(bits)?);
                if d >= DIST_BASE.len() {
                    return Err(InflateError::BadCode);
                }
                let distance =
                    usize::from(DIST_BASE[d]) + bits.bits(u32::from(DIST_EXTRA[d]))? as usize;
                w.copy_match(len, distance)?;
            }
            _ => return Err(InflateError::BadCode),
        }
    }
}

fn fixed_tables() -> Result<(Huffman, Huffman), InflateError> {
    // RFC 1951 3.2.6: all 32 distance codes are 5 bits (30 and 31 are
    // never valid to use; `codes` rejects them).
    let mut l = [0u8; 288];
    l[..144].fill(8);
    l[144..256].fill(9);
    l[256..280].fill(7);
    l[280..].fill(8);
    Ok((Huffman::new(&l)?, Huffman::new(&[5u8; 32])?))
}

fn dynamic_tables<R: Read>(bits: &mut Bits<R>) -> Result<(Huffman, Huffman), InflateError> {
    let nlen = bits.bits(5)? as usize + 257;
    let ndist = bits.bits(5)? as usize + 1;
    let ncode = bits.bits(4)? as usize + 4;
    // RFC 1951 3.2.7: HLIT 257..=286, HDIST 1..=30 are the usable sizes.
    if nlen > 286 || ndist > 30 {
        return Err(InflateError::BadCode);
    }
    let mut cl = [0u8; 19];
    for &o in &CLEN_ORDER[..ncode] {
        cl[o] = bits.bits(3)? as u8;
    }
    let clh = Huffman::new(&cl)?;
    let mut lengths = [0u8; 286 + 30];
    let total = nlen + ndist;
    let mut i = 0;
    while i < total {
        let sym = clh.decode(bits)?;
        if sym < 16 {
            lengths[i] = sym as u8;
            i += 1;
            continue;
        }
        let (prev, rep) = match sym {
            16 => {
                if i == 0 {
                    return Err(InflateError::BadCode);
                }
                (lengths[i - 1], 3 + bits.bits(2)? as usize)
            }
            17 => (0, 3 + bits.bits(3)? as usize),
            _ => (0, 11 + bits.bits(7)? as usize),
        };
        if i + rep > total {
            return Err(InflateError::BadCode);
        }
        lengths[i..i + rep].fill(prev);
        i += rep;
    }
    // Without an end-of-block code the block could never end.
    if lengths[256] == 0 {
        return Err(InflateError::BadCode);
    }
    Ok((
        Huffman::new(&lengths[..nlen])?,
        Huffman::new(&lengths[nlen..total])?,
    ))
}

/// The block loop over a prepared reader; returns the bytes produced.
pub(crate) fn run<R: Read>(
    bits: &mut Bits<R>,
    limit: u64,
    out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<u64, InflateError> {
    let mut w = Window {
        buf: Box::new([0; WINDOW]),
        pos: 0,
        flushed: 0,
        total: 0,
        limit,
        out,
    };
    // The fixed tables never change: built at the first fixed block only,
    // so a stream of many empty fixed blocks costs O(1) per block, not a
    // table construction (~1 KiB of state, bounded).
    let mut fixed: Option<(Huffman, Huffman)> = None;
    loop {
        let last = bits.bits(1)? == 1;
        match bits.bits(2)? {
            0 => stored(bits, &mut w)?,
            1 => {
                if fixed.is_none() {
                    fixed = Some(fixed_tables()?);
                }
                if let Some((l, d)) = &fixed {
                    codes(bits, &mut w, l, d)?;
                }
            }
            2 => {
                let (l, d) = dynamic_tables(bits)?;
                codes(bits, &mut w, &l, &d)?;
            }
            _ => return Err(InflateError::BadBlockType),
        }
        if last {
            w.flush()?;
            return Ok(w.total);
        }
    }
}

/// Inflate a raw deflate stream (RFC 1951).
///
/// Output goes to `out` in chunks of at most 32 KiB. `limit` bounds the
/// bytes actually produced: a stream that would produce one more byte fails
/// with `OutputLimit` before that byte is written, so the chunks passed to
/// `out` never sum to more than `limit`. A stream of exactly `limit` bytes
/// is accepted. After an error, bytes decoded since the last chunk are
/// dropped, never delivered. Bytes after the final block are not read from
/// the stream's point of view (`in_bytes` stops at the block end), though
/// the reader may have buffered ahead.
pub fn inflate<R: Read>(
    input: R,
    limit: u64,
    out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<Inflated, InflateError> {
    let mut bits = Bits::new(input);
    let out_bytes = run(&mut bits, limit, out)?;
    bits.align();
    Ok(Inflated {
        in_bytes: bits.consumed(),
        out_bytes,
    })
}
