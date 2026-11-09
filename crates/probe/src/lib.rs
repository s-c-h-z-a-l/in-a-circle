//! Header probe for JPEG and PNG files that come from outside (CBZ pages).
//!
//! The probe walks markers or chunks and never decodes pixels, so its cost
//! is linear in the file and its memory is independent of the declared
//! image size. `width * height` is compared with the caller's limit before
//! anything is sized from it. [`check_png_data`] additionally inflates the
//! PNG's IDAT stream into a counter (nothing is stored) to prove it holds
//! exactly the scanlines the header promises.

mod jpeg;
mod png;
pub mod sniff;

use std::ops::Range;

pub use png::{MAX_IDAT_CHUNKS, check_png_data};
pub use sniff::{Format, Sniffed, sniff};

/// PNG color types the PDF writer can pass through (8 bit, no alpha).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PngColor {
    Gray,
    Rgb,
    Palette,
}

/// What a JPEG header says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JpegInfo {
    pub width: u32,
    pub height: u32,
    /// 1, 3 or 4.
    pub components: u8,
    /// A 4-component file with an Adobe APP14 marker: its CMYK values are
    /// stored inverted, so the PDF image needs `/Decode [1 0 1 0 1 0 1 0]`.
    pub adobe_inverted: bool,
}

/// What a PNG's chunks say. Ranges index the probed byte slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PngInfo {
    pub width: u32,
    pub height: u32,
    pub color: PngColor,
    /// PLTE data, only for [`PngColor::Palette`].
    pub palette: Option<Range<usize>>,
    /// IDAT chunk data, in file order (consecutive chunks).
    pub idat: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    Jpeg(JpegInfo),
    Png(PngInfo),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probed {
    pub image: Image,
    /// An Exif Orientation other than 1 is present; the PDF page is not
    /// rotated, so the caller reports it.
    pub orientation_ignored: bool,
    /// An embedded ICC profile (PNG iCCP, JPEG APP2) is present; the PDF
    /// uses the device color space, so the caller reports it.
    pub profile_ignored: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeError {
    /// Neither a JPEG nor a PNG signature.
    NotImage,
    /// The file ends inside a marker, a chunk or before the end marker.
    Truncated,
    /// Structurally wrong: bad CRC, missing or misplaced chunk, bad size.
    Damaged,
    /// `width * height` is over the limit (or a size computation overflows).
    TooLarge,
    /// A PNG with more than [`MAX_IDAT_CHUNKS`] data chunks.
    TooManyChunks,
    Unsupported(Unsupported),
}

/// Valid files this crate deliberately does not pass through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    Alpha,
    Transparency,
    Interlace,
    /// PNG bit depth other than 8, or a JPEG sample precision other than 8.
    BitDepth(u8),
    /// A JPEG with a component count other than 1, 3 or 4.
    JpegComponents(u8),
    Arithmetic,
    /// Lossless and hierarchical (differential) JPEG: not baseline or
    /// progressive Huffman, so no PDF reader decodes them through DCTDecode.
    Lossless,
}

/// Probe `bytes` as a JPEG or a PNG. Files with more than `max_pixels`
/// pixels are `TooLarge`.
pub fn probe(bytes: &[u8], max_pixels: u64) -> Result<Probed, ProbeError> {
    if bytes.starts_with(&png::SIGNATURE) {
        png::probe(bytes, max_pixels)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        jpeg::probe(bytes, max_pixels)
    } else {
        Err(ProbeError::NotImage)
    }
}

/// The reason line for a skipped page (user-facing, exact copy).
/// `JpegComponents` and the non-16 `BitDepth` carry a number the caller
/// formats itself; these are the number-free forms.
pub fn unsupported_text(u: &Unsupported) -> &'static str {
    match u {
        Unsupported::Alpha => "PNG with alpha needs the image family",
        Unsupported::Transparency => "PNG transparency needs the image family",
        Unsupported::Interlace => "interlaced PNG needs the image family",
        Unsupported::BitDepth(16) => "16-bit PNG needs the image family",
        Unsupported::BitDepth(_) => "this image bit depth is not supported",
        Unsupported::JpegComponents(_) => "JPEG with this component count is not supported",
        Unsupported::Arithmetic => "arithmetic-coded JPEG is not supported",
        Unsupported::Lossless => "lossless JPEG is not supported",
    }
}
