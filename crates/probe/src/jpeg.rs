//! JPEG marker walk, SOI to SOS.

use crate::{Image, JpegInfo, ProbeError, Probed, Unsupported};

pub(crate) fn probe(bytes: &[u8], max_pixels: u64) -> Result<Probed, ProbeError> {
    let mut pos = 2; // after SOI
    let mut sof: Option<(u32, u32, u8)> = None;
    let mut adobe = false;
    let mut orientation_ignored = false;
    let mut profile_ignored = false;
    loop {
        // Marker: 0xFF, optional 0xFF fill bytes, then the code.
        if *bytes.get(pos).ok_or(ProbeError::Truncated)? != 0xFF {
            return Err(ProbeError::Damaged);
        }
        let mut code = 0xFF;
        while code == 0xFF {
            pos += 1;
            code = *bytes.get(pos).ok_or(ProbeError::Truncated)?;
        }
        pos += 1;
        match code {
            // Stuffed zero, TEM, RSTn: no length. A stuffed zero outside
            // entropy data is not a marker.
            0x00 => return Err(ProbeError::Damaged),
            0x01 | 0xD0..=0xD7 => continue,
            // SOI again, or EOI before any scan.
            0xD8 | 0xD9 => return Err(ProbeError::Damaged),
            _ => {}
        }
        let len_end = pos.checked_add(2).ok_or(ProbeError::Truncated)?;
        let len_bytes = bytes.get(pos..len_end).ok_or(ProbeError::Truncated)?;
        let len = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
        if len < 2 {
            return Err(ProbeError::Damaged);
        }
        let end = pos.checked_add(len).ok_or(ProbeError::Truncated)?;
        let seg = bytes.get(len_end..end).ok_or(ProbeError::Truncated)?;
        match code {
            // SOF0 baseline, SOF1 extended sequential, SOF2 progressive.
            0xC0..=0xC2 => {
                if sof.is_some() {
                    return Err(ProbeError::Damaged);
                }
                sof = Some(parse_sof(seg)?);
            }
            // Lossless and the differential (hierarchical) Huffman modes.
            0xC3 | 0xC5..=0xC7 => return Err(ProbeError::Unsupported(Unsupported::Lossless)),
            // Arithmetic-coded variants.
            0xC9..=0xCB | 0xCD..=0xCF => {
                return Err(ProbeError::Unsupported(Unsupported::Arithmetic));
            }
            // APP1: Exif.
            0xE1 => {
                if exif_orientation_not_normal(seg) {
                    orientation_ignored = true;
                }
            }
            // APP2: ICC profile chunk.
            0xE2 => {
                if seg.starts_with(b"ICC_PROFILE\0") {
                    profile_ignored = true;
                }
            }
            // APP14: Adobe. Present on every Photoshop-style CMYK/YCCK file.
            0xEE => {
                if seg.starts_with(b"Adobe") {
                    adobe = true;
                }
            }
            // SOS: the headers are done; entropy data is not walked.
            0xDA => break,
            _ => {}
        }
        pos = end;
    }
    let (width, height, components) = sof.ok_or(ProbeError::Damaged)?;
    if u64::from(width) * u64::from(height) > max_pixels {
        return Err(ProbeError::TooLarge);
    }
    Ok(Probed {
        image: Image::Jpeg(JpegInfo {
            width,
            height,
            components,
            adobe_inverted: adobe && components == 4,
        }),
        orientation_ignored,
        profile_ignored,
    })
}

/// SOF segment body: precision, height, width, component count, then 3
/// bytes per component. Returns (width, height, components).
fn parse_sof(seg: &[u8]) -> Result<(u32, u32, u8), ProbeError> {
    if seg.len() < 6 {
        return Err(ProbeError::Damaged);
    }
    let precision = seg[0];
    let height = u32::from(u16::from_be_bytes([seg[1], seg[2]]));
    let width = u32::from(u16::from_be_bytes([seg[3], seg[4]]));
    let n = seg[5];
    if seg.len() != 6 + 3 * usize::from(n) {
        return Err(ProbeError::Damaged);
    }
    // Height 0 means "defined by a DNL marker after the first scan"; no
    // PDF reader needs that and we do not walk the scan, so it is damaged.
    if width == 0 || height == 0 {
        return Err(ProbeError::Damaged);
    }
    if precision != 8 {
        return Err(ProbeError::Unsupported(Unsupported::BitDepth(precision)));
    }
    if !matches!(n, 1 | 3 | 4) {
        return Err(ProbeError::Unsupported(Unsupported::JpegComponents(n)));
    }
    Ok((width, height, n))
}

/// True if the APP1 segment is Exif whose IFD0 has Orientation (0x0112)
/// with a value other than 1. Anything malformed counts as "no": Exif is
/// metadata, never a reason to reject the image.
fn exif_orientation_not_normal(seg: &[u8]) -> bool {
    let Some(tiff) = seg.strip_prefix(b"Exif\0\0") else {
        return false;
    };
    let big = match tiff.get(..4) {
        Some(b"MM\0*") => true,
        Some(b"II*\0") => false,
        _ => return false,
    };
    let u16_at = |o: usize| -> Option<u16> {
        let b = tiff.get(o..o.checked_add(2)?)?;
        Some(if big {
            u16::from_be_bytes([b[0], b[1]])
        } else {
            u16::from_le_bytes([b[0], b[1]])
        })
    };
    let Some(ob) = tiff.get(4..8) else {
        return false;
    };
    let ifd = if big {
        u32::from_be_bytes([ob[0], ob[1], ob[2], ob[3]])
    } else {
        u32::from_le_bytes([ob[0], ob[1], ob[2], ob[3]])
    };
    let Ok(ifd) = usize::try_from(ifd) else {
        return false;
    };
    let Some(count) = u16_at(ifd) else {
        return false;
    };
    // Each entry is 12 bytes; the count is 16 bits, so this walk is at most
    // 65535 steps and every access is bounds-checked.
    for i in 0..usize::from(count) {
        let Some(e) = ifd.checked_add(2 + 12 * i) else {
            return false;
        };
        if u16_at(e) == Some(0x0112) {
            // Type SHORT, count 1: the value is the first two bytes of the
            // value field.
            return matches!(u16_at(e + 8), Some(v) if v != 1);
        }
    }
    false
}
