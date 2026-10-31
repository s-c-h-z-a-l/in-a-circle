//! Test-only generator of ZIP archives and PNG images. Not shipped: it
//! compresses with `miniz_oxide` and computes its CRC-32 with its own bitwise
//! loop, so the readers under test are checked against independent code.

use miniz_oxide::deflate::{compress_to_vec, compress_to_vec_zlib};

/// Compression method of a generated ZIP entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    Stored,
    Deflated,
}

/// CRC-32 (IEEE, reflected 0xEDB88320), bit by bit: slow, obviously correct.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// A ZIP archive of `(name, data, method)` entries: local header, data (no
/// data descriptor, no extra field), then the central directory and an
/// end-of-central-directory record without a comment. Sizes and CRCs are
/// correct. Names may repeat (for tests of the reader's rejection).
pub fn zip(entries: &[(&[u8], &[u8], Store)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, method) in entries {
        let (code, body) = match method {
            Store::Stored => (0u16, data.to_vec()),
            Store::Deflated => (8u16, compress_to_vec(data, 6)),
        };
        let crc = crc32(data);
        let offset = out.len() as u32;
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&code.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0x21u16.to_le_bytes()); // date 1980-01-01
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra length
        out.extend_from_slice(name);
        out.extend_from_slice(&body);

        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&code.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0x21u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(body.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra length
        central.extend_from_slice(&0u16.to_le_bytes()); // comment length
        central.extend_from_slice(&0u16.to_le_bytes()); // disk number
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        central.extend_from_slice(&0u32.to_le_bytes()); // external attributes
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&0u16.to_le_bytes()); // this disk
    out.extend_from_slice(&0u16.to_le_bytes()); // central directory disk
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment length
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

/// An 8-bit PNG. `color` is the PNG color type (0, 2, 3, ...), `palette` the
/// PLTE bytes (written when `Some`), `trns` adds a tRNS chunk, `interlace`
/// sets the Adam7 flag in the header (the data is not reordered), `rows` is
/// the raw scanline data including each row's filter byte, compressed as one
/// zlib stream in a single IDAT. Chunk CRCs are correct.
pub fn png(
    width: u32,
    height: u32,
    color: u8,
    palette: Option<&[u8]>,
    trns: bool,
    interlace: bool,
    rows: &[u8],
) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, color, 0, 0, u8::from(interlace)]);
    chunk(&mut out, b"IHDR", &ihdr);
    if let Some(p) = palette {
        chunk(&mut out, b"PLTE", p);
    }
    if trns {
        let t: &[u8] = match color {
            0 => &[0, 0],
            2 => &[0, 0, 0, 0, 0, 0],
            _ => &[0],
        };
        chunk(&mut out, b"tRNS", t);
    }
    chunk(&mut out, b"IDAT", &compress_to_vec_zlib(rows, 6));
    chunk(&mut out, b"IEND", &[]);
    out
}
