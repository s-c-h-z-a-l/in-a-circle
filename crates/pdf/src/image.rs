//! The page layer: one image XObject, one content stream, one page object.

use crate::object::Objects;
use probe::{Image, PngColor};
use std::io::{self, Write};

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// Write the three objects of a page and return the page's id.
pub(crate) fn add_page<W: Write>(
    objs: &mut Objects<W>,
    parent: u32,
    bytes: &[u8],
    image: &Image,
) -> io::Result<u32> {
    let (width, height) = match image {
        Image::Jpeg(j) => (j.width, j.height),
        Image::Png(p) => (p.width, p.height),
    };
    if width == 0 || height == 0 {
        return Err(invalid("image has no area"));
    }
    // Everything that can be refused is refused before the first byte of
    // this page is written.
    let (dict, ranges) = match image {
        Image::Jpeg(j) => {
            let space = match j.components {
                1 => "/DeviceGray",
                3 => "/DeviceRGB",
                4 => "/DeviceCMYK",
                _ => return Err(invalid("JPEG component count")),
            };
            // Adobe CMYK stores inverted values; /Decode flips them back.
            let decode = if j.adobe_inverted && j.components == 4 {
                " /Decode [1 0 1 0 1 0 1 0]"
            } else {
                ""
            };
            (
                format!(
                    "/Type /XObject /Subtype /Image /Width {width} /Height {height} \
                     /ColorSpace {space} /BitsPerComponent 8 /Filter /DCTDecode{decode}"
                ),
                None,
            )
        }
        Image::Png(p) => {
            let (space, colors) = match p.color {
                PngColor::Gray => ("/DeviceGray".to_string(), 1),
                PngColor::Rgb => ("/DeviceRGB".to_string(), 3),
                PngColor::Palette => {
                    let r = p
                        .palette
                        .clone()
                        .ok_or_else(|| invalid("palette missing"))?;
                    let plte = bytes
                        .get(r)
                        .ok_or_else(|| invalid("palette out of range"))?;
                    // 1..=256 RGB entries (PNG spec 11.2.3).
                    if plte.is_empty() || plte.len() % 3 != 0 || plte.len() > 3 * 256 {
                        return Err(invalid("palette size"));
                    }
                    let mut hex = String::with_capacity(plte.len() * 2);
                    for b in plte {
                        hex.push_str(&format!("{b:02X}"));
                    }
                    (
                        format!("[/Indexed /DeviceRGB {} <{hex}>]", plte.len() / 3 - 1),
                        1,
                    )
                }
            };
            for r in &p.idat {
                bytes
                    .get(r.clone())
                    .ok_or_else(|| invalid("IDAT out of range"))?;
            }
            if p.idat.is_empty() {
                return Err(invalid("no IDAT"));
            }
            (
                format!(
                    "/Type /XObject /Subtype /Image /Width {width} /Height {height} \
                     /ColorSpace {space} /BitsPerComponent 8 /Filter /FlateDecode \
                     /DecodeParms <</Predictor 15 /Colors {colors} /BitsPerComponent 8 \
                     /Columns {width}>>"
                ),
                Some(&p.idat),
            )
        }
    };

    let img = objs.alloc();
    let content = objs.alloc();
    let page = objs.alloc();
    match ranges {
        // PNG: the zlib stream is the IDAT data back to back; the PNG
        // predictor bytes inside it are exactly what /Predictor 15 reads.
        Some(idat) => objs.write_stream(img, &dict, idat.iter().map(|r| &bytes[r.clone()]))?,
        // JPEG: the whole file is the DCT stream.
        None => objs.write_stream(img, &dict, [bytes])?,
    }
    // 1 pixel = 1 point: the image is scaled to its own pixel size.
    let draw = format!("q {width} 0 0 {height} 0 0 cm /Im0 Do Q\n");
    objs.write_stream(content, "", [draw.as_bytes()])?;
    objs.write_object(
        page,
        &format!(
            "/Type /Page /Parent {parent} 0 R /MediaBox [0 0 {width} {height}] \
             /Resources <</XObject <</Im0 {img} 0 R>>>> /Contents {content} 0 R"
        ),
    )?;
    Ok(page)
}
