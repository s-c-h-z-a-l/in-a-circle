//! Contract tests of the image probe and the PNG data check.

use flate::Crc32;
use probe::{Image, PngColor, PngInfo, ProbeError, Unsupported, check_png_data, probe};
use std::fs;
use std::path::PathBuf;

const MAX: u64 = 180_000_000;

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name);
    fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn crc(kind: &[u8], data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(kind);
    c.update(data);
    c.finish()
}

/// A PNG chunk with a correct CRC.
fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = (data.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(data);
    v.extend_from_slice(&crc(kind, data).to_be_bytes());
    v
}

/// Rewrite the 13 IHDR bytes of a PNG made by `testzip::png` (CRC fixed).
fn patch_ihdr(mut png: Vec<u8>, f: impl FnOnce(&mut [u8])) -> Vec<u8> {
    f(&mut png[16..29]);
    let c = crc(b"IHDR", &png[16..29]);
    png[29..33].copy_from_slice(&c.to_be_bytes());
    png
}

/// Insert a chunk right after IHDR (offset 33).
fn insert_after_ihdr(mut png: Vec<u8>, extra: &[u8]) -> Vec<u8> {
    png.splice(33..33, extra.iter().copied());
    png
}

fn raw_rows(w: usize, h: usize, channels: usize) -> Vec<u8> {
    let mut rows = Vec::new();
    for y in 0..h {
        rows.push(0);
        for x in 0..w * channels {
            rows.push((x * 7 + y * 13) as u8);
        }
    }
    rows
}

fn png_gray(w: u32, h: u32) -> Vec<u8> {
    testzip::png(
        w,
        h,
        0,
        None,
        false,
        false,
        &raw_rows(w as usize, h as usize, 1),
    )
}

/// A header-only JPEG: SOI, one SOF segment, an SOS header.
fn jpeg_sof(marker: u8, height: u16, width: u16, components: u8) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF, marker];
    v.extend_from_slice(&(8 + 3 * u16::from(components)).to_be_bytes());
    v.push(8);
    v.extend_from_slice(&height.to_be_bytes());
    v.extend_from_slice(&width.to_be_bytes());
    v.push(components);
    for i in 0..components {
        v.extend_from_slice(&[i + 1, 0x11, 0]);
    }
    v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
    v
}

fn jpeg_info(bytes: &[u8]) -> probe::JpegInfo {
    match probe(bytes, MAX).unwrap().image {
        Image::Jpeg(j) => j,
        Image::Png(_) => panic!("expected a JPEG"),
    }
}

fn png_info(bytes: &[u8]) -> PngInfo {
    match probe(bytes, MAX).unwrap().image {
        Image::Png(p) => p,
        Image::Jpeg(_) => panic!("expected a PNG"),
    }
}

#[test]
fn probes_baseline_and_progressive_jpeg() {
    for (name, w, h, c) in [
        ("gray.jpg", 24, 16, 1),
        ("rgb.jpg", 32, 24, 3),
        ("progressive.jpg", 40, 30, 3),
    ] {
        let j = jpeg_info(&fixture(name));
        assert_eq!((j.width, j.height, j.components), (w, h, c), "{name}");
        assert!(!j.adobe_inverted, "{name}");
    }
    // The fixture really is progressive (SOF2), not a second baseline file.
    assert!(
        fixture("progressive.jpg")
            .windows(2)
            .any(|w| w == [0xFF, 0xC2])
    );
}

#[test]
fn probes_adobe_cmyk_jpeg() {
    let j = jpeg_info(&fixture("cmyk_adobe.jpg"));
    assert_eq!((j.width, j.height), (16, 16));
    assert_eq!(j.components, 4);
    assert!(j.adobe_inverted);
}

#[test]
fn probes_png_gray_rgb_palette() {
    let g = png_gray(5, 3);
    let p = png_info(&g);
    assert_eq!((p.width, p.height, p.color), (5, 3, PngColor::Gray));
    assert!(p.palette.is_none());
    assert_eq!(p.idat.len(), 1);
    // The IDAT range is the chunk data: it starts with the zlib header.
    let r = p.idat[0].clone();
    assert_eq!(g[r.start], 0x78);
    assert_eq!(&g[r.end + 4..], &chunk(b"IEND", &[])[..]);

    let rgb = testzip::png(4, 2, 2, None, false, false, &raw_rows(4, 2, 3));
    let p = png_info(&rgb);
    assert_eq!(p.color, PngColor::Rgb);
    assert!(p.palette.is_none());

    let plte = [255, 0, 0, 0, 255, 0, 0, 0, 255];
    let pal = testzip::png(4, 2, 3, Some(&plte), false, false, &raw_rows(4, 2, 1));
    let p = png_info(&pal);
    assert_eq!(p.color, PngColor::Palette);
    assert_eq!(&pal[p.palette.clone().unwrap()], &plte);
}

#[test]
fn probe_rejects_alpha_png() {
    for color in [4, 6] {
        let png = patch_ihdr(png_gray(2, 2), |h| h[9] = color);
        assert_eq!(
            probe(&png, MAX).unwrap_err(),
            ProbeError::Unsupported(Unsupported::Alpha),
            "color type {color}"
        );
    }
}

#[test]
fn probe_rejects_trns_in_every_color_type() {
    let plte = [1, 2, 3, 4, 5, 6];
    for (color, ch) in [(0u8, 1usize), (2, 3), (3, 1)] {
        let pal = (color == 3).then_some(&plte[..]);
        let png = testzip::png(2, 2, color, pal, true, false, &raw_rows(2, 2, ch));
        assert_eq!(
            probe(&png, MAX).unwrap_err(),
            ProbeError::Unsupported(Unsupported::Transparency),
            "color type {color}"
        );
    }
}

#[test]
fn probe_rejects_interlace() {
    let png = testzip::png(2, 2, 0, None, false, true, &raw_rows(2, 2, 1));
    assert_eq!(
        probe(&png, MAX).unwrap_err(),
        ProbeError::Unsupported(Unsupported::Interlace)
    );
}

#[test]
fn probe_rejects_16_bit() {
    let png = patch_ihdr(png_gray(2, 2), |h| h[8] = 16);
    assert_eq!(
        probe(&png, MAX).unwrap_err(),
        ProbeError::Unsupported(Unsupported::BitDepth(16))
    );
    let png = patch_ihdr(png_gray(2, 2), |h| h[8] = 4);
    assert_eq!(
        probe(&png, MAX).unwrap_err(),
        ProbeError::Unsupported(Unsupported::BitDepth(4))
    );
}

#[test]
fn probe_rejects_huge_dimensions() {
    // 60000 x 60000 = 3.6e9 pixels, a header of a few bytes: no allocation.
    let jpg = jpeg_sof(0xC0, 60000, 60000, 3);
    assert_eq!(probe(&jpg, MAX).unwrap_err(), ProbeError::TooLarge);
    let png = patch_ihdr(png_gray(2, 2), |h| {
        h[0..4].copy_from_slice(&0x7FFF_FFFFu32.to_be_bytes());
        h[4..8].copy_from_slice(&0x7FFF_FFFFu32.to_be_bytes());
    });
    assert_eq!(probe(&png, MAX).unwrap_err(), ProbeError::TooLarge);
    // The limit is on the product and inclusive.
    let ok = jpeg_sof(0xC0, 100, 100, 1);
    assert!(probe(&ok, 10_000).is_ok());
    assert_eq!(probe(&ok, 9_999).unwrap_err(), ProbeError::TooLarge);
}

#[test]
fn probe_rejects_png_dimension_over_31_bits() {
    let png = patch_ihdr(png_gray(2, 2), |h| {
        h[0..4].copy_from_slice(&0x8000_0000u32.to_be_bytes());
    });
    assert_eq!(probe(&png, MAX).unwrap_err(), ProbeError::Damaged);
    let png = patch_ihdr(png_gray(2, 2), |h| {
        h[4..8].copy_from_slice(&0u32.to_be_bytes())
    });
    assert_eq!(probe(&png, MAX).unwrap_err(), ProbeError::Damaged);
}

#[test]
fn probe_rejects_truncated_chunk() {
    let png = png_gray(5, 3);
    // Cut in the middle of the IDAT, then right before IEND, then in the
    // signature-sized prefix of a chunk header.
    for cut in [png.len() - 20, png.len() - 12, png.len() - 1, 35] {
        assert_eq!(
            probe(&png[..cut], MAX).unwrap_err(),
            ProbeError::Truncated,
            "cut at {cut}"
        );
    }
    // A chunk that declares more bytes than the file holds.
    let mut lying = png.clone();
    lying[33..37].copy_from_slice(&0x7FFF_FFF0u32.to_be_bytes());
    assert_eq!(probe(&lying, MAX).unwrap_err(), ProbeError::Truncated);
    // A JPEG cut inside the SOF segment, and one that never reaches SOS.
    let jpg = jpeg_sof(0xC0, 10, 10, 3);
    assert_eq!(probe(&jpg[..12], MAX).unwrap_err(), ProbeError::Truncated);
    assert_eq!(
        probe(&jpg[..jpg.len() - 4], MAX).unwrap_err(),
        ProbeError::Truncated
    );
}

#[test]
fn probe_rejects_bad_chunk_crc() {
    let mut png = png_gray(5, 3);
    let n = png.len();
    png[n - 20] ^= 0x01; // a byte inside the IDAT
    assert_eq!(probe(&png, MAX).unwrap_err(), ProbeError::Damaged);
    let mut png = png_gray(5, 3);
    png[29] ^= 0x01; // the IHDR CRC itself
    assert_eq!(probe(&png, MAX).unwrap_err(), ProbeError::Damaged);
}

#[test]
fn probe_rejects_not_an_image() {
    assert_eq!(probe(b"", MAX).unwrap_err(), ProbeError::NotImage);
    assert_eq!(probe(b"GIF89a....", MAX).unwrap_err(), ProbeError::NotImage);
    assert_eq!(probe(b"\x89PNG", MAX).unwrap_err(), ProbeError::NotImage);
    assert_eq!(probe(&[0xFF], MAX).unwrap_err(), ProbeError::NotImage);
    assert_eq!(
        probe(&vec![0u8; 4096], MAX).unwrap_err(),
        ProbeError::NotImage
    );
}

#[test]
fn probe_rejects_malformed_png_structure() {
    let ok = png_gray(5, 3);
    // IHDR not first.
    let mut v = ok[..8].to_vec();
    v.extend_from_slice(&chunk(b"tEXt", b"k\0v"));
    v.extend_from_slice(&ok[8..]);
    assert_eq!(probe(&v, MAX).unwrap_err(), ProbeError::Damaged);
    // No IDAT at all.
    let mut v = ok[..33].to_vec();
    v.extend_from_slice(&chunk(b"IEND", &[]));
    assert_eq!(probe(&v, MAX).unwrap_err(), ProbeError::Damaged);
    // A palette image without PLTE.
    let v = patch_ihdr(ok.clone(), |h| h[9] = 3);
    assert_eq!(probe(&v, MAX).unwrap_err(), ProbeError::Damaged);
    // PLTE in a gray image, and a PLTE whose length is not a multiple of 3.
    let v = insert_after_ihdr(ok.clone(), &chunk(b"PLTE", &[1, 2, 3]));
    assert_eq!(probe(&v, MAX).unwrap_err(), ProbeError::Damaged);
    let v = patch_ihdr(ok.clone(), |h| h[9] = 3);
    let v = insert_after_ihdr(v, &chunk(b"PLTE", &[1, 2, 3, 4]));
    assert_eq!(probe(&v, MAX).unwrap_err(), ProbeError::Damaged);
    // IDAT chunks split by another chunk.
    let p = png_info(&ok);
    let r = p.idat[0].clone();
    let mid = r.start + (r.end - r.start) / 2;
    let mut v = ok[..33].to_vec();
    v.extend_from_slice(&chunk(b"IDAT", &ok[r.start..mid]));
    v.extend_from_slice(&chunk(b"tEXt", b"k\0v"));
    v.extend_from_slice(&chunk(b"IDAT", &ok[mid..r.end]));
    v.extend_from_slice(&chunk(b"IEND", &[]));
    assert_eq!(probe(&v, MAX).unwrap_err(), ProbeError::Damaged);
    // Trailing bytes after IEND are not part of the image and are ignored.
    let mut v = ok.clone();
    v.extend_from_slice(b"trailing");
    assert!(probe(&v, MAX).is_ok());
}

#[test]
fn probe_rejects_unsupported_jpeg_kinds() {
    for m in [0xC3, 0xC5, 0xC6, 0xC7] {
        assert_eq!(
            probe(&jpeg_sof(m, 8, 8, 3), MAX).unwrap_err(),
            ProbeError::Unsupported(Unsupported::Lossless),
            "SOF {m:#x}"
        );
    }
    for m in [0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF] {
        assert_eq!(
            probe(&jpeg_sof(m, 8, 8, 3), MAX).unwrap_err(),
            ProbeError::Unsupported(Unsupported::Arithmetic),
            "SOF {m:#x}"
        );
    }
    for n in [2, 5] {
        assert_eq!(
            probe(&jpeg_sof(0xC0, 8, 8, n), MAX).unwrap_err(),
            ProbeError::Unsupported(Unsupported::JpegComponents(n))
        );
    }
    for m in [0xC0, 0xC1, 0xC2] {
        assert!(probe(&jpeg_sof(m, 8, 8, 3), MAX).is_ok(), "SOF {m:#x}");
    }
}

#[test]
fn probe_rejects_malformed_jpeg() {
    // SOS before any SOF.
    assert_eq!(
        probe(&[0xFF, 0xD8, 0xFF, 0xDA, 0, 2], MAX).unwrap_err(),
        ProbeError::Damaged
    );
    // Zero width, and zero height (DNL is not supported).
    assert_eq!(
        probe(&jpeg_sof(0xC0, 8, 0, 3), MAX).unwrap_err(),
        ProbeError::Damaged
    );
    assert_eq!(
        probe(&jpeg_sof(0xC0, 0, 8, 3), MAX).unwrap_err(),
        ProbeError::Damaged
    );
    // A segment length below its own two bytes.
    assert_eq!(
        probe(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 1, 0xFF, 0xDA], MAX).unwrap_err(),
        ProbeError::Damaged
    );
    // SOF length that disagrees with the component count.
    let mut j = jpeg_sof(0xC0, 8, 8, 3);
    j[4..6].copy_from_slice(&11u16.to_be_bytes());
    assert_eq!(probe(&j, MAX).unwrap_err(), ProbeError::Damaged);
    // 12-bit precision.
    let mut j = jpeg_sof(0xC1, 8, 8, 3);
    j[6] = 12;
    assert_eq!(
        probe(&j, MAX).unwrap_err(),
        ProbeError::Unsupported(Unsupported::BitDepth(12))
    );
    // EOI before SOS.
    assert_eq!(
        probe(&[0xFF, 0xD8, 0xFF, 0xD9], MAX).unwrap_err(),
        ProbeError::Damaged
    );
}

#[test]
fn notes_exif_orientation_and_icc() {
    let p = probe(&fixture("exif_orientation6.jpg"), MAX).unwrap();
    assert!(p.orientation_ignored);
    assert!(!p.profile_ignored);
    let p = probe(&fixture("rgb.jpg"), MAX).unwrap();
    assert!(!p.orientation_ignored);

    // Orientation 1 (normal) is not a rotation to ignore.
    let mut j = fixture("exif_orientation6.jpg");
    let pos = j.windows(4).position(|w| w == b"Exif").unwrap();
    // IFD0 entry search: the tag 0x0112 value sits 8 bytes into its entry.
    let tag = j[pos..]
        .windows(2)
        .position(|w| w == [0x01, 0x12] || w == [0x12, 0x01])
        .unwrap()
        + pos;
    let val = tag + 8;
    assert!(j[val] == 6 || j[val + 1] == 6);
    j[val] = if j[val] == 6 { 1 } else { 0 };
    j[val + 1] = if j[val + 1] == 6 { 1 } else { 0 };
    assert!(!probe(&j, MAX).unwrap().orientation_ignored);

    // An ICC profile in a JPEG (APP2) and in a PNG (iCCP).
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE2];
    let body = b"ICC_PROFILE\0\x01\x01profile";
    v.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    v.extend_from_slice(body);
    v.extend_from_slice(&jpeg_sof(0xC0, 8, 8, 3)[2..]);
    assert!(probe(&v, MAX).unwrap().profile_ignored);

    let png = insert_after_ihdr(png_gray(5, 3), &chunk(b"iCCP", b"icc\0\0data"));
    let p = probe(&png, MAX).unwrap();
    assert!(p.profile_ignored);
    assert!(!p.orientation_ignored);
    assert!(!probe(&png_gray(5, 3), MAX).unwrap().profile_ignored);
}

#[test]
fn malformed_exif_is_ignored_not_fatal() {
    // An APP1 "Exif" with a lying IFD offset: metadata only, never an error.
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE1];
    let body = b"Exif\0\0II*\0\xFF\xFF\xFF\x7F\x05";
    v.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    v.extend_from_slice(body);
    v.extend_from_slice(&jpeg_sof(0xC0, 8, 8, 3)[2..]);
    let p = probe(&v, MAX).unwrap();
    assert!(!p.orientation_ignored);
}

fn info_of(png: &[u8]) -> PngInfo {
    png_info(png)
}

#[test]
fn png_data_check_accepts_exact_size() {
    for (color, ch, plte) in [
        (0u8, 1usize, None),
        (2, 3, None),
        (3, 1, Some(&[1u8, 2, 3][..])),
    ] {
        let png = testzip::png(7, 5, color, plte, false, false, &raw_rows(7, 5, ch));
        check_png_data(&png, &info_of(&png)).unwrap();
    }
    // The stream split over several IDAT chunks (mid-symbol boundaries).
    let one = png_gray(40, 40);
    let r = png_info(&one).idat[0].clone();
    let data = &one[r.clone()];
    let mut v = one[..33].to_vec();
    for part in data.chunks(7) {
        v.extend_from_slice(&chunk(b"IDAT", part));
    }
    v.extend_from_slice(&chunk(b"IEND", &[]));
    let info = info_of(&v);
    assert!(info.idat.len() > 3);
    check_png_data(&v, &info).unwrap();
}

#[test]
fn png_data_check_rejects_short_idat() {
    // One row missing.
    let mut rows = raw_rows(6, 4, 1);
    rows.truncate(rows.len() - 7);
    let png = testzip::png(6, 4, 0, None, false, false, &rows);
    assert_eq!(
        check_png_data(&png, &info_of(&png)).unwrap_err(),
        ProbeError::Damaged
    );
    // A stream cut off mid-way is damaged as well.
    let ok = png_gray(30, 30);
    let mut info = info_of(&ok);
    info.idat[0].end -= 10;
    assert_eq!(check_png_data(&ok, &info).unwrap_err(), ProbeError::Damaged);
}

#[test]
fn png_data_check_rejects_extra_data() {
    let mut rows = raw_rows(6, 4, 1);
    rows.extend_from_slice(&[0, 1, 2, 3, 4, 5, 6]);
    let png = testzip::png(6, 4, 0, None, false, false, &rows);
    assert_eq!(
        check_png_data(&png, &info_of(&png)).unwrap_err(),
        ProbeError::Damaged
    );
    // One byte more than a whole row, too.
    let mut rows = raw_rows(6, 4, 1);
    rows.push(0);
    let png = testzip::png(6, 4, 0, None, false, false, &rows);
    assert_eq!(
        check_png_data(&png, &info_of(&png)).unwrap_err(),
        ProbeError::Damaged
    );
}

#[test]
fn png_data_check_rejects_bad_filter_byte_and_trailing_idat_bytes() {
    let mut rows = raw_rows(6, 4, 1);
    rows[14] = 5; // the third row's filter byte
    let png = testzip::png(6, 4, 0, None, false, false, &rows);
    assert_eq!(
        check_png_data(&png, &info_of(&png)).unwrap_err(),
        ProbeError::Damaged
    );
    // Junk after the zlib stream, inside the IDAT chunk.
    let ok = png_gray(6, 4);
    let r = png_info(&ok).idat[0].clone();
    let mut data = ok[r].to_vec();
    data.extend_from_slice(b"junk");
    let mut v = ok[..33].to_vec();
    v.extend_from_slice(&chunk(b"IDAT", &data));
    v.extend_from_slice(&chunk(b"IEND", &[]));
    assert_eq!(
        check_png_data(&v, &info_of(&v)).unwrap_err(),
        ProbeError::Damaged
    );
}

#[test]
fn png_data_check_is_safe_on_lying_info() {
    let ok = png_gray(6, 4);
    // A range past the buffer.
    let mut info = info_of(&ok);
    info.idat[0].end = ok.len() + 100;
    assert_eq!(check_png_data(&ok, &info).unwrap_err(), ProbeError::Damaged);
    // Dimensions whose byte count overflows u64 are refused without work.
    let mut info = info_of(&ok);
    info.width = u32::MAX;
    info.height = u32::MAX;
    info.color = PngColor::Rgb;
    assert_eq!(
        check_png_data(&ok, &info).unwrap_err(),
        ProbeError::TooLarge
    );
}

#[test]
fn unsupported_text_is_the_planned_copy() {
    use probe::unsupported_text as t;
    assert_eq!(
        t(&Unsupported::Alpha),
        "PNG with alpha needs the image family"
    );
    assert_eq!(
        t(&Unsupported::Transparency),
        "PNG transparency needs the image family"
    );
    assert_eq!(
        t(&Unsupported::Interlace),
        "interlaced PNG needs the image family"
    );
    assert_eq!(
        t(&Unsupported::BitDepth(16)),
        "16-bit PNG needs the image family"
    );
    assert_eq!(
        t(&Unsupported::Arithmetic),
        "arithmetic-coded JPEG is not supported"
    );
    assert_eq!(t(&Unsupported::Lossless), "lossless JPEG is not supported");
}

/// A JPEG with an Exif APP1 whose IFD0 holds the given `(tag, value)`
/// SHORT entries, in the given byte order, followed by a SOF segment.
fn jpeg_with_exif(little: bool, entries: &[(u16, u16)]) -> Vec<u8> {
    let w16 = |v: u16| {
        if little {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    };
    let w32 = |v: u32| {
        if little {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    };
    let mut body = b"Exif\0\0".to_vec();
    body.extend_from_slice(if little { b"II*\0" } else { b"MM\0*" });
    body.extend_from_slice(&w32(8));
    body.extend_from_slice(&w16(entries.len() as u16));
    for &(tag, value) in entries {
        body.extend_from_slice(&w16(tag));
        body.extend_from_slice(&w16(3)); // SHORT
        body.extend_from_slice(&w32(1));
        body.extend_from_slice(&w16(value));
        body.extend_from_slice(&[0, 0]);
    }
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE1];
    v.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    v.extend_from_slice(&body);
    v.extend_from_slice(&jpeg_sof(0xC0, 8, 8, 3)[2..]);
    v
}

#[test]
fn exif_orientation_in_either_byte_order_and_any_entry() {
    for little in [false, true] {
        let first = jpeg_with_exif(little, &[(0x0112, 6)]);
        assert!(probe(&first, MAX).unwrap().orientation_ignored, "{little}");
        // The orientation is not the first entry.
        let third = jpeg_with_exif(little, &[(0x010F, 1), (0x011A, 72), (0x0112, 8)]);
        assert!(probe(&third, MAX).unwrap().orientation_ignored, "{little}");
        // Orientation 1 is normal, and no orientation at all is normal.
        let normal = jpeg_with_exif(little, &[(0x010F, 1), (0x0112, 1)]);
        assert!(!probe(&normal, MAX).unwrap().orientation_ignored);
        let none = jpeg_with_exif(little, &[(0x010F, 1), (0x011A, 72)]);
        assert!(!probe(&none, MAX).unwrap().orientation_ignored);
    }
}

#[test]
fn jpeg_stuffed_zero_outside_entropy_data_is_damaged() {
    // FF 00 is not a marker; read as one with a 2-byte "segment" it would
    // be skipped and the file would probe fine.
    let mut v = vec![0xFF, 0xD8, 0xFF, 0x00, 0x00, 0x02];
    v.extend_from_slice(&jpeg_sof(0xC0, 8, 8, 3)[2..]);
    assert_eq!(probe(&v, MAX).unwrap_err(), ProbeError::Damaged);
}

#[test]
fn jpeg_sof_segment_sizes_around_the_fixed_part() {
    // A SOF segment of exactly the 6 fixed bytes names zero components.
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xC0, 0, 8, 8, 0, 8, 0, 8, 0];
    v.extend_from_slice(&[0xFF, 0xDA, 0, 2]);
    assert_eq!(
        probe(&v, MAX).unwrap_err(),
        ProbeError::Unsupported(Unsupported::JpegComponents(0))
    );
    // Shorter than that is damaged, never a read past the segment.
    for body in 0..6u16 {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xC0];
        v.extend_from_slice(&(body + 2).to_be_bytes());
        v.extend(std::iter::repeat_n(8u8, usize::from(body)));
        v.extend_from_slice(&[0xFF, 0xDA, 0, 2]);
        assert_eq!(probe(&v, MAX).unwrap_err(), ProbeError::Damaged, "{body}");
    }
}

/// A PNG chunk header (length, type) with nothing behind it.
fn bare_chunk_header(len: u32) -> Vec<u8> {
    let mut v = png_gray(5, 3)[..33].to_vec();
    v.extend_from_slice(&len.to_be_bytes());
    v.extend_from_slice(b"IDAT");
    v
}

#[test]
fn png_chunk_length_limit_is_2_pow_31_minus_1() {
    // The largest legal length is cut short by the file; one more is damaged.
    assert_eq!(
        probe(&bare_chunk_header(0x7FFF_FFFF), MAX).unwrap_err(),
        ProbeError::Truncated
    );
    for len in [0x8000_0000, 0xFFFF_FFFF] {
        assert_eq!(
            probe(&bare_chunk_header(len), MAX).unwrap_err(),
            ProbeError::Damaged,
            "{len:#x}"
        );
    }
}

#[test]
fn png_ihdr_must_be_unique_and_13_bytes() {
    let ok = png_gray(5, 3);
    // A second IHDR.
    let v = insert_after_ihdr(ok.clone(), &ok[8..33]);
    assert_eq!(probe(&v, MAX).unwrap_err(), ProbeError::Damaged);
    // An IHDR of 14 bytes (CRC fixed) is the wrong size even when the first
    // 13 bytes are fine.
    let mut data = ok[16..29].to_vec();
    data.push(0);
    let mut v = ok[..8].to_vec();
    v.extend_from_slice(&chunk(b"IHDR", &data));
    v.extend_from_slice(&ok[33..]);
    assert_eq!(probe(&v, MAX).unwrap_err(), ProbeError::Damaged);
}

#[test]
fn png_palette_size_and_count_rules() {
    let palette = |entries: usize| {
        let plte = vec![9u8; 3 * entries];
        testzip::png(4, 2, 3, Some(&plte), false, false, &raw_rows(4, 2, 1))
    };
    // 1 to 256 entries are fine; 257 entries do not fit 8-bit indices.
    for n in [1, 255, 256] {
        assert!(probe(&palette(n), MAX).is_ok(), "{n} entries");
    }
    assert_eq!(probe(&palette(257), MAX).unwrap_err(), ProbeError::Damaged);
    assert_eq!(probe(&palette(0), MAX).unwrap_err(), ProbeError::Damaged);
    // A second PLTE before the image data.
    let base = testzip::png(4, 2, 3, Some(&[1, 2, 3]), false, false, &raw_rows(4, 2, 1));
    let v = insert_after_ihdr(base, &chunk(b"PLTE", &[4, 5, 6]));
    assert_eq!(probe(&v, MAX).unwrap_err(), ProbeError::Damaged);
}

#[test]
fn png_ihdr_method_fields_are_each_checked() {
    for field in [10usize, 11, 12] {
        let v = patch_ihdr(png_gray(5, 3), |h| h[field] = 2);
        assert_eq!(probe(&v, MAX).unwrap_err(), ProbeError::Damaged, "{field}");
    }
}

#[test]
fn png_pixel_limit_is_inclusive() {
    let png = png_gray(5, 3);
    assert!(probe(&png, 15).is_ok());
    assert_eq!(probe(&png, 14).unwrap_err(), ProbeError::TooLarge);
}

#[test]
fn png_filter_byte_is_checked_at_the_start_of_every_row() {
    // 101-byte rows: the inflate output chunks end in the middle of a row,
    // and the next chunk starts with the rest of it. The bad filter byte
    // is the first byte of a row that starts in the second chunk.
    let (w, h) = (100usize, 400usize);
    let mut rows = raw_rows(w, h, 1);
    let bad_row = 32_768 / 101 + 1;
    rows[bad_row * 101] = 9;
    let png = testzip::png(w as u32, h as u32, 0, None, false, false, &rows);
    assert_eq!(
        check_png_data(&png, &info_of(&png)).unwrap_err(),
        ProbeError::Damaged
    );
    // The same image with valid filter bytes passes.
    let png = testzip::png(
        w as u32,
        h as u32,
        0,
        None,
        false,
        false,
        &raw_rows(w, h, 1),
    );
    check_png_data(&png, &info_of(&png)).unwrap();
}

/// A 1x1 gray PNG whose IDAT is split into `n` chunks: the first holds the
/// whole zlib stream, the others are empty.
fn png_with_idat_chunks(n: usize) -> Vec<u8> {
    let ok = png_gray(1, 1);
    let r = png_info(&ok).idat[0].clone();
    let mut v = ok[..33].to_vec();
    v.extend_from_slice(&chunk(b"IDAT", &ok[r.start..r.end]));
    let empty = chunk(b"IDAT", &[]);
    for _ in 1..n {
        v.extend_from_slice(&empty);
    }
    v.extend_from_slice(&chunk(b"IEND", &[]));
    v
}

#[test]
fn png_idat_chunk_count_limit() {
    let limit = probe::MAX_IDAT_CHUNKS;
    let at = probe(&png_with_idat_chunks(limit), MAX).unwrap();
    assert!(matches!(at.image, Image::Png(ref p) if p.idat.len() == limit));
    assert_eq!(
        probe(&png_with_idat_chunks(limit + 1), MAX).unwrap_err(),
        ProbeError::TooManyChunks
    );
}
