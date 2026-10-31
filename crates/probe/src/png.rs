//! PNG chunk walk and IDAT data check.

use crate::{Image, PngColor, PngInfo, ProbeError, Probed, Unsupported};
use flate::{Crc32, zlib_inflate};
use std::io::{self, Read};
use std::ops::Range;

pub(crate) const SIGNATURE: [u8; 8] = *b"\x89PNG\r\n\x1a\n";

/// PNG chunk lengths and image dimensions are at most 2^31 - 1 (PNG spec,
/// section 5.2).
pub(crate) const MAX_31: u32 = 0x7FFF_FFFF;

/// At most this many IDAT chunks are listed. The list costs 16 bytes per
/// chunk while each chunk costs the file at least 12, so an unbounded list
/// could be larger than the file that produced it. Encoders write 8-64 KiB
/// IDATs (libpng default 8 KiB): 65536 chunks cover 512 MiB at that size,
/// far above any page we accept, and cap the list at 1 MiB. Past it the
/// file is `TooManyChunks`.
pub const MAX_IDAT_CHUNKS: usize = 65_536;

pub(crate) fn probe(bytes: &[u8], max_pixels: u64) -> Result<Probed, ProbeError> {
    let mut pos = SIGNATURE.len();
    let mut header: Option<(u32, u32, PngColor)> = None;
    let mut palette: Option<Range<usize>> = None;
    let mut idat: Vec<Range<usize>> = Vec::new();
    let mut idat_done = false;
    let mut profile_ignored = false;
    let mut first = true;
    loop {
        let head = bytes
            .get(pos..pos.checked_add(8).ok_or(ProbeError::Truncated)?)
            .ok_or(ProbeError::Truncated)?;
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        let kind: [u8; 4] = [head[4], head[5], head[6], head[7]];
        if len > MAX_31 {
            return Err(ProbeError::Damaged);
        }
        // Checked in usize: a lying length on a 32-bit target must not wrap.
        let data_start = pos + 8;
        let data_end = data_start
            .checked_add(len as usize)
            .ok_or(ProbeError::Truncated)?;
        let chunk_end = data_end.checked_add(4).ok_or(ProbeError::Truncated)?;
        let tail = bytes
            .get(data_start..chunk_end)
            .ok_or(ProbeError::Truncated)?;
        let (data, crc) = tail.split_at(len as usize);
        let mut c = Crc32::new();
        c.update(&kind);
        c.update(data);
        if c.finish().to_be_bytes() != crc {
            return Err(ProbeError::Damaged);
        }
        if first && &kind != b"IHDR" {
            return Err(ProbeError::Damaged);
        }
        first = false;
        match &kind {
            b"IHDR" => {
                if header.is_some() || data.len() != 13 {
                    return Err(ProbeError::Damaged);
                }
                header = Some(parse_ihdr(data, max_pixels)?);
            }
            b"PLTE" => {
                let (_, _, color) = header.ok_or(ProbeError::Damaged)?;
                // Only palette images need it; a gray image must not have
                // one; an RGB image may carry a suggested palette (ignored).
                if palette.is_some() || !idat.is_empty() || color == PngColor::Gray {
                    return Err(ProbeError::Damaged);
                }
                if data.is_empty() || data.len() % 3 != 0 || data.len() > 3 * 256 {
                    return Err(ProbeError::Damaged);
                }
                palette = Some(data_start..data_end);
            }
            b"tRNS" => return Err(ProbeError::Unsupported(Unsupported::Transparency)),
            b"iCCP" => profile_ignored = true,
            b"IDAT" => {
                if idat_done {
                    return Err(ProbeError::Damaged);
                }
                if idat.len() >= MAX_IDAT_CHUNKS {
                    return Err(ProbeError::TooManyChunks);
                }
                idat.push(data_start..data_end);
            }
            b"IEND" => {
                if !data.is_empty() || idat.is_empty() {
                    return Err(ProbeError::Damaged);
                }
                let (width, height, color) = header.ok_or(ProbeError::Damaged)?;
                if color == PngColor::Palette && palette.is_none() {
                    return Err(ProbeError::Damaged);
                }
                return Ok(Probed {
                    image: Image::Png(PngInfo {
                        width,
                        height,
                        color,
                        palette: if color == PngColor::Palette {
                            palette
                        } else {
                            None
                        },
                        idat,
                    }),
                    orientation_ignored: false,
                    profile_ignored,
                });
            }
            _ => {}
        }
        if &kind != b"IDAT" && !idat.is_empty() {
            idat_done = true;
        }
        if &kind == b"IDAT"
            && palette.is_none()
            && matches!(header, Some((_, _, PngColor::Palette)))
        {
            return Err(ProbeError::Damaged);
        }
        pos = chunk_end;
    }
}

fn parse_ihdr(d: &[u8], max_pixels: u64) -> Result<(u32, u32, PngColor), ProbeError> {
    let width = u32::from_be_bytes([d[0], d[1], d[2], d[3]]);
    let height = u32::from_be_bytes([d[4], d[5], d[6], d[7]]);
    let (depth, color_type, compression, filter, interlace) = (d[8], d[9], d[10], d[11], d[12]);
    if width == 0 || height == 0 || width > MAX_31 || height > MAX_31 {
        return Err(ProbeError::Damaged);
    }
    if compression != 0 || filter != 0 || interlace > 1 {
        return Err(ProbeError::Damaged);
    }
    let color = match color_type {
        0 => PngColor::Gray,
        2 => PngColor::Rgb,
        3 => PngColor::Palette,
        4 | 6 => return Err(ProbeError::Unsupported(Unsupported::Alpha)),
        _ => return Err(ProbeError::Damaged),
    };
    if !matches!(depth, 1 | 2 | 4 | 8 | 16) {
        return Err(ProbeError::Damaged);
    }
    if depth != 8 {
        return Err(ProbeError::Unsupported(Unsupported::BitDepth(depth)));
    }
    if interlace == 1 {
        return Err(ProbeError::Unsupported(Unsupported::Interlace));
    }
    if u64::from(width) * u64::from(height) > max_pixels {
        return Err(ProbeError::TooLarge);
    }
    Ok((width, height, color))
}

/// The IDAT chunks as one `Read`.
struct Chain<'a> {
    bytes: &'a [u8],
    ranges: &'a [Range<usize>],
    at: usize,
    cur: &'a [u8],
}

impl Read for Chain<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.cur.is_empty() {
            let Some(r) = self.ranges.get(self.at) else {
                return Ok(0);
            };
            self.at += 1;
            // Ranges come from `probe`, but the caller owns `PngInfo`: a
            // range outside the buffer is an error, not a panic.
            self.cur = self
                .bytes
                .get(r.clone())
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        }
        let n = buf.len().min(self.cur.len());
        buf[..n].copy_from_slice(&self.cur[..n]);
        self.cur = &self.cur[n..];
        Ok(n)
    }
}

/// Inflate the IDAT stream into a counter and require exactly
/// `height * (1 + width * channels)` bytes, every row starting with a valid
/// filter byte (0-4), the zlib stream (Adler-32 included) ending exactly at
/// the end of the last IDAT. Nothing is stored; memory is the inflate
/// window. The inflate limit is the expected size, so an IDAT bomb stops
/// after that many bytes.
pub fn check_png_data(bytes: &[u8], info: &PngInfo) -> Result<(), ProbeError> {
    let channels: u64 = match info.color {
        PngColor::Gray | PngColor::Palette => 1,
        PngColor::Rgb => 3,
    };
    let row = 1 + u64::from(info.width) * channels; // cannot overflow u64
    let expected = row
        .checked_mul(u64::from(info.height))
        .ok_or(ProbeError::TooLarge)?;
    let mut total_in = 0u64;
    for r in &info.idat {
        total_in += bytes.get(r.clone()).ok_or(ProbeError::Damaged)?.len() as u64;
    }
    let reader = Chain {
        bytes,
        ranges: &info.idat,
        at: 0,
        cur: &[],
    };
    let mut col = 0u64; // position inside the current row
    let done = zlib_inflate(reader, expected, &mut |chunk| {
        let mut i = 0;
        while i < chunk.len() {
            if col == 0 && chunk[i] > 4 {
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
            let take = (row - col).min((chunk.len() - i) as u64);
            col = (col + take) % row;
            i += take as usize;
        }
        Ok(())
    })
    // Every inflate failure (bad stream, output past the expected size,
    // short data, our own filter-byte error) means damaged image data.
    .map_err(|_| ProbeError::Damaged)?;
    if done.out_bytes != expected || done.in_bytes != total_in {
        return Err(ProbeError::Damaged);
    }
    Ok(())
}
