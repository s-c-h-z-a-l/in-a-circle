//! Streamed header check for any JPEG or PNG (CBZ pages copied unchanged).

//!
//! Unlike [`crate::probe`], which needs the whole file and accepts only
//! what the PDF writer can pass through, this check reads a stream up to
//! the end of the header and accepts every valid JPEG and PNG: the image is
//! stored byte for byte, so only "is this really a JPEG or PNG, and how
//! large is it" matters. Image data is never read (no IDAT inflate, no
//! entropy data); an image damaged inside passes as the file is.

use crate::ProbeError;
use crate::png::{MAX_31, SIGNATURE};
use flate::Crc32;
use std::io::{ErrorKind, Read};

/// The two image formats a header check recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Jpeg,
    Png,
}

impl Format {
    /// The file name extension for this format.
    pub fn ext(self) -> &'static str {
        match self {
            Format::Jpeg => "jpg",
            Format::Png => "png",
        }
    }
}

/// What an image's header says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sniffed {
    pub format: Format,
    pub width: u32,
    pub height: u32,
}

/// The discard buffer for skipped segment data. A JPEG segment is at most
/// 65533 data bytes (a 16-bit length that counts its own two bytes), so
/// 4 KiB skips the largest in 16 reads; it lives on the stack and is the
/// whole memory of the walk, whatever the segments declare.
const SKIP_BUF: usize = 4096;

/// At most this many JPEG markers and fill bytes before the SOF. A real
/// header holds tens of segments (APPn, DQT, DHT, the SOF) and a fill run of
/// a few bytes; 65536 is far above that and bounds the walk even on a
/// stream that never ends (a run of 0xFF fill bytes or of empty segments),
/// at about 65536 x 65537 B = 4 GiB read at worst. The caller's file-size
/// limit is the tighter, natural bound; this one only ends a loop that
/// would otherwise not end. Past it the file is `Damaged`.
const MAX_STEPS: u32 = 65_536;

/// The sample precisions some coding process allows (ITU T.81,
/// B.2.2: 8 for baseline, 8 or 12 for extended and progressive, 2 to 16
/// for lossless); the union is 2..=16, so no per-process table is needed.
const PRECISION: std::ops::RangeInclusive<u8> = 2..=16;

/// Horizontal and vertical sampling factors (T.81, B.2.2: 1 to 4).
const SAMPLING: std::ops::RangeInclusive<u8> = 1..=4;

/// Highest quantization table selector (T.81, B.2.2: 0 to 3).
const MAX_TQ: u8 = 3;

/// What the walk saw that the public result does not say, each a case
/// where the strict probe (no marker limit, DHP skipped, sampling factors
/// and table selectors unread) may decide otherwise. Only the fuzz target
/// and the tests read it.
#[derive(Default)]
struct Trace {
    /// Stopped at [`MAX_STEPS`].
    step_limit: bool,
    /// A hierarchical (DHP) header set the size.
    dhp: bool,
    /// A frame header field was out of its range.
    field: bool,
}

/// Check the header of the image `r` streams: a PNG signature and a valid
/// first IHDR chunk (CRC included), or a JPEG SOI and a SOF segment before
/// the first SOS. Any valid bit depth, color type, interlace, precision,
/// component count or coding process is accepted; nothing past the header
/// is read or decoded. Files with more than `max_pixels` pixels are
/// `TooLarge`; for a hierarchical JPEG both the DHP segment's size (the
/// completed image, reported) and the first frame's must fit.
///
/// Reads at most what the header's own lengths allow: the stream is left
/// at the end of the IHDR chunk or of the SOF segment, never further. A
/// JPEG may put many segments before its SOF, so that can be up to about
/// 4 GiB; the caller bounds `r` to its own file-size limit (e.g.
/// `take(ENTRY_BYTES)`), and the end of that bound is `Truncated`. No
/// heap allocation; the only buffer is 4 KiB on the stack.
/// The end of the stream inside the header is `Truncated`; a read error is
/// `Damaged` (the header cannot be confirmed, so the image is refused).
pub fn sniff(r: &mut impl Read, max_pixels: u64) -> Result<Sniffed, ProbeError> {
    traced(r, max_pixels, &mut Trace::default())
}

/// Fuzz entry point: [`sniff`], plus why the strict probe may disagree on
/// this input, if it may: `"marker limit"` (always with `Damaged`),
/// `"hierarchical"` or `"frame field"`.
#[cfg(fuzzing)]
pub fn fuzz_sniff(
    r: &mut impl Read,
    max_pixels: u64,
) -> (Result<Sniffed, ProbeError>, Option<&'static str>) {
    let mut t = Trace::default();
    let res = traced(r, max_pixels, &mut t);
    let why = if t.step_limit {
        Some("marker limit")
    } else if t.dhp {
        Some("hierarchical")
    } else if t.field {
        Some("frame field")
    } else {
        None
    };
    (res, why)
}

fn traced<R: Read + ?Sized>(
    r: &mut R,
    max_pixels: u64,
    t: &mut Trace,
) -> Result<Sniffed, ProbeError> {
    let mut head = [0u8; 3];
    let n = fill(r, &mut head)?;
    if n == head.len() && head == [0xFF, 0xD8, 0xFF] {
        return jpeg(r, max_pixels, t);
    }
    if n == head.len() && head == SIGNATURE[..3] {
        let mut rest = [0u8; 5];
        if fill(r, &mut rest)? == rest.len() && rest == SIGNATURE[3..] {
            return png(r, max_pixels);
        }
    }
    Err(ProbeError::NotImage)
}

/// Read until `buf` is full or the stream ends; returns the bytes read.
/// `Interrupted` is retried (it is not an error of the stream); any other
/// error, or a reader claiming more bytes than it was given room for, is
/// `Damaged`.
fn fill<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> Result<usize, ProbeError> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(k) => {
                got = got
                    .checked_add(k)
                    .filter(|&g| g <= buf.len())
                    .ok_or(ProbeError::Damaged)?;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return Err(ProbeError::Damaged),
        }
    }
    Ok(got)
}

/// Fill `buf` completely; the end of the stream first is `Truncated`.
fn exact<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> Result<(), ProbeError> {
    if fill(r, buf)? == buf.len() {
        Ok(())
    } else {
        Err(ProbeError::Truncated)
    }
}

fn byte<R: Read + ?Sized>(r: &mut R) -> Result<u8, ProbeError> {
    let mut b = [0u8; 1];
    exact(r, &mut b)?;
    Ok(b[0])
}

/// Read and drop `n` bytes through the fixed buffer.
fn skip<R: Read + ?Sized>(r: &mut R, mut n: usize) -> Result<(), ProbeError> {
    let mut buf = [0u8; SKIP_BUF];
    while n > 0 {
        let take = n.min(SKIP_BUF);
        exact(r, &mut buf[..take])?;
        n -= take;
    }
    Ok(())
}

fn pixels_ok(width: u32, height: u32, max_pixels: u64) -> Result<(), ProbeError> {
    if u64::from(width) * u64::from(height) > max_pixels {
        Err(ProbeError::TooLarge)
    } else {
        Ok(())
    }
}

/// After the signature: the first chunk must be a 13-byte IHDR with a
/// matching CRC.
fn png<R: Read + ?Sized>(r: &mut R, max_pixels: u64) -> Result<Sniffed, ProbeError> {
    let mut head = [0u8; 8];
    exact(r, &mut head)?;
    let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    // Checked before reading the chunk: its length alone sizes nothing.
    if &head[4..] != b"IHDR" || len != 13 {
        return Err(ProbeError::Damaged);
    }
    let mut body = [0u8; 13 + 4];
    exact(r, &mut body)?;
    let (d, crc) = body.split_at(13);
    let mut c = Crc32::new();
    c.update(&head[4..]);
    c.update(d);
    if c.finish().to_be_bytes() != crc {
        return Err(ProbeError::Damaged);
    }
    let width = u32::from_be_bytes([d[0], d[1], d[2], d[3]]);
    let height = u32::from_be_bytes([d[4], d[5], d[6], d[7]]);
    let (depth, color, compression, filter, interlace) = (d[8], d[9], d[10], d[11], d[12]);
    if width == 0 || height == 0 || width > MAX_31 || height > MAX_31 {
        return Err(ProbeError::Damaged);
    }
    if compression != 0 || filter != 0 || interlace > 1 {
        return Err(ProbeError::Damaged);
    }
    // The bit depths each color type allows (PNG spec, section 11.2.2).
    let valid = match color {
        0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
        3 => matches!(depth, 1 | 2 | 4 | 8),
        2 | 4 | 6 => matches!(depth, 8 | 16),
        _ => false,
    };
    if !valid {
        return Err(ProbeError::Damaged);
    }
    pixels_ok(width, height, max_pixels)?;
    Ok(Sniffed {
        format: Format::Png,
        width,
        height,
    })
}

/// After SOI and the first marker's 0xFF: walk the markers to the first
/// SOF, skipping every segment by its length.
fn jpeg<R: Read + ?Sized>(
    r: &mut R,
    max_pixels: u64,
    t: &mut Trace,
) -> Result<Sniffed, ProbeError> {
    let mut steps = 0u32;
    let mut step = |t: &mut Trace| {
        steps += 1;
        if steps > MAX_STEPS {
            t.step_limit = true;
            Err(ProbeError::Damaged)
        } else {
            Ok(())
        }
    };
    // The size of the completed image, from a hierarchical header.
    let mut dhp = None;
    // The first marker's 0xFF was the third byte of the signature.
    let mut first = true;
    loop {
        if !first && byte(r)? != 0xFF {
            return Err(ProbeError::Damaged);
        }
        first = false;
        let mut code = byte(r)?;
        // Fill bytes: any number of 0xFF before the code.
        while code == 0xFF {
            step(t)?;
            code = byte(r)?;
        }
        step(t)?;
        match code {
            // A stuffed zero outside entropy data is not a marker.
            0x00 => return Err(ProbeError::Damaged),
            // TEM, RSTn: standalone, no length.
            0x01 | 0xD0..=0xD7 => continue,
            // SOI again, EOI, or SOS before any SOF: no frame header.
            0xD8..=0xDA => return Err(ProbeError::Damaged),
            _ => {}
        }
        let mut lb = [0u8; 2];
        exact(r, &mut lb)?;
        let len = usize::from(u16::from_be_bytes(lb));
        // The length counts its own two bytes.
        let body = len.checked_sub(2).ok_or(ProbeError::Damaged)?;
        match code {
            // SOF0-SOF15, every coding process; C4 (DHT), C8 (JPG) and CC
            // (DAC) share the range but are not frame headers.
            0xC0..=0xCF if !matches!(code, 0xC4 | 0xC8 | 0xCC) => {
                let frame_size = frame(r, body, max_pixels, t)?;
                // T.81, B.3.2: the DHP gives the completed image's size; the
                // frames are its (possibly smaller) levels.
                let (width, height) = dhp.unwrap_or(frame_size);
                return Ok(Sniffed {
                    format: Format::Jpeg,
                    width,
                    height,
                });
            }
            // DHP: the frame header layout (T.81, B.3.2), for the whole
            // hierarchical image. Its size counts for the pixel limit too,
            // or a small first frame would hide a huge image.
            0xDE => {
                t.dhp = true;
                dhp = Some(frame(r, body, max_pixels, t)?);
            }
            _ => skip(r, body)?,
        }
    }
}

/// SOF (or DHP) segment body: precision, height, width, component count,
/// then per component an id, the sampling factors and a quantization table
/// selector (ITU T.81, B.2.2). Each field must be in the range some coding
/// process allows; no per-process check: the image is copied, not decoded.
/// Returns (width, height), within `max_pixels`.
fn frame<R: Read + ?Sized>(
    r: &mut R,
    body: usize,
    max_pixels: u64,
    t: &mut Trace,
) -> Result<(u32, u32), ProbeError> {
    if body < 6 {
        return Err(ProbeError::Damaged);
    }
    let mut f = [0u8; 6];
    exact(r, &mut f)?;
    let height = u32::from(u16::from_be_bytes([f[1], f[2]]));
    let width = u32::from(u16::from_be_bytes([f[3], f[4]]));
    let n = usize::from(f[5]);
    if n == 0 || body != 6 + 3 * n {
        return Err(ProbeError::Damaged);
    }
    if !PRECISION.contains(&f[0]) {
        t.field = true;
        return Err(ProbeError::Damaged);
    }
    // Height 0 means "defined by a DNL marker after the first scan"; the
    // scan is not walked, so the size is unknown and the file is refused.
    if width == 0 || height == 0 {
        return Err(ProbeError::Damaged);
    }
    for _ in 0..n {
        let mut c = [0u8; 3];
        exact(r, &mut c)?;
        let (h, v) = (c[1] >> 4, c[1] & 0x0F);
        if !SAMPLING.contains(&h) || !SAMPLING.contains(&v) || c[2] > MAX_TQ {
            t.field = true;
            return Err(ProbeError::Damaged);
        }
    }
    pixels_ok(width, height, max_pixels)?;
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Image, probe};
    use flate::Crc32;
    use std::cell::Cell;
    use std::io;

    const MAX: u64 = 180_000_000;

    const GRAY: &[u8] = include_bytes!("../tests/data/gray.jpg");
    const RGB: &[u8] = include_bytes!("../tests/data/rgb.jpg");
    const PROGRESSIVE: &[u8] = include_bytes!("../tests/data/progressive.jpg");
    const CMYK: &[u8] = include_bytes!("../tests/data/cmyk_adobe.jpg");
    const EXIF: &[u8] = include_bytes!("../tests/data/exif_orientation6.jpg");
    const PRECISION12: &[u8] = include_bytes!("../tests/data/precision12.jpg");
    const ARITHMETIC: &[u8] = include_bytes!("../tests/data/arithmetic.jpg");
    const LOSSLESS: &[u8] = include_bytes!("../tests/data/lossless.jpg");

    fn s(bytes: &[u8]) -> Result<Sniffed, ProbeError> {
        sniff(&mut &bytes[..], MAX)
    }

    fn ok(format: Format, width: u32, height: u32) -> Result<Sniffed, ProbeError> {
        Ok(Sniffed {
            format,
            width,
            height,
        })
    }

    fn crc(kind: &[u8], data: &[u8]) -> u32 {
        let mut c = Crc32::new();
        c.update(kind);
        c.update(data);
        c.finish()
    }

    /// A PNG from `testzip::png` (rows: one zero filter byte per row,
    /// zero samples; the sniff never looks at them).
    fn png(w: u32, h: u32, color: u8, palette: Option<&[u8]>, trns: bool, il: bool) -> Vec<u8> {
        let channels = match color {
            2 => 3,
            4 => 2,
            6 => 4,
            _ => 1,
        };
        let rows = vec![0u8; h as usize * (1 + w as usize * channels)];
        testzip::png(w, h, color, palette, trns, il, &rows)
    }

    /// Rewrite the 13 IHDR bytes (offset 16) and fix the CRC.
    fn patch_ihdr(mut png: Vec<u8>, f: impl FnOnce(&mut [u8])) -> Vec<u8> {
        f(&mut png[16..29]);
        let c = crc(b"IHDR", &png[16..29]);
        png[29..33].copy_from_slice(&c.to_be_bytes());
        png
    }

    fn depth(png: Vec<u8>, d: u8) -> Vec<u8> {
        patch_ihdr(png, |h| h[8] = d)
    }

    /// A header-only JPEG: SOI, one SOF segment, an SOS header.
    fn jpeg_sof(marker: u8, precision: u8, height: u16, width: u16, n: u8) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, marker];
        v.extend_from_slice(&(8 + 3 * u16::from(n)).to_be_bytes());
        v.push(precision);
        v.extend_from_slice(&height.to_be_bytes());
        v.extend_from_slice(&width.to_be_bytes());
        v.push(n);
        for i in 0..n {
            v.extend_from_slice(&[i + 1, 0x11, 0]);
        }
        v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
        v
    }

    /// A marker segment with `body` as its data (`body.len() <= 65533`).
    fn segment(marker: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, marker];
        v.extend_from_slice(&u16::try_from(body.len() + 2).unwrap().to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    /// `jpeg` with `segs` inserted right after SOI.
    fn after_soi(jpeg: &[u8], segs: &[u8]) -> Vec<u8> {
        let mut v = jpeg[..2].to_vec();
        v.extend_from_slice(segs);
        v.extend_from_slice(&jpeg[2..]);
        v
    }

    #[test]
    fn format_ext() {
        assert_eq!(Format::Jpeg.ext(), "jpg");
        assert_eq!(Format::Png.ext(), "png");
    }

    #[test]
    fn sniff_accepts_every_valid_variant() {
        let pal: &[u8] = &[0, 0, 0, 255, 255, 255];
        let pngs: Vec<(&str, Vec<u8>, u32, u32)> = vec![
            ("gray 8", png(5, 3, 0, None, false, false), 5, 3),
            ("gray 1", depth(png(9, 2, 0, None, false, false), 1), 9, 2),
            ("gray 16", depth(png(4, 4, 0, None, false, false), 16), 4, 4),
            ("rgb 8", png(6, 2, 2, None, false, false), 6, 2),
            ("rgb 16", depth(png(6, 2, 2, None, false, false), 16), 6, 2),
            ("rgba 8", png(3, 7, 6, None, false, false), 3, 7),
            ("rgba 16", depth(png(3, 7, 6, None, false, false), 16), 3, 7),
            ("gray+alpha", png(2, 8, 4, None, false, false), 2, 8),
            (
                "gray+alpha 16",
                depth(png(2, 8, 4, None, false, false), 16),
                2,
                8,
            ),
            ("palette + tRNS", png(4, 1, 3, Some(pal), true, false), 4, 1),
            (
                "palette 4",
                depth(png(4, 1, 3, Some(pal), false, false), 4),
                4,
                1,
            ),
            ("gray + tRNS", png(5, 5, 0, None, true, false), 5, 5),
            ("interlaced", png(7, 5, 2, None, false, true), 7, 5),
        ];
        for (what, bytes, w, h) in &pngs {
            assert_eq!(s(bytes), ok(Format::Png, *w, *h), "{what}");
        }
        for (what, bytes, w, h) in [
            ("gray", GRAY, 24, 16),
            ("baseline", RGB, 32, 24),
            ("progressive", PROGRESSIVE, 40, 30),
            ("cmyk", CMYK, 16, 16),
            ("exif", EXIF, 20, 10),
            ("12-bit SOF1", PRECISION12, 12, 8),
            ("arithmetic SOF9", ARITHMETIC, 14, 6),
            ("lossless SOF3", LOSSLESS, 10, 9),
        ] {
            assert_eq!(s(bytes), ok(Format::Jpeg, w, h), "{what}");
        }
        // Every frame type, whatever its precision and component count;
        // the three codes in the SOF range that are not frames (DHT, JPG,
        // DAC) are skipped as segments, so a stream with only them has no
        // SOF before SOS.
        for m in 0xC0..=0xCFu8 {
            let j = jpeg_sof(m, 12, 300, 200, 4);
            if matches!(m, 0xC4 | 0xC8 | 0xCC) {
                assert_eq!(s(&j), Err(ProbeError::Damaged), "{m:#x}");
            } else {
                assert_eq!(s(&j), ok(Format::Jpeg, 200, 300), "{m:#x}");
            }
        }
        for (precision, n) in [(2, 1), (16, 3), (8, 2), (8, 255)] {
            assert_eq!(
                s(&jpeg_sof(0xC3, precision, 3, 5, n)),
                ok(Format::Jpeg, 5, 3),
                "P={precision} Nf={n}"
            );
        }
        // Fill bytes before a marker, standalone markers (TEM, RST) between
        // segments, and a DHT before the SOF.
        let mut j = vec![0xFF, 0xD8, 0xFF, 0xFF, 0xFF, 0x01, 0xFF, 0xD3];
        j.extend_from_slice(&segment(0xC4, &[0; 20]));
        j.extend_from_slice(&jpeg_sof(0xC0, 8, 2, 3, 1)[2..]);
        assert_eq!(s(&j), ok(Format::Jpeg, 3, 2));
    }

    thread_local! {
        static ALLOCATED: Cell<usize> = const { Cell::new(0) };
    }
    /// Counts, per thread, the bytes allocated or grown into.
    struct Counting;
    // SAFETY: every method forwards to `System` with the arguments it got;
    // the only addition is a counter in a const-initialized thread-local
    // that needs no allocation.
    unsafe impl std::alloc::GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
            let _ = ALLOCATED.try_with(|c| c.set(c.get() + l.size()));
            unsafe { std::alloc::System.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
            unsafe { std::alloc::System.dealloc(p, l) }
        }
        unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, n: usize) -> *mut u8 {
            let _ = ALLOCATED.try_with(|c| c.set(c.get() + n.saturating_sub(l.size())));
            unsafe { std::alloc::System.realloc(p, l, n) }
        }
    }
    #[global_allocator]
    static COUNTING: Counting = Counting;

    /// A reader that records the largest read and the bytes handed out.
    struct Watch<'a> {
        data: &'a [u8],
        given: usize,
        largest: usize,
    }
    impl Read for Watch<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.largest = self.largest.max(buf.len());
            let n = buf.len().min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            self.given += n;
            Ok(n)
        }
    }

    #[test]
    fn sniff_finds_sof_after_large_app_segments() {
        // An EXIF thumbnail and a multi-segment ICC profile: three APP1 at
        // the largest segment length, then an APP2, all before the SOF.
        let mut segs = Vec::new();
        for i in 0..3u8 {
            segs.extend_from_slice(&segment(0xE1, &[i; 65_533]));
        }
        let mut icc = b"ICC_PROFILE\0\x01\x01".to_vec();
        icc.resize(40_000, 0xAB);
        segs.extend_from_slice(&segment(0xE2, &icc));
        let j = after_soi(RGB, &segs);
        assert!(j.len() > 3 * 65_536);

        let mut w = Watch {
            data: &j,
            given: 0,
            largest: 0,
        };
        let before = ALLOCATED.with(Cell::get);
        let got = sniff(&mut w, MAX);
        let allocated = ALLOCATED.with(Cell::get) - before;
        assert_eq!(got, ok(Format::Jpeg, 32, 24));
        // Memory: no heap at all, and no read larger than the fixed buffer.
        assert_eq!(allocated, 0, "the sniff allocated {allocated} B");
        assert!(w.largest <= 4096, "a read of {} B", w.largest);
        // No read-ahead: the stream is consumed exactly to the end of the
        // SOF segment, the last byte the marker lengths allow.
        let sof = j.windows(2).position(|m| m == [0xFF, 0xC0]).unwrap();
        let sof_len = usize::from(u16::from_be_bytes([j[sof + 2], j[sof + 3]]));
        assert_eq!(w.given, sof + 2 + sof_len);
    }

    #[test]
    fn png_reads_only_the_signature_and_ihdr() {
        let p = png(5, 3, 6, None, false, false);
        let mut w = Watch {
            data: &p,
            given: 0,
            largest: 0,
        };
        assert_eq!(sniff(&mut w, MAX), ok(Format::Png, 5, 3));
        assert_eq!(w.given, 33);
    }

    #[test]
    fn sniff_rejects() {
        use ProbeError::{Damaged, NotImage, TooLarge, Truncated};
        // Not an image, or only part of a signature.
        for b in [
            &b""[..],
            b"GIF89a",
            b"hello world",
            b"\xFF\xD8",
            b"\xFF\xD8\x00",
            b"\x89PNG\r\n\x1a",
            b"\x89PNG\r\n\x1a\x0b",
        ] {
            assert_eq!(s(b), Err(NotImage), "{b:?}");
        }

        let p = png(5, 3, 2, None, false, false);
        // IHDR cut anywhere: inside the chunk head, the data, the CRC.
        for cut in [8, 12, 16, 20, 29, 32] {
            assert_eq!(s(&p[..cut]), Err(Truncated), "cut at {cut}");
        }
        // Bad IHDR CRC.
        let mut bad = p.clone();
        bad[30] ^= 1;
        assert_eq!(s(&bad), Err(Damaged));
        // IHDR not first, or with another length.
        let mut not_first = p.clone();
        not_first[12..16].copy_from_slice(b"IDAT");
        assert_eq!(s(&not_first), Err(Damaged));
        for len in [0u32, 12, 14, 0x7FFF_FFFF, u32::MAX] {
            let mut l = p.clone();
            l[8..12].copy_from_slice(&len.to_be_bytes());
            assert_eq!(s(&l), Err(Damaged), "IHDR length {len}");
        }
        // Zero or over-31-bit dimensions.
        for (w, h) in [(0u32, 3u32), (5, 0), (0x8000_0000, 1), (1, 0x8000_0000)] {
            let q = patch_ihdr(p.clone(), |d| {
                d[0..4].copy_from_slice(&w.to_be_bytes());
                d[4..8].copy_from_slice(&h.to_be_bytes());
            });
            assert_eq!(s(&q), Err(Damaged), "{w}x{h}");
        }
        // Every invalid bit depth / color type pair, and the method fields.
        for color in 0..=7u8 {
            for d in [0u8, 1, 2, 3, 4, 5, 8, 16, 32] {
                let valid = match color {
                    0 => matches!(d, 1 | 2 | 4 | 8 | 16),
                    3 => matches!(d, 1 | 2 | 4 | 8),
                    2 | 4 | 6 => matches!(d, 8 | 16),
                    _ => false,
                };
                let q = patch_ihdr(p.clone(), |h| {
                    h[8] = d;
                    h[9] = color;
                });
                let want = if valid {
                    ok(Format::Png, 5, 3)
                } else {
                    Err(Damaged)
                };
                assert_eq!(s(&q), want, "color {color} depth {d}");
            }
        }
        for (i, v) in [(10, 1u8), (11, 1), (12, 2)] {
            let q = patch_ihdr(p.clone(), |h| h[i] = v);
            assert_eq!(s(&q), Err(Damaged), "IHDR byte {i} = {v}");
        }
        // 200 megapixels.
        let big = patch_ihdr(p.clone(), |d| {
            d[0..4].copy_from_slice(&20_000u32.to_be_bytes());
            d[4..8].copy_from_slice(&10_000u32.to_be_bytes());
        });
        assert_eq!(s(&big), Err(TooLarge));

        // JPEG: SOS, EOI or SOI before any SOF; a stuffed zero outside
        // entropy data; a marker without its 0xFF.
        for b in [
            &[0xFF, 0xD8, 0xFF, 0xDA, 0, 2][..],
            &[0xFF, 0xD8, 0xFF, 0xD9],
            &[0xFF, 0xD8, 0xFF, 0xD8],
            &[0xFF, 0xD8, 0xFF, 0x00],
            &[0xFF, 0xD8, 0xFF, 0xE0, 0, 2, 0x12, 0xC0],
        ] {
            assert_eq!(s(b), Err(Damaged), "{b:?}");
        }
        // A segment length below its own two bytes.
        for len in [0u8, 1] {
            assert_eq!(
                s(&[0xFF, 0xD8, 0xFF, 0xE0, 0, len, 0xFF, 0xDA]),
                Err(Damaged)
            );
        }
        // EOF anywhere before the SOF ends: in a fill run, a length, an
        // APP segment's body, the SOF's fixed part or its components.
        let app = after_soi(RGB, &segment(0xE1, &[7; 1000]));
        let sof = app.windows(2).position(|m| m == [0xFF, 0xC0]).unwrap();
        for cut in [3, 4, 5, 500, 1005, sof + 1, sof + 3, sof + 7, sof + 12] {
            assert_eq!(s(&app[..cut]), Err(Truncated), "cut at {cut}");
        }
        assert_eq!(s(&[0xFF, 0xD8, 0xFF, 0xFF, 0xFF]), Err(Truncated));
        // SOF with zero width or height, no components, or a length that
        // disagrees with its component count.
        assert_eq!(s(&jpeg_sof(0xC0, 8, 8, 0, 3)), Err(Damaged));
        assert_eq!(s(&jpeg_sof(0xC0, 8, 0, 8, 3)), Err(Damaged));
        assert_eq!(s(&jpeg_sof(0xC0, 8, 8, 8, 0)), Err(Damaged));
        for len in [2u16, 7, 10, 12, 14] {
            let mut j = jpeg_sof(0xC0, 8, 8, 8, 1);
            j[4..6].copy_from_slice(&len.to_be_bytes());
            assert_eq!(s(&j), Err(Damaged), "SOF length {len}");
        }
        // 200 megapixels (the 16-bit fields allow up to 4.3 billion).
        assert_eq!(s(&jpeg_sof(0xC0, 8, 10_000, 20_000, 3)), Err(TooLarge));
        assert_eq!(
            sniff(&mut &jpeg_sof(0xC0, 8, 100, 100, 3)[..], 10_000),
            ok(Format::Jpeg, 100, 100)
        );
        assert_eq!(
            sniff(&mut &jpeg_sof(0xC0, 8, 100, 101, 3)[..], 10_000),
            Err(TooLarge)
        );
    }

    /// A frame header body (SOF or DHP layout) with `hv` as every
    /// component's sampling byte and `tq` as its table selector.
    fn frame_body(precision: u8, height: u16, width: u16, n: u8, hv: u8, tq: u8) -> Vec<u8> {
        let mut v = vec![precision];
        v.extend_from_slice(&height.to_be_bytes());
        v.extend_from_slice(&width.to_be_bytes());
        v.push(n);
        for i in 0..n {
            v.extend_from_slice(&[i + 1, hv, tq]);
        }
        v
    }

    /// SOI, `segs`, then a SOF0 segment with `sof` as its body and an SOS.
    fn jpeg_with(segs: &[u8], sof: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        v.extend_from_slice(segs);
        v.extend_from_slice(&segment(0xC0, sof));
        v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
        v
    }

    #[test]
    fn sniff_checks_frame_header_fields() {
        use ProbeError::Damaged;
        // Precision outside 2..=16, a sampling factor outside 1..=4, or a
        // table selector above 3, in any component.
        for (what, body) in [
            ("P=0", frame_body(0, 5, 5, 1, 0x11, 0)),
            ("P=1", frame_body(1, 5, 5, 1, 0x11, 0)),
            ("P=17", frame_body(17, 5, 5, 1, 0x11, 0)),
            ("P=255", frame_body(255, 5, 5, 1, 0x11, 0)),
            ("H=0", frame_body(8, 5, 5, 3, 0x01, 0)),
            ("V=0", frame_body(8, 5, 5, 3, 0x10, 0)),
            ("H=5", frame_body(8, 5, 5, 3, 0x51, 0)),
            ("V=5", frame_body(8, 5, 5, 3, 0x15, 0)),
            ("H=V=0", frame_body(8, 5, 5, 1, 0x00, 0)),
            ("Tq=4", frame_body(8, 5, 5, 3, 0x11, 4)),
            ("Tq=255", frame_body(8, 5, 5, 1, 0x11, 255)),
        ] {
            assert_eq!(s(&jpeg_with(&[], &body)), Err(Damaged), "{what}");
        }
        // Only the last component bad.
        let mut body = frame_body(8, 5, 5, 3, 0x11, 0);
        body[6 + 2 * 3 + 1] = 0x50;
        assert_eq!(s(&jpeg_with(&[], &body)), Err(Damaged), "last H=5");
        // The edges of every range are valid.
        for (p, hv, tq) in [(2, 0x11, 0), (16, 0x44, 3), (12, 0x14, 1), (8, 0x41, 2)] {
            assert_eq!(
                s(&jpeg_with(&[], &frame_body(p, 5, 7, 3, hv, tq))),
                ok(Format::Jpeg, 7, 5),
                "P={p} HV={hv:#x} Tq={tq}"
            );
        }
    }

    #[test]
    fn sniff_counts_the_hierarchical_size() {
        use ProbeError::{Damaged, TooLarge, Truncated};
        let small = frame_body(8, 1, 1, 1, 0x11, 0);
        // A 4.3-gigapixel DHP before a 1x1 first frame.
        let dhp = segment(0xDE, &frame_body(8, 65_535, 65_535, 1, 0x11, 0));
        assert_eq!(s(&jpeg_with(&dhp, &small)), Err(TooLarge));
        // Just over and at the limit: the frame is small either way.
        let dhp = segment(0xDE, &frame_body(8, 100, 101, 1, 0x11, 0));
        assert_eq!(
            sniff(&mut &jpeg_with(&dhp, &small)[..], 10_000),
            Err(TooLarge)
        );
        let dhp = segment(0xDE, &frame_body(8, 100, 100, 1, 0x11, 0));
        assert_eq!(
            sniff(&mut &jpeg_with(&dhp, &small)[..], 10_000),
            ok(Format::Jpeg, 100, 100)
        );
        // The frame still counts after a small DHP.
        let dhp = segment(0xDE, &frame_body(8, 2, 2, 1, 0x11, 0));
        let big = frame_body(8, 10_000, 20_000, 1, 0x11, 0);
        assert_eq!(s(&jpeg_with(&dhp, &big)), Err(TooLarge));
        // The reported size is the completed image's (the DHP's).
        let dhp = segment(0xDE, &frame_body(8, 30, 40, 3, 0x11, 0));
        let level = frame_body(8, 15, 20, 3, 0x11, 0);
        assert_eq!(s(&jpeg_with(&dhp, &level)), ok(Format::Jpeg, 40, 30));
        // A DHP is checked like a frame header.
        for body in [
            frame_body(0, 30, 40, 1, 0x11, 0),
            frame_body(8, 0, 40, 1, 0x11, 0),
            frame_body(8, 30, 40, 1, 0x05, 0),
            frame_body(8, 30, 40, 0, 0x11, 0),
            vec![8, 0, 30, 0],
        ] {
            assert_eq!(s(&jpeg_with(&segment(0xDE, &body), &level)), Err(Damaged));
        }
        let cut = jpeg_with(&dhp, &level);
        assert_eq!(s(&cut[..2 + 4 + 5]), Err(Truncated));
    }

    #[test]
    fn trace_says_why_the_walk_stopped() {
        fn trace(b: &[u8]) -> (Result<Sniffed, ProbeError>, bool, bool, bool) {
            let mut t = Trace::default();
            let r = traced(&mut &b[..], MAX, &mut t);
            (r, t.step_limit, t.dhp, t.field)
        }
        let mut j = vec![0xFF, 0xD8];
        j.extend(std::iter::repeat_n([0xFF, 0xE0, 0, 2], 70_000).flatten());
        assert_eq!(trace(&j), (Err(ProbeError::Damaged), true, false, false));
        let mut fill = io::Read::chain(&[0xFF, 0xD8][..], Endless(0xFF));
        let mut t = Trace::default();
        assert_eq!(traced(&mut fill, MAX, &mut t), Err(ProbeError::Damaged));
        assert!(t.step_limit);
        // Damaged for another reason after more than 64 KiB: not the limit.
        let big = after_soi(
            &jpeg_with(&[], &frame_body(0, 5, 5, 1, 0x11, 0)),
            &segment(0xE1, &[1; 65_533]),
        );
        assert!(big.len() > MAX_STEPS as usize);
        assert_eq!(trace(&big), (Err(ProbeError::Damaged), false, false, true));
        let no_sof = after_soi(
            &[0xFF, 0xD8, 0xFF, 0xDA, 0, 2],
            &segment(0xE1, &[1; 65_533]),
        );
        assert!(no_sof.len() > MAX_STEPS as usize);
        assert_eq!(
            trace(&no_sof),
            (Err(ProbeError::Damaged), false, false, false)
        );
        assert_eq!(trace(RGB), (ok(Format::Jpeg, 32, 24), false, false, false));
        // Found by the fuzzer: the probe reads no sampling factors, so it
        // accepts a component with H=14, V=7; the sniff refuses it and says
        // why, so the fuzz target does not count it as a disagreement.
        let sof = RGB.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        let mut bad = RGB.to_vec();
        bad[sof + 10 + 2 * 3 + 1] = 0xE7;
        assert!(probe(&bad, MAX).is_ok());
        assert_eq!(trace(&bad), (Err(ProbeError::Damaged), false, false, true));
        let dhp = segment(0xDE, &frame_body(8, 30, 40, 1, 0x11, 0));
        let h = jpeg_with(&dhp, &frame_body(8, 15, 20, 1, 0x11, 0));
        assert_eq!(trace(&h), (ok(Format::Jpeg, 40, 30), false, true, false));
    }

    /// A reader that never ends: every byte is `byte`.
    struct Endless(u8);
    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            buf.fill(self.0);
            Ok(buf.len())
        }
    }

    /// SOI, the first marker's 0xFF, `fills` more fill bytes, then a SOF0
    /// segment: the walk takes `fills + 1` steps to reach the frame header.
    fn jpeg_after_fills(fills: usize) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF];
        v.extend(std::iter::repeat_n(0xFF, fills));
        v.extend_from_slice(&[0xC0, 0x00, 0x0B]);
        v.extend_from_slice(&frame_body(8, 5, 7, 1, 0x11, 0));
        v
    }

    #[test]
    fn the_marker_walk_allows_exactly_the_step_limit() {
        let max = MAX_STEPS as usize;
        assert_eq!(s(&jpeg_after_fills(max - 1)), ok(Format::Jpeg, 7, 5));
        assert_eq!(s(&jpeg_after_fills(max)), Err(ProbeError::Damaged));
        assert_eq!(s(&jpeg_after_fills(max + 1)), Err(ProbeError::Damaged));
    }

    #[test]
    fn a_png_dimension_of_exactly_31_bits_is_valid() {
        let p = png(5, 3, 2, None, false, false);
        for (w, h) in [(MAX_31, 1u32), (1, MAX_31)] {
            let q = patch_ihdr(p.clone(), |d| {
                d[0..4].copy_from_slice(&w.to_be_bytes());
                d[4..8].copy_from_slice(&h.to_be_bytes());
            });
            assert_eq!(sniff(&mut &q[..], u64::MAX), ok(Format::Png, w, h));
        }
    }

    #[test]
    fn a_wrong_signature_is_not_an_image_even_if_the_rest_matches() {
        // The first three bytes are not the PNG signature's, the next five
        // are: only the whole signature makes a PNG.
        let mut q = png(5, 3, 2, None, false, false);
        q[..3].copy_from_slice(b"abc");
        assert_eq!(s(&q), Err(ProbeError::NotImage));
    }

    #[test]
    fn a_frame_shorter_than_its_header_is_damaged_before_any_read() {
        // Length 2 (no body) and 8 (body of 6, which is a header with no
        // components): the first is damaged by its length alone, the second
        // is read, and the stream ends in it.
        let head = [0xFF, 0xD8, 0xFF, 0xC0];
        let mut a = head.to_vec();
        a.extend_from_slice(&[0x00, 0x02]);
        assert_eq!(s(&a), Err(ProbeError::Damaged));
        let mut b = head.to_vec();
        b.extend_from_slice(&[0x00, 0x08, 8, 0]);
        assert_eq!(s(&b), Err(ProbeError::Truncated));
    }

    /// Fails the test if asked to read into an empty buffer.
    struct NoEmptyReads<'a>(&'a [u8]);
    impl Read for NoEmptyReads<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Err(io::Error::other("read into an empty buffer"));
            }
            let n = buf.len().min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn a_full_buffer_is_not_read_into_again() {
        assert_eq!(sniff(&mut NoEmptyReads(RGB), MAX), ok(Format::Jpeg, 32, 24));
        let p = png(5, 3, 2, None, false, false);
        assert_eq!(sniff(&mut NoEmptyReads(&p), MAX), ok(Format::Png, 5, 3));
    }

    #[test]
    fn sniff_stops_on_an_endless_stream() {
        // An endless run of fill bytes, and endless empty segments: the
        // marker limit ends both.
        let mut fill = io::Read::chain(&[0xFF, 0xD8][..], Endless(0xFF));
        assert_eq!(sniff(&mut fill, MAX), Err(ProbeError::Damaged));
        let segs = std::iter::repeat_n([0xFF, 0xE0, 0, 2], 70_000).flatten();
        let mut j = vec![0xFF, 0xD8];
        j.extend(segs);
        assert_eq!(s(&j), Err(ProbeError::Damaged));
        // Under the limit the walk goes on: 60 000 empty segments, then a
        // SOF.
        let mut j = vec![0xFF, 0xD8];
        j.extend(std::iter::repeat_n([0xFF, 0xE0, 0, 2], 60_000).flatten());
        j.extend_from_slice(&jpeg_sof(0xC0, 8, 2, 2, 1)[2..]);
        assert_eq!(s(&j), ok(Format::Jpeg, 2, 2));
    }

    /// Hands out `data` a byte at a time, then fails every read with `kind`.
    struct Failing<'a> {
        data: &'a [u8],
        kind: io::ErrorKind,
    }
    impl Read for Failing<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.data.is_empty() {
                return Err(self.kind.into());
            }
            let n = buf.len().min(self.data.len()).min(1);
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    /// Interrupts every other read.
    struct Interrupting<'a> {
        data: &'a [u8],
        flip: bool,
    }
    impl Read for Interrupting<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.flip = !self.flip;
            if self.flip {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let n = buf.len().min(self.data.len()).min(3);
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    /// Claims to have read more than it was given room for.
    struct Lying;
    impl Read for Lying {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            Ok(buf.len() + 1)
        }
    }

    #[test]
    fn read_errors_are_damaged_and_interrupts_retry() {
        for data in [&RGB[..3], &RGB[..100], &b"\x89PNG"[..], &b""[..]] {
            let mut f = Failing {
                data,
                kind: io::ErrorKind::Other,
            };
            assert_eq!(sniff(&mut f, MAX), Err(ProbeError::Damaged));
        }
        let mut i = Interrupting {
            data: RGB,
            flip: false,
        };
        assert_eq!(sniff(&mut i, MAX), ok(Format::Jpeg, 32, 24));
        let p = png(5, 3, 0, None, false, false);
        let mut i = Interrupting {
            data: &p,
            flip: false,
        };
        assert_eq!(sniff(&mut i, MAX), ok(Format::Png, 5, 3));
        assert_eq!(sniff(&mut Lying, MAX), Err(ProbeError::Damaged));
    }

    #[test]
    fn sniff_agrees_with_probe() {
        let pal: &[u8] = &[0, 0, 0, 255, 255, 255, 9, 9, 9];
        let mut inputs: Vec<Vec<u8>> = [GRAY, RGB, PROGRESSIVE, CMYK, EXIF]
            .iter()
            .map(|b| b.to_vec())
            .collect();
        for (w, h, color, palette) in [
            (5, 3, 0, None),
            (6, 2, 2, None),
            (4, 1, 3, Some(pal)),
            (1, 1, 0, None),
        ] {
            inputs.push(png(w, h, color, palette, false, false));
        }
        for m in [0xC0, 0xC1, 0xC2] {
            for n in [1, 3, 4] {
                inputs.push(jpeg_sof(m, 8, 7, 9, n));
            }
        }
        inputs.push(after_soi(RGB, &segment(0xE1, &[1; 65_533])));
        let mut accepted = 0;
        for (i, b) in inputs.iter().enumerate() {
            let Ok(p) = probe(b, MAX) else {
                continue;
            };
            accepted += 1;
            let want = match p.image {
                Image::Jpeg(j) => (Format::Jpeg, j.width, j.height),
                Image::Png(p) => (Format::Png, p.width, p.height),
            };
            assert_eq!(s(b), ok(want.0, want.1, want.2), "input {i}");
        }
        // Every input above is one the probe accepts: none was skipped.
        assert_eq!(accepted, inputs.len());
    }
}
