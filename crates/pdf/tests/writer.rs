//! Contract tests of the streaming PDF writer: structure checked by the
//! test itself, then by independent readers (qpdf, pdftoppm, djpeg) when
//! they are installed.

use pdf::Writer;
use probe::{Image, PngColor, probe};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MAX: u64 = 180_000_000;

// ---------- inputs ----------

fn jpeg_fixture(name: &str) -> Vec<u8> {
    // The JPEG fixtures live only in crates/probe/tests/data, so they are not duplicated.
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../probe/tests/data")
        .join(name);
    fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A page: its file bytes and the pixels the page must show (RGB, row
/// major) when `expected` is known exactly.
struct Page {
    bytes: Vec<u8>,
    w: usize,
    h: usize,
    /// RGB of the source image where it is computable without a JPEG
    /// decoder (PNG); `None` for JPEG (djpeg decides).
    rgb: Option<Vec<u8>>,
    name: &'static str,
}

/// Forward PNG filters, so the generated scanlines use all five filter types
/// and the writer's `/Predictor 15` is really exercised.
fn filter_rows(pix: &[u8], w: usize, h: usize, bpp: usize) -> Vec<u8> {
    let stride = w * bpp;
    let mut out = Vec::new();
    for y in 0..h {
        let row = &pix[y * stride..(y + 1) * stride];
        let prev = if y > 0 {
            &pix[(y - 1) * stride..y * stride]
        } else {
            &[][..]
        };
        let ft = (y % 5) as u8;
        out.push(ft);
        for x in 0..stride {
            let a = if x >= bpp { i32::from(row[x - bpp]) } else { 0 };
            let b = if y > 0 { i32::from(prev[x]) } else { 0 };
            let c = if y > 0 && x >= bpp {
                i32::from(prev[x - bpp])
            } else {
                0
            };
            let pred = match ft {
                0 => 0,
                1 => a,
                2 => b,
                3 => (a + b) / 2,
                _ => {
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
            };
            out.push((i32::from(row[x]) - pred) as u8);
        }
    }
    out
}

fn png_page(color: u8, w: usize, h: usize, name: &'static str) -> Page {
    let bpp = if color == 2 { 3 } else { 1 };
    let mut pix = Vec::new();
    for y in 0..h {
        for x in 0..w * bpp {
            pix.push(((x * 37 + y * 91 + x * y) % 251) as u8);
        }
    }
    // 16 palette entries; palette images use indices 0..16.
    let plte: Vec<u8> = (0..16u8)
        .flat_map(|i| [i * 16, 255 - i * 9, i.wrapping_mul(53)])
        .collect();
    if color == 3 {
        for v in &mut pix {
            *v %= 16;
        }
    }
    let rgb = match color {
        0 => pix.iter().flat_map(|&g| [g, g, g]).collect(),
        2 => pix.clone(),
        _ => pix
            .iter()
            .flat_map(|&i| {
                let i = usize::from(i) * 3;
                [plte[i], plte[i + 1], plte[i + 2]]
            })
            .collect(),
    };
    let rows = filter_rows(&pix, w, h, bpp);
    let pal = (color == 3).then_some(&plte[..]);
    let bytes = testzip::png(w as u32, h as u32, color, pal, false, false, &rows);
    Page {
        bytes,
        w,
        h,
        rgb: Some(rgb),
        name,
    }
}

fn jpeg_page(file: &'static str, w: usize, h: usize) -> Page {
    Page {
        bytes: jpeg_fixture(file),
        w,
        h,
        rgb: None,
        name: file,
    }
}

fn all_pages() -> Vec<Page> {
    vec![
        jpeg_page("gray.jpg", 24, 16),
        jpeg_page("rgb.jpg", 32, 24),
        jpeg_page("progressive.jpg", 40, 30),
        jpeg_page("cmyk_adobe.jpg", 16, 16),
        png_page(0, 21, 13, "png-gray"),
        png_page(2, 18, 25, "png-rgb"),
        png_page(3, 17, 11, "png-palette"),
    ]
}

fn build(pages: &[Page]) -> Vec<u8> {
    let mut w = Writer::new(Vec::new()).unwrap();
    for p in pages {
        let probed = probe(&p.bytes, MAX).unwrap();
        if let Image::Png(info) = &probed.image {
            probe::check_png_data(&p.bytes, info).unwrap();
            assert_eq!((info.width as usize, info.height as usize), (p.w, p.h));
        }
        w.add_page(&p.bytes, &probed.image).unwrap();
    }
    w.finish().unwrap()
}

// ---------- structure, checked by the test itself ----------

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

fn rfind(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len())
        .rposition(|w| w == needle)
        .unwrap()
}

/// Every xref offset points at "N 0 obj"; /Length of every stream matches
/// the bytes up to `endstream`; the trailer's startxref points at "xref".
/// Returns the object count.
fn check_structure(pdf: &[u8]) -> usize {
    assert!(pdf.starts_with(b"%PDF-1.4\n%"));
    assert!(
        pdf[10..14].iter().any(|&b| b >= 0x80),
        "binary marker comment"
    );
    assert!(pdf.ends_with(b"%%EOF\n"));
    let sx = rfind(pdf, b"startxref\n");
    let tail = std::str::from_utf8(&pdf[sx + 10..]).unwrap();
    let xref_at: usize = tail.lines().next().unwrap().parse().unwrap();
    assert_eq!(&pdf[xref_at..xref_at + 5], b"xref\n");
    let hdr_end = find(pdf, b"\n", xref_at + 5).unwrap();
    let hdr = std::str::from_utf8(&pdf[xref_at + 5..hdr_end]).unwrap();
    let (first, count) = hdr.split_once(' ').unwrap();
    assert_eq!(first, "0");
    let count: usize = count.parse().unwrap();
    let mut at = hdr_end + 1;
    assert_eq!(&pdf[at..at + 20], b"0000000000 65535 f \n");
    at += 20;
    for id in 1..count {
        let e = &pdf[at..at + 20];
        at += 20;
        assert_eq!(&e[10..], b" 00000 n \n", "entry {id}");
        let off: usize = std::str::from_utf8(&e[..10]).unwrap().parse().unwrap();
        let want = format!("{id} 0 obj\n");
        assert_eq!(&pdf[off..off + want.len()], want.as_bytes(), "object {id}");
        // Streams: the declared /Length is the exact stream byte count.
        let end = find(pdf, b"endobj\n", off).unwrap();
        let obj = &pdf[off..end];
        if let Some(s) = find(obj, b"stream\n", 0) {
            let dict = std::str::from_utf8(&obj[..s]).unwrap();
            let l = dict.find("/Length ").unwrap() + 8;
            let len: usize = dict[l..]
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(&obj[s + 7 + len..], b"\nendstream\n", "object {id} length");
        }
    }
    let trailer = String::from_utf8_lossy(&pdf[at..sx]);
    assert!(
        trailer.contains(&format!("/Size {count} /Root")),
        "{trailer}"
    );
    // The trailer follows the table directly.
    assert_eq!(&pdf[at..at + 8], b"trailer\n");
    count
}

#[test]
fn one_page_per_image_type() {
    for p in all_pages() {
        let out = build(std::slice::from_ref(&p));
        let n = check_structure(&out);
        // free head + Pages + (image, content, page) + Catalog.
        assert_eq!(n, 1 + 1 + 3 + 1, "{}", p.name);
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains(&format!("/MediaBox [0 0 {} {}]", p.w, p.h)),
            "{}",
            p.name
        );
        assert!(
            text.contains(&format!("q {} 0 0 {} 0 0 cm /Im0 Do Q", p.w, p.h)),
            "{}",
            p.name
        );
        assert!(text.contains("/Count 1"));
    }
}

#[test]
fn image_dictionaries_carry_the_color_model() {
    let get = |p: &Page| String::from_utf8_lossy(&build(std::slice::from_ref(p))).into_owned();
    let t = get(&jpeg_page("gray.jpg", 24, 16));
    assert!(t.contains("/Filter /DCTDecode") && t.contains("/ColorSpace /DeviceGray"));
    assert!(!t.contains("/Decode"));
    let t = get(&jpeg_page("rgb.jpg", 32, 24));
    assert!(t.contains("/ColorSpace /DeviceRGB") && !t.contains("/Decode"));
    let t = get(&jpeg_page("cmyk_adobe.jpg", 16, 16));
    assert!(t.contains("/ColorSpace /DeviceCMYK"));
    assert!(t.contains("/Decode [1 0 1 0 1 0 1 0]"));
    let t = get(&png_page(2, 18, 25, "x"));
    assert!(t.contains("/Filter /FlateDecode"));
    assert!(t.contains("/DecodeParms <</Predictor 15 /Colors 3 /BitsPerComponent 8 /Columns 18>>"));
    let t = get(&png_page(3, 17, 11, "x"));
    assert!(
        t.contains("/ColorSpace [/Indexed /DeviceRGB 15 <00FF00"),
        "{t}"
    );
    assert!(t.contains("/Colors 1 "));
}

#[test]
fn png_stream_is_the_idat_concatenation() {
    // Several IDAT chunks: the PDF stream is their data back to back,
    // byte for byte, and /Length is that size.
    let p = png_page(0, 21, 13, "x");
    let probed = probe(&p.bytes, MAX).unwrap();
    let Image::Png(info) = &probed.image else {
        panic!()
    };
    let whole: Vec<u8> = info
        .idat
        .iter()
        .flat_map(|r| p.bytes[r.clone()].to_vec())
        .collect();
    let out = build(std::slice::from_ref(&p));
    assert!(find(&out, &whole, 0).is_some());
    assert!(String::from_utf8_lossy(&out).contains(&format!("/Length {} ", whole.len())));
}

#[test]
fn pages_keep_their_order_and_kids_are_listed() {
    let pages = all_pages();
    let out = build(&pages);
    let n = check_structure(&out);
    assert_eq!(n, 1 + 1 + 3 * pages.len() + 1);
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains(&format!("/Count {}", pages.len())));
    // MediaBox sizes appear in page order.
    let mut last = 0;
    for p in &pages {
        let m = format!("/MediaBox [0 0 {} {}]", p.w, p.h);
        let at = text[last..]
            .find(&m)
            .unwrap_or_else(|| panic!("{} out of order", p.name))
            + last;
        last = at;
    }
}

#[test]
fn empty_document_and_bad_input_are_errors() {
    let w = Writer::new(Vec::new()).unwrap();
    assert!(w.finish().is_err(), "a PDF without pages is not written");
    // PNG ranges outside the bytes are an error, not a panic.
    let p = png_page(0, 21, 13, "x");
    let probed = probe(&p.bytes, MAX).unwrap();
    let mut w = Writer::new(Vec::new()).unwrap();
    assert!(w.add_page(&p.bytes[..40], &probed.image).is_err());
    // Palette image without palette range.
    let p = png_page(3, 17, 11, "x");
    let Image::Png(mut info) = probe(&p.bytes, MAX).unwrap().image else {
        panic!()
    };
    assert_eq!(info.color, PngColor::Palette);
    info.palette = None;
    let mut w = Writer::new(Vec::new()).unwrap();
    assert!(w.add_page(&p.bytes, &Image::Png(info)).is_err());
}

#[test]
fn write_errors_propagate() {
    struct Full(usize);
    impl std::io::Write for Full {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if self.0 < b.len() {
                return Err(std::io::Error::from(std::io::ErrorKind::StorageFull));
            }
            self.0 -= b.len();
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let p = jpeg_page("rgb.jpg", 32, 24);
    let probed = probe(&p.bytes, MAX).unwrap();
    let r = Writer::new(Full(100)).and_then(|mut w| {
        w.add_page(&p.bytes, &probed.image)?;
        w.finish().map(|_| ())
    });
    assert!(r.is_err());
}

// ---------- independent readers ----------

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

/// A scratch directory under the system temp dir, removed on drop.
struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("inacircle-pdf-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("home/cache")).unwrap();
        Tmp(d)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Run a tool with HOME and the XDG dirs inside the scratch dir.
fn run(tmp: &Tmp, prog: &str, args: &[&std::ffi::OsStr]) -> std::process::Output {
    let home = tmp.0.join("home");
    Command::new(prog)
        .args(args)
        .current_dir(&tmp.0)
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("{prog}: {e}"))
}

/// Parse a binary PNM (P5 gray or P6 RGB) into (w, h, RGB bytes).
fn read_pnm(data: &[u8]) -> (usize, usize, Vec<u8>) {
    let mut fields = Vec::new();
    let mut i = 2;
    while fields.len() < 3 {
        while data[i].is_ascii_whitespace() {
            i += 1;
        }
        let s = i;
        while !data[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(
            std::str::from_utf8(&data[s..i])
                .unwrap()
                .parse::<usize>()
                .unwrap(),
        );
    }
    assert_eq!(fields[2], 255);
    let px = &data[i + 1..];
    let (w, h) = (fields[0], fields[1]);
    match &data[..2] {
        b"P6" => {
            assert_eq!(px.len(), w * h * 3);
            (w, h, px.to_vec())
        }
        b"P5" => {
            assert_eq!(px.len(), w * h);
            (w, h, px.iter().flat_map(|&g| [g, g, g]).collect())
        }
        m => panic!("unexpected PNM magic {m:?}"),
    }
}

/// Largest per-channel difference.
fn max_diff(a: &[u8], b: &[u8]) -> u8 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0)
}

/// Mean of each channel over a 4 x 4 grid of equal blocks (RGB, 48 values).
fn block_means(w: usize, h: usize, rgb: &[u8]) -> Vec<f64> {
    let mut sum = vec![0f64; 48];
    let mut n = [0f64; 16];
    for y in 0..h {
        for x in 0..w {
            let cell = (y * 4 / h) * 4 + x * 4 / w;
            n[cell] += 1.0;
            for c in 0..3 {
                sum[cell * 3 + c] += f64::from(rgb[(y * w + x) * 3 + c]);
            }
        }
    }
    sum.iter().enumerate().map(|(i, s)| s / n[i / 3]).collect()
}

fn max_block_diff(a: &[f64], b: &[f64]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f64::max)
}

// Tolerances, per check and image type, and why. They were chosen from the
// measured values (printed with --nocapture) and each is shown to reject a
// wrong image by the negative controls in `tolerances_catch_wrong_images`.
//
// 1. Decoded pixels, read back with `pdfimages` (poppler's decoder, no
//    rendering, no resampling):
//    * PNG gray, RGB, palette: 0. Flate, the five PNG predictors and the
//      Indexed color space are integer-exact; any difference is a writer
//      bug. The generated images are pseudo-noise, so a wrong predictor,
//      column count or color count changes almost every pixel.
//    * JPEG gray, RGB, progressive: 0 measured against `djpeg` (the
//      fixtures are 4:4:4 and poppler decodes with libjpeg(-turbo) too);
//      the allowance below is 1 for an IDCT rounding step on another
//      libjpeg build. A wrong image is off by tens (negative controls).
//    * The JPEG bytes themselves must come back from `pdfimages -all`
//      identical to the input: passthrough, nothing re-encoded.
//    * CMYK: bytes only (above). Poppler converts DeviceCMYK to RGB with
//      its own formula, not djpeg's, so a pixel comparison would test the
//      converter, not our file.
// 2. Rendering with `pdftoppm -r 72`: page count, order and pixel size are
//    exact (1 px = 1 pt). Pixel values are NOT exact: poppler 26.08
//    resamples an image drawn 1:1 so that a ramp of n rows comes out as
//    y * 255 / n instead of y * 255 / (n - 1) (measured, same on a
//    hand-made PDF, so it is the renderer, not the writer): up to 14 off on
//    a 16-row gray ramp. Only a low-frequency check is meaningful: the mean
//    of each channel over a 4 x 4 grid of blocks, tolerance 16 =
//    255 / 16, the resampling shift of the shortest smooth fixture (16
//    rows); the measured worst case (12.75, the 16-row gray ramp) is
//    printed by the test, and the R/B-swap control below differs by 201.
//    Only the smooth JPEG fixtures take this check: the PNG pages are
//    pseudo-noise (to exercise the predictors), whose block means move by
//    up to 38 under the resampling (measured), so for them the render
//    proves page count, order and size, and `pdfimages` the exact pixels.
//    Compared against the expected pixels (PNG: the generated ones, JPEG: djpeg's output).
//    CMYK is classified (left half red, right half blue: R >= 150 and
//    G, B <= 100 on the left; B >= 120 and R, G <= 100 on the right: poppler
//    renders the fixture's red as (236, 27, 36) and its blue as
//    (46, 49, 145)).
const PNG_TOLERANCE: u8 = 0;
const JPEG_TOLERANCE: u8 = 1;
const BLOCK_TOLERANCE: f64 = 16.0;

fn toolset() -> bool {
    // `&` not `&&`: every missing tool is reported (and fails when required).
    let q = have("qpdf");
    let t = have("pdftoppm");
    let i = have("pdfimages");
    let d = have("djpeg");
    q & t & i & d
}

/// `qpdf --check` and `pdftoppm -r 72`: the pages as (w, h, RGB), in order.
fn render(tmp: &Tmp, pdf_bytes: &[u8], tag: &str) -> Vec<(usize, usize, Vec<u8>)> {
    let path = tmp.0.join(format!("{tag}.pdf"));
    fs::write(&path, pdf_bytes).unwrap();
    let q = run(tmp, "qpdf", &["--check".as_ref(), path.as_os_str()]);
    assert!(
        q.status.success(),
        "qpdf --check: {}{}",
        String::from_utf8_lossy(&q.stdout),
        String::from_utf8_lossy(&q.stderr)
    );
    let prefix = tmp.0.join(format!("{tag}-page"));
    let r = run(
        tmp,
        "pdftoppm",
        &[
            "-r".as_ref(),
            "72".as_ref(),
            path.as_os_str(),
            prefix.as_os_str(),
        ],
    );
    assert!(
        r.status.success(),
        "pdftoppm: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert!(
        r.stderr.is_empty(),
        "pdftoppm warned: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    // pdftoppm zero-pads the page number to the width of the page count,
    // so a name sort is the page order.
    list(tmp, &format!("{tag}-page-"), ".ppm")
        .iter()
        .map(|f| read_pnm(&fs::read(f).unwrap()))
        .collect()
}

fn list(tmp: &Tmp, prefix: &str, suffix: &str) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(&tmp.0)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy();
            n.starts_with(prefix) && n.ends_with(suffix)
        })
        .collect();
    files.sort();
    files
}

/// `pdfimages` on the PDF: the decoded images (PGM/PPM, default format) and
/// the JPEG streams as stored (`-all`), in image order. A PDF here has one
/// image per page.
type Pnm = (usize, usize, Vec<u8>);

fn images(tmp: &Tmp, pdf_bytes: &[u8], tag: &str) -> (Vec<Pnm>, Vec<Vec<u8>>) {
    let path = tmp.0.join(format!("{tag}.pdf"));
    fs::write(&path, pdf_bytes).unwrap();
    let dec = tmp.0.join(format!("{tag}-dec"));
    let raw = tmp.0.join(format!("{tag}-raw"));
    for (root, flags) in [(&dec, &[][..]), (&raw, &["-all"][..])] {
        let mut args: Vec<&std::ffi::OsStr> = flags.iter().map(|f| f.as_ref()).collect();
        args.push(path.as_os_str());
        args.push(root.as_os_str());
        let o = run(tmp, "pdfimages", &args);
        assert!(
            o.status.success(),
            "pdfimages: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    let decoded = list(tmp, &format!("{tag}-dec-"), "")
        .iter()
        .map(|f| read_pnm(&fs::read(f).unwrap()))
        .collect();
    let jpegs = list(tmp, &format!("{tag}-raw-"), ".jpg")
        .iter()
        .map(|f| fs::read(f).unwrap())
        .collect();
    (decoded, jpegs)
}

fn djpeg(tmp: &Tmp, name: &str, bytes: &[u8]) -> (usize, usize, Vec<u8>) {
    let path = tmp.0.join(name);
    fs::write(&path, bytes).unwrap();
    let o = run(tmp, "djpeg", &["-pnm".as_ref(), path.as_os_str()]);
    assert!(
        o.status.success(),
        "djpeg: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    read_pnm(&o.stdout)
}

/// True if the half-image classification of the CMYK fixture holds.
fn cmyk_is_red_then_blue(w: usize, h: usize, rgb: &[u8]) -> bool {
    let px = |x: usize, y: usize| &rgb[(y * w + x) * 3..(y * w + x) * 3 + 3];
    let (l, r) = (px(w / 4, h / 2), px(3 * w / 4, h / 2));
    l[0] >= 150 && l[1] <= 100 && l[2] <= 100 && r[2] >= 120 && r[0] <= 100 && r[1] <= 100
}

/// The pixels a page must show: PNG, the generated ones; JPEG, djpeg's.
fn expected_rgb(tmp: &Tmp, p: &Page) -> Option<Vec<u8>> {
    match &p.rgb {
        Some(v) => Some(v.clone()),
        None if p.name == "cmyk_adobe.jpg" => None,
        None => {
            let (w, h, v) = djpeg(tmp, p.name, &p.bytes);
            assert_eq!((w, h), (p.w, p.h));
            Some(v)
        }
    }
}

#[test]
fn independent_reader_accepts_and_renders() {
    if !toolset() {
        return;
    }
    let tmp = Tmp::new("render");
    let pages = all_pages();
    let pdf_bytes = build(&pages);

    // qpdf accepts the file; pdftoppm: count, order and size of the pages.
    let rendered = render(&tmp, &pdf_bytes, "all");
    assert_eq!(rendered.len(), pages.len(), "page count");
    let mut worst_block = 0f64;
    for (p, (rw, rh, rgb)) in pages.iter().zip(&rendered) {
        assert_eq!((*rw, *rh), (p.w, p.h), "{}: 1 px = 1 pt", p.name);
        match expected_rgb(&tmp, p) {
            Some(_) if p.rgb.is_some() => {} // pseudo-noise PNG: see above
            Some(exp) => {
                let d = max_block_diff(&block_means(*rw, *rh, rgb), &block_means(p.w, p.h, &exp));
                worst_block = worst_block.max(d);
                assert!(d <= BLOCK_TOLERANCE, "{}: block mean diff {d}", p.name);
            }
            None => assert!(cmyk_is_red_then_blue(*rw, *rh, rgb), "{}", p.name),
        }
    }
    eprintln!("worst 4x4 block-mean diff of the render: {worst_block:.2}");

    // pdfimages: exact decoded pixels and byte-identical JPEG streams.
    let (decoded, jpegs) = images(&tmp, &pdf_bytes, "img");
    assert_eq!(decoded.len(), pages.len());
    let mut worst_jpeg = 0u8;
    let mut jpeg_i = 0;
    for (p, (w, h, rgb)) in pages.iter().zip(&decoded) {
        assert_eq!((*w, *h), (p.w, p.h), "{}", p.name);
        if p.name.ends_with(".jpg") {
            assert_eq!(jpegs[jpeg_i], p.bytes, "{}: passthrough", p.name);
            jpeg_i += 1;
        }
        match expected_rgb(&tmp, p) {
            Some(exp) if p.rgb.is_some() => {
                assert_eq!(max_diff(rgb, &exp), PNG_TOLERANCE, "{}", p.name);
            }
            Some(exp) => {
                let d = max_diff(rgb, &exp);
                worst_jpeg = worst_jpeg.max(d);
                assert!(d <= JPEG_TOLERANCE, "{}: max diff {d} vs djpeg", p.name);
            }
            None => {} // CMYK: bytes only, see the tolerance notes
        }
    }
    assert_eq!(jpeg_i, jpegs.len());
    eprintln!("worst decoded JPEG diff vs djpeg: {worst_jpeg}");

    // Each page alone gives the same render as inside the combined document.
    for (i, p) in pages.iter().enumerate() {
        let one = render(&tmp, &build(std::slice::from_ref(p)), "one");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0], rendered[i], "{}", p.name);
        for f in list(&tmp, "one-page", "") {
            fs::remove_file(f).unwrap();
        }
    }
}

/// The tolerances above must reject a wrong image. Each control builds a
/// wrong-but-valid situation and shows the comparison fails by a margin.
#[test]
fn tolerances_catch_wrong_images() {
    if !toolset() {
        return;
    }
    let tmp = Tmp::new("negative");
    let pages = all_pages();
    let pdf_bytes = build(&pages);
    let rendered = render(&tmp, &pdf_bytes, "neg");
    let (decoded, _) = images(&tmp, &pdf_bytes, "negimg");

    // 1. PNG, exact: one channel of one pixel off by one is caught; so is
    //    every other page's pixels offered in place of this page's (sizes
    //    equal by cropping the common prefix of the byte stream).
    let png = &pages[4];
    let exp = png.rgb.clone().unwrap();
    assert_eq!(
        max_diff(&decoded[4].2, &exp),
        0,
        "control: the right pixels pass"
    );
    let mut off_by_one = exp.clone();
    off_by_one[(png.h / 2 * png.w + png.w / 2) * 3 + 1] ^= 1;
    assert!(max_diff(&decoded[4].2, &off_by_one) > PNG_TOLERANCE);
    let other = pages[5].rgb.clone().unwrap(); // the RGB PNG, other content
    let n = exp.len().min(other.len());
    assert!(
        max_diff(&decoded[4].2[..n], &other[..n]) > 50,
        "swapped PNG pages"
    );

    // 2. JPEG decode: the RGB fixture's decoded pixels against the
    //    progressive fixture's (a swap) and the gray one's cropped to the
    //    common 24 x 16 corner (a wrong color space).
    let (_, _, rgb_dj) = djpeg(&tmp, "rgb.jpg", &pages[1].bytes);
    assert!(
        max_diff(&decoded[1].2, &rgb_dj) <= JPEG_TOLERANCE,
        "control: the right pixels pass"
    );
    let (_, _, prog_dj) = djpeg(&tmp, "progressive.jpg", &pages[2].bytes);
    let n = rgb_dj.len().min(prog_dj.len());
    let swapped = max_diff(&decoded[1].2[..n], &prog_dj[..n]);
    assert!(swapped > 30, "swapped JPEG pages differ by {swapped}");
    let (_, _, gray_dj) = djpeg(&tmp, "gray.jpg", &pages[0].bytes);
    let n = gray_dj.len();
    let wrong_space = max_diff(&decoded[0].2, &decoded[1].2[..n]);
    assert!(
        wrong_space > 30,
        "gray vs RGB content differ by {wrong_space}"
    );
    eprintln!("negative controls: swapped JPEG {swapped}, wrong space {wrong_space}");

    // 3. The 4 x 4 block check: red and blue channels swapped (a BGR/RGB
    //    mix-up) in the expected RGB fixture is far outside the tolerance.
    let (rw, rh, rgb) = &rendered[1];
    let good = max_block_diff(&block_means(*rw, *rh, rgb), &block_means(*rw, *rh, &rgb_dj));
    assert!(
        good <= BLOCK_TOLERANCE,
        "control: right pixels pass ({good})"
    );
    let swapped_rb: Vec<u8> = rgb_dj.chunks(3).flat_map(|p| [p[2], p[1], p[0]]).collect();
    let bad = max_block_diff(
        &block_means(*rw, *rh, rgb),
        &block_means(*rw, *rh, &swapped_rb),
    );
    assert!(
        bad > 5.0 * BLOCK_TOLERANCE,
        "R/B swap differs by only {bad}"
    );
    eprintln!("block check: right {good:.2}, R/B swapped {bad:.2}");

    // 4. Inverted CMYK: the same file written without the /Decode array
    //    (adobe_inverted forced false) renders as the complementary colours
    //    and fails the classification the correct file passes.
    let cmyk = &pages[3];
    let Image::Jpeg(mut j) = probe(&cmyk.bytes, MAX).unwrap().image else {
        panic!()
    };
    assert!(j.adobe_inverted);
    assert!(
        cmyk_is_red_then_blue(16, 16, &rendered[3].2),
        "control: the right file passes"
    );
    j.adobe_inverted = false;
    let mut w = Writer::new(Vec::new()).unwrap();
    w.add_page(&cmyk.bytes, &Image::Jpeg(j)).unwrap();
    let wrong = render(&tmp, &w.finish().unwrap(), "inv");
    assert!(
        !cmyk_is_red_then_blue(wrong[0].0, wrong[0].1, &wrong[0].2),
        "uninverted CMYK must fail the check"
    );
    let right = &rendered[3].2;
    let m = right
        .iter()
        .zip(&wrong[0].2)
        .map(|(a, b)| f64::from(a.abs_diff(*b)))
        .sum::<f64>()
        / right.len() as f64;
    eprintln!("inverted CMYK: mean diff {m:.1}");
    assert!(m > 50.0);
}

// ---------- lying input and limits ----------

#[test]
fn a_zero_width_or_zero_height_image_is_refused() {
    for (width, height) in [(0u32, 5u32), (5, 0), (0, 0)] {
        let info = probe::JpegInfo {
            width,
            height,
            components: 3,
            adobe_inverted: false,
        };
        let mut w = Writer::new(Vec::new()).unwrap();
        assert!(
            w.add_page(&[0xFF, 0xD8], &Image::Jpeg(info)).is_err(),
            "{width}x{height}"
        );
    }
    for (width, height) in [(0u32, 5u32), (5, 0)] {
        let info = probe::PngInfo {
            width,
            height,
            color: PngColor::Gray,
            palette: None,
            idat: std::iter::once(0..4).collect(),
        };
        let mut w = Writer::new(Vec::new()).unwrap();
        assert!(w.add_page(&[0; 8], &Image::Png(info)).is_err());
    }
}

#[test]
fn palette_sizes_are_one_to_256_rgb_entries() {
    let bytes = vec![7u8; 4096];
    let page = |len: usize| {
        let info = probe::PngInfo {
            width: 2,
            height: 2,
            color: PngColor::Palette,
            palette: Some(0..len),
            idat: std::iter::once(1000..1010).collect(),
        };
        Writer::new(Vec::new())
            .unwrap()
            .add_page(&bytes, &Image::Png(info))
    };
    // Empty, not a multiple of 3, and one entry too many are each refused
    // on their own; the smallest and the largest palette are written.
    for bad in [0, 1, 4, 767, 771, 1000] {
        assert!(page(bad).is_err(), "{bad} palette bytes");
    }
    for good in [3, 6, 768] {
        page(good).unwrap();
    }
}

/// A sink that counts the bytes and notes where object 3 (the page's
/// content stream, the object after the image) starts.
#[derive(Clone, Default)]
struct Count {
    total: std::rc::Rc<std::cell::Cell<u64>>,
    content_at: std::rc::Rc<std::cell::Cell<Option<u64>>>,
}
impl std::io::Write for Count {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if b == b"3 0 obj\n" {
            self.content_at.set(Some(self.total.get()));
        }
        self.total.set(self.total.get() + b.len() as u64);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The largest offset an xref entry can hold: ten decimal digits.
const MAX_OFFSET: u64 = 9_999_999_999;

/// Write one gray PNG page whose image stream is `149 * 64 MiB + extra`
/// bytes (the same zero buffer listed over and over: a lying `PngInfo`
/// costs nothing but the count). Returns the sink and the result.
fn big_page(extra: usize) -> (Count, std::io::Result<()>) {
    const CHUNK: usize = 64 << 20;
    let buf = vec![0u8; CHUNK];
    let mut idat = vec![0..CHUNK; 149];
    idat.push(0..extra);
    let info = probe::PngInfo {
        width: 1,
        height: 1,
        color: PngColor::Gray,
        palette: None,
        idat,
    };
    let sink = Count::default();
    let mut w = Writer::new(sink.clone()).unwrap();
    let r = w.add_page(&buf, &Image::Png(info));
    (sink, r)
}

#[test]
fn an_object_may_start_at_the_last_ten_digit_offset_but_not_past_it() {
    let (sink, r) = big_page(0);
    r.unwrap();
    let base = sink.content_at.get().unwrap();
    assert!(base < MAX_OFFSET - 1000, "the test scale is off: {base}");

    // The content stream starts exactly at the last offset: it is written
    // (the page object behind it is not).
    let (sink, r) = big_page((MAX_OFFSET - base) as usize);
    assert_eq!(sink.content_at.get(), Some(MAX_OFFSET));
    assert!(r.is_err());
    assert!(sink.total.get() > MAX_OFFSET + 8, "the object was refused");

    // One byte further: refused before its first byte.
    let (sink, r) = big_page((MAX_OFFSET + 1 - base) as usize);
    assert!(r.is_err());
    assert_eq!(sink.content_at.get(), None);
    assert_eq!(sink.total.get(), MAX_OFFSET + 1);
}
