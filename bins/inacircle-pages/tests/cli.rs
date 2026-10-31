//! Behavior of `inacircle-pages` run directly: one CBZ in, one PDF out, or
//! a `skip`/`fail` with the plan's exact copy and no file left behind.
//! PDFs are checked with independent tools (qpdf, pdfinfo) when installed;
//! `INACIRCLE_REQUIRE_PDF_TOOLS=1` turns a missing tool into a failure.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use testzip::Store::{Deflated, Stored};

const MANIFEST: &str = "inacircle-manifest 0\n\
action\tpages.cbz-to-pdf\tfamily=pages\tin=application/vnd.comicbook+zip\tout=application/pdf\text=pdf\tlabel=PDF\tgroup=book\tcost=10\tlossy=yes\tbatch=yes\n\
action\tpages.images-to-pdf\tfamily=pages\tin=image/jpeg,image/png\tout=application/pdf\text=pdf\tlabel=PDF\tgroup=book\tcost=10\tlossy=yes\tbatch=yes\tjoin=yes\tmin=1\n\
action\tpages.images-to-cbz\tfamily=pages\tin=image/jpeg,image/png\tout=application/vnd.comicbook+zip\text=cbz\tlabel=CBZ\tgroup=book\tcost=10\tlossy=no\tbatch=yes\tjoin=yes\tmin=1\n";

/// A unique temporary directory, removed on drop.
struct Tmp(PathBuf);

impl std::ops::Deref for Tmp {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn tmp(name: &str) -> Tmp {
    let d = std::env::temp_dir().join(format!(
        "inacircle-pages-test-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(d.join("work")).unwrap();
    fs::create_dir_all(d.join("home/cache")).unwrap();
    Tmp(d)
}

fn command(work: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_inacircle-pages"));
    c.args(["run", "pages.cbz-to-pdf", "--work"])
        .arg(work)
        .arg("--");
    c
}

fn run(work: &Path, inputs: &[&Path]) -> Output {
    command(work).args(inputs).output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn lines<'a>(out: &'a str, kind: &str) -> Vec<Vec<&'a str>> {
    out.lines()
        .map(|l| l.split('\t').collect::<Vec<_>>())
        .filter(|f| f[0] == kind)
        .collect()
}

/// Write a CBZ made of `(name, data, method)` entries and return its path.
fn cbz(dir: &Path, name: &str, entries: &[(&[u8], &[u8], testzip::Store)]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, testzip::zip(entries)).unwrap();
    p
}

// ---------- images ----------

fn jpeg_fixture(name: &str) -> Vec<u8> {
    // The JPEG fixtures live only in crates/probe/tests/data, so they are not duplicated.
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/probe/tests/data")
        .join(name);
    fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// A valid 8-bit PNG of a given color type (0 gray, 2 RGB, 6 RGBA), all zero
/// pixels, filter type 0 on every row.
fn flat_png(w: u32, h: u32, color: u8) -> Vec<u8> {
    let bpp = match color {
        0 => 1,
        2 => 3,
        _ => 4,
    };
    let rows = vec![0u8; h as usize * (1 + w as usize * bpp)];
    testzip::png(w, h, color, None, false, false, &rows)
}

fn rgb_png(w: u32, h: u32) -> Vec<u8> {
    flat_png(w, h, 2)
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

fn tool(root: &Path, prog: &str, args: &[&std::ffi::OsStr]) -> Output {
    let home = root.join("home");
    Command::new(prog)
        .args(args)
        .current_dir(root)
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("{prog}: {e}"))
}

/// The page sizes of a PDF in page order, from `pdfinfo`, after `qpdf
/// --check`. `None` when a tool is missing (the test then skips this part).
fn page_sizes(root: &Path, pdf: &Path, pages: usize) -> Option<Vec<(u32, u32)>> {
    if !(have("qpdf") && have("pdfinfo")) {
        return None;
    }
    let q = tool(root, "qpdf", &["--check".as_ref(), pdf.as_os_str()]);
    assert!(
        q.status.success(),
        "qpdf --check: {}{}",
        String::from_utf8_lossy(&q.stdout),
        String::from_utf8_lossy(&q.stderr)
    );
    let last = pages.to_string();
    let o = tool(
        root,
        "pdfinfo",
        &[
            "-f".as_ref(),
            "1".as_ref(),
            "-l".as_ref(),
            last.as_ref(),
            pdf.as_os_str(),
        ],
    );
    assert!(o.status.success(), "pdfinfo failed");
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let mut v = Vec::new();
    for l in text.lines() {
        // "Page    1 size: 20 x 20 pts"
        if l.starts_with("Page ") && l.contains(" size: ") {
            let sz = l.split(" size: ").nth(1).unwrap();
            let mut p = sz.split(" x ");
            let w = p.next().unwrap().trim().parse::<f64>().unwrap();
            let h = p.next().unwrap().split_whitespace().next().unwrap();
            v.push((w as u32, h.parse::<f64>().unwrap() as u32));
        }
    }
    let count = text.lines().find(|l| l.starts_with("Pages:")).unwrap();
    assert_eq!(count.split_whitespace().nth(1).unwrap(), last, "{text}");
    Some(v)
}

/// Every file under `dir` with the extension `ext`.
fn files_with_ext(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            v.extend(files_with_ext(&p, ext));
        } else if p.extension().is_some_and(|x| x == ext) {
            v.push(p);
        }
    }
    v
}

fn assert_no_pdf(work: &Path) {
    let left = files_with_ext(work, "pdf");
    assert!(left.is_empty(), "half-written output left behind: {left:?}");
}

/// Run one CBZ and return (output, the single skip/fail line's fields).
fn one_input(t: &Tmp, kind: &str, input: &Path) -> (Output, Vec<String>) {
    let work = t.join("work");
    let o = run(&work, &[input]);
    let out = stdout(&o);
    let l = lines(&out, kind);
    assert_eq!(l.len(), 1, "{out}");
    assert_eq!(l[0][1], input.to_str().unwrap());
    let fields = l[0].iter().map(|s| s.to_string()).collect();
    assert_no_pdf(&work);
    (o, fields)
}

// ---------- tests ----------

#[test]
fn manifest_lists_three_actions() {
    let o = Command::new(env!("CARGO_BIN_EXE_inacircle-pages"))
        .arg("--manifest")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(stdout(&o), MANIFEST);
}

#[test]
fn converts_stored_and_deflated_cbz() {
    let t = tmp("convert");
    let jpg = jpeg_fixture("rgb.jpg"); // 32 x 24
    let one = rgb_png(20, 20);
    let ten = rgb_png(50, 10);
    // Archive order is not page order: natural order puts 10.png after 2.jpg.
    let input = cbz(
        &t,
        "book.cbz",
        &[
            (b"10.png", &ten, Deflated),
            (b"2.jpg", &jpg, Stored),
            (b"1.png", &one, Deflated),
        ],
    );
    let work = t.join("work");
    let o = run(&work, &[&input]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.starts_with("progress\t0\n"), "{out}");
    let done = lines(&out, "done");
    assert_eq!(done.len(), 1, "{out}");
    assert_eq!(done[0][1], input.to_str().unwrap());
    assert_eq!(done[0][2], "0/book.pdf");
    assert!(lines(&out, "warn").is_empty(), "{out}");
    let pdf = work.join("0/book.pdf");
    assert!(fs::read(&pdf).unwrap().starts_with(b"%PDF-"));
    if let Some(sizes) = page_sizes(&t, &pdf, 3) {
        assert_eq!(sizes, [(20, 20), (32, 24), (50, 10)]);
    }
}

#[test]
fn upper_case_extensions_and_folders_are_pages() {
    let t = tmp("upper");
    let a = rgb_png(7, 9);
    let b = jpeg_fixture("gray.jpg");
    let input = cbz(
        &t,
        "a.cbz",
        &[
            (b"ch2/1.JPEG", &b, Stored),
            (b"ch1/", b"", Stored),
            (b"ch1/9.PNG", &a, Stored),
        ],
    );
    let work = t.join("work");
    let o = run(&work, &[&input]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert!(lines(&stdout(&o), "warn").is_empty());
    if let Some(sizes) = page_sizes(&t, &work.join("0/a.pdf"), 2) {
        assert_eq!(sizes, [(7, 9), (24, 16)]);
    }
}

#[test]
fn mixed_cbz_is_skipped_whole_and_leaves_no_file() {
    let t = tmp("mixed");
    let jpg = jpeg_fixture("rgb.jpg");
    let alpha = flat_png(4, 4, 6);
    let input = cbz(
        &t,
        "m.cbz",
        &[
            (b"1.jpg", &jpg, Stored),
            (b"2.jpg", &jpg, Stored),
            (b"3.png", &alpha, Stored),
            (b"4.jpg", &jpg, Stored),
        ],
    );
    let (o, f) = one_input(&t, "skip", &input);
    assert_eq!(f[2], "page 3.png: PNG with alpha needs the image family");
    assert_eq!(o.status.code(), Some(3));
}

#[test]
fn unsupported_images_use_the_probe_copy() {
    let t = tmp("unsupported");
    let interlaced = {
        let rows = vec![0u8; 4 * (1 + 4 * 3)];
        testzip::png(4, 4, 2, None, false, true, &rows)
    };
    let trns = {
        let rows = vec![0u8; 4 * (1 + 4 * 3)];
        testzip::png(4, 4, 2, None, true, false, &rows)
    };
    for (data, want) in [
        (interlaced, "interlaced PNG needs the image family"),
        (trns, "PNG transparency needs the image family"),
    ] {
        let input = cbz(&t, "u.cbz", &[(b"1.png", &data, Stored)]);
        let (_, f) = one_input(&t, "skip", &input);
        assert_eq!(f[2], format!("page 1.png: {want}"));
    }
}

#[test]
fn jpeg_component_count_is_formatted_from_the_variant() {
    let t = tmp("components");
    // SOI, then a baseline frame header declaring 2 components.
    let j: Vec<u8> = vec![
        0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0E, 8, 0x00, 0x10, 0x00, 0x10, 2, 1, 0x11, 0, 2, 0x11, 0,
        0xFF, 0xD9,
    ];
    let input = cbz(&t, "j.cbz", &[(b"1.jpg", &j, Stored)]);
    let (_, f) = one_input(&t, "skip", &input);
    assert_eq!(f[2], "page 1.jpg: JPEG with 2 components is not supported");
}

#[test]
fn damaged_png_page_fails_and_leaves_no_file() {
    let t = tmp("damaged");
    let good = rgb_png(8, 8);
    // 8 rows are declared, 3 are present: the IDAT is shorter than expected.
    let short = testzip::png(8, 8, 2, None, false, false, &[0u8; 3 * (1 + 8 * 3)]);
    let input = cbz(
        &t,
        "d.cbz",
        &[(b"a.png", &good, Stored), (b"x.png", &short, Stored)],
    );
    let (o, f) = one_input(&t, "fail", &input);
    assert_eq!(f[2], "page x.png: damaged image data");
    assert_eq!(o.status.code(), Some(1));
}

#[test]
fn not_an_image_page_fails() {
    let t = tmp("notimage");
    let input = cbz(&t, "n.cbz", &[(b"x.jpg", b"this is text", Stored)]);
    let (_, f) = one_input(&t, "fail", &input);
    assert_eq!(f[2], "page x.jpg: not a JPEG or PNG");
}

#[test]
fn mismatched_extension_fails() {
    let t = tmp("mismatch");
    let jpg = jpeg_fixture("rgb.jpg");
    let input = cbz(&t, "e.cbz", &[(b"x.png", &jpg, Stored)]);
    let (_, f) = one_input(&t, "fail", &input);
    assert_eq!(f[2], "page x.png: content does not match its extension");
    let png = rgb_png(3, 3);
    let input = cbz(&t, "e2.cbz", &[(b"y.jpg", &png, Stored)]);
    let (_, f) = one_input(&t, "fail", &input);
    assert_eq!(f[2], "page y.jpg: content does not match its extension");
}

#[test]
fn corrupt_zip_data_fails_as_damaged_cbz() {
    let t = tmp("corrupt");
    let png = rgb_png(8, 8);
    let mut b = testzip::zip(&[(b"1.png", &png, Stored)]);
    b[30 + 5 + 20] ^= 0xFF; // a data byte: the stored CRC-32 no longer matches
    let input = t.join("c.cbz");
    fs::write(&input, b).unwrap();
    let (_, f) = one_input(&t, "fail", &input);
    assert_eq!(f[2], "damaged CBZ: the entry's CRC-32 does not match");
    let input = t.join("notzip.cbz");
    fs::write(&input, b"definitely not a zip file").unwrap();
    let (_, f) = one_input(&t, "fail", &input);
    assert_eq!(f[2], "damaged CBZ: not a ZIP archive");
}

#[test]
fn empty_cbz_is_skipped() {
    let t = tmp("empty");
    let input = cbz(&t, "e.cbz", &[]);
    let (o, f) = one_input(&t, "skip", &input);
    assert_eq!(f[2], "no pages");
    assert_eq!(o.status.code(), Some(3));
}

#[test]
fn cbz_with_only_comicinfo_is_skipped() {
    let t = tmp("comicinfo");
    let input = cbz(
        &t,
        "c.cbz",
        &[(b"ComicInfo.xml", b"<ComicInfo/>", Deflated)],
    );
    let (_, f) = one_input(&t, "skip", &input);
    assert_eq!(f[2], "no pages");
}

#[test]
fn non_image_entries_are_counted_once() {
    let t = tmp("nonimage");
    let png = rgb_png(5, 5);
    let input = cbz(
        &t,
        "w.cbz",
        &[
            (b"sub/", b"", Stored), // a folder entry does not count
            (b"ComicInfo.xml", b"<ComicInfo/>", Deflated),
            (b"sub/1.png", &png, Stored),
        ],
    );
    let work = t.join("work");
    let o = run(&work, &[&input]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert_eq!(
        lines(&out, "warn"),
        [vec![
            "warn",
            input.to_str().unwrap(),
            "1 non-image entries left out"
        ]]
    );
}

#[test]
fn orientation_and_profile_warnings_are_counted() {
    let t = tmp("warns");
    let exif = jpeg_fixture("exif_orientation6.jpg");
    let input = cbz(
        &t,
        "o.cbz",
        &[(b"1.jpg", &exif, Stored), (b"2.jpg", &exif, Stored)],
    );
    let o = run(&t.join("work"), &[&input]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert_eq!(
        lines(&out, "warn"),
        [vec![
            "warn",
            input.to_str().unwrap(),
            "orientation ignored on 2 pages"
        ]]
    );
}

/// Set the encryption flag in the local and central header of entry 0.
fn encrypt_flag(b: &mut [u8]) {
    b[6] |= 1;
    let c = b.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    b[c + 8] |= 1;
}

#[test]
fn encrypted_and_zip64_are_skipped() {
    let t = tmp("encrypted");
    let png = rgb_png(5, 5);
    let mut b = testzip::zip(&[(b"1.png", &png, Stored)]);
    encrypt_flag(&mut b);
    let input = t.join("enc.cbz");
    fs::write(&input, &b).unwrap();
    let (o, f) = one_input(&t, "skip", &input);
    assert_eq!(f[2], "encrypted CBZ");
    assert_eq!(o.status.code(), Some(3));

    // The end record claims 0xFFFF entries: the ZIP64 marker.
    let mut b = testzip::zip(&[(b"1.png", &png, Stored)]);
    let e = b.len() - 22;
    b[e + 10] = 0xFF;
    b[e + 11] = 0xFF;
    let input = t.join("z64.cbz");
    fs::write(&input, &b).unwrap();
    let (_, f) = one_input(&t, "skip", &input);
    assert_eq!(f[2], "ZIP64 CBZ is not supported");
}

#[test]
fn unsupported_method_is_skipped() {
    let t = tmp("method");
    let png = rgb_png(5, 5);
    let mut b = testzip::zip(&[(b"1.png", &png, Stored)]);
    b[8] = 99; // local header method
    let c = b.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    b[c + 10] = 99; // central header method
    let input = t.join("m.cbz");
    fs::write(&input, &b).unwrap();
    let (_, f) = one_input(&t, "skip", &input);
    assert_eq!(f[2], "page 1.png: compression method 99 is not supported");
}

#[test]
fn non_utf8_names_are_lossy_in_messages_and_sorted_by_bytes() {
    let t = tmp("cp437");
    let big = rgb_png(11, 11);
    let small = rgb_png(12, 12);
    let alpha = flat_png(4, 4, 6);
    let input = cbz(
        &t,
        "cp.cbz",
        &[(b"p\x82.png", &small, Stored), (b"p\x81.png", &big, Stored)],
    );
    let work = t.join("work");
    let o = run(&work, &[&input]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    if let Some(sizes) = page_sizes(&t, &work.join("0/cp.pdf"), 2) {
        assert_eq!(sizes, [(11, 11), (12, 12)]);
    }
    let t = tmp("cp437b");
    let input = cbz(&t, "cp2.cbz", &[(b"p\x82.png", &alpha, Stored)]);
    let (_, f) = one_input(&t, "skip", &input);
    assert_eq!(
        f[2],
        "page p\u{FFFD}.png: PNG with alpha needs the image family"
    );
}

// The limits (bins/inacircle-pages/src/main.rs, docs/resource-limits.md).
const ENTRIES: usize = 12_000;
const PAGES: usize = 10_000;

#[test]
fn limits_fail_with_their_copy() {
    let t = tmp("limits");
    let png = rgb_png(1, 1);

    // PAGES + 1 one-pixel pages.
    let names: Vec<String> = (0..=PAGES).map(|i| format!("{i:05}.png")).collect();
    let entries: Vec<(&[u8], &[u8], testzip::Store)> = names
        .iter()
        .map(|n| (n.as_bytes(), png.as_slice(), Stored))
        .collect();
    let input = cbz(&t, "pages.cbz", &entries);
    let (_, f) = one_input(&t, "fail", &input);
    assert_eq!(f[2], "CBZ has more than 10000 pages");

    // ENTRIES + 1 entries (none of them a page).
    let names: Vec<String> = (0..=ENTRIES).map(|i| format!("{i}.txt")).collect();
    let entries: Vec<(&[u8], &[u8], testzip::Store)> = names
        .iter()
        .map(|n| (n.as_bytes(), &b""[..], Stored))
        .collect();
    let input = cbz(&t, "entries.cbz", &entries);
    let (_, f) = one_input(&t, "fail", &input);
    assert_eq!(f[2], "CBZ has more than 12000 entries");

    // A page of 64 MiB + 1 (declared). Zeros deflate to 64 KB, so the
    // archive stays small.
    let zeros = vec![0u8; (64 << 20) + 1];
    let input = cbz(&t, "big.cbz", &[(b"1.png", &zeros, Deflated)]);
    let (_, f) = one_input(&t, "fail", &input);
    assert_eq!(f[2], "page 1.png is larger than 64 MiB");

    // 20000 x 10000 = 200 megapixels, over the 180 limit; the data is not
    // needed, the header decides.
    let huge = testzip::png(20_000, 10_000, 2, None, false, false, &[0u8; 8]);
    let input = cbz(&t, "mp.cbz", &[(b"1.png", &huge, Stored)]);
    let (_, f) = one_input(&t, "fail", &input);
    assert_eq!(f[2], "page 1.png is larger than 180 megapixels");

    // MAX_IDAT_CHUNKS + 1 data chunks: a valid 1x1 PNG whose IDAT is
    // followed by empty IDAT chunks (about 786 KB, well inside the page
    // limit). The copy names the real cause, not the pixel limit.
    let mut many = rgb_png(1, 1);
    many.truncate(many.len() - 12); // drop IEND
    let mut empty = 0u32.to_be_bytes().to_vec();
    empty.extend_from_slice(b"IDAT");
    empty.extend_from_slice(&testzip::crc32(b"IDAT").to_be_bytes());
    for _ in 0..probe::MAX_IDAT_CHUNKS {
        many.extend_from_slice(&empty);
    }
    many.extend_from_slice(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]);
    let input = cbz(&t, "chunks.cbz", &[(b"1.png", &many, Deflated)]);
    let (_, f) = one_input(&t, "fail", &input);
    assert_eq!(f[2], "page 1.png: PNG has more than 65536 data chunks");
}

/// Run with the child's `RLIMIT_FSIZE` set to `bytes` (soft and hard), the
/// way the engine's sandbox bounds an action's output, but small enough to
/// reach with little data.
fn run_with_fsize(work: &Path, input: &Path, bytes: u64) -> Output {
    use std::os::unix::process::CommandExt;
    let mut c = command(work);
    c.arg(input);
    // SAFETY: the closure runs between fork and exec and only calls
    // `setrlimit`, which is async-signal-safe, on a local struct; it
    // touches no memory of the parent and takes no lock.
    unsafe {
        c.pre_exec(move || {
            let rl = libc::rlimit {
                rlim_cur: bytes as libc::rlim_t,
                rlim_max: bytes as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &rl) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    c.output().unwrap()
}

#[test]
fn pages_over_the_output_size_limit_fail_with_their_copy() {
    let t = tmp("fsize");
    // Each page is an incompressible RGB PNG (pseudo-random rows, filter 0)
    // of about 750 KB, so one page fits in 1 MiB and two do not.
    let (w, h) = (500usize, 500usize);
    let mut x = 0x2545_F491u32;
    let mut rows = Vec::with_capacity(h * (1 + 3 * w));
    for _ in 0..h {
        rows.extend_from_slice(&[0]); // filter type 0
        for _ in 0..3 * w {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            rows.push((x >> 24) as u8);
        }
    }
    let png = testzip::png(w as u32, h as u32, 2, None, false, false, &rows);
    assert!(png.len() > 750_000 && png.len() < 800_000, "{}", png.len());
    let work = t.join("work");

    // Positive control: one page under the cap succeeds.
    let one = cbz(&t, "one.cbz", &[(b"1.png", &png, Stored)]);
    let o = run_with_fsize(&work, &one, 1 << 20);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(files_with_ext(&work, "pdf").len(), 1);
    fs::remove_dir_all(&work).unwrap();
    fs::create_dir_all(&work).unwrap();

    // Two pages: the declared sizes already exceed the cap.
    let two = cbz(
        &t,
        "two.cbz",
        &[(b"1.png", &png, Stored), (b"2.png", &png, Stored)],
    );
    let o = run_with_fsize(&work, &two, 1 << 20);
    assert_eq!(o.status.code(), Some(1));
    let out = stdout(&o);
    let f = lines(&out, "fail");
    assert_eq!(f.len(), 1, "{out}");
    assert_eq!(f[0][2], "PDF would be larger than 1 MiB");
    assert_no_pdf(&work);

    // The cap in MiB is rounded down.
    let o = run_with_fsize(&work, &two, (1 << 20) + 200_000);
    assert_eq!(
        lines(&stdout(&o), "fail")[0][2],
        "PDF would be larger than 1 MiB"
    );
    assert_no_pdf(&work);
}

#[test]
fn macos_metadata_entries_are_not_pages() {
    let t = tmp("macos");
    let png = rgb_png(5, 5);
    let input = cbz(
        &t,
        "m.cbz",
        &[
            (b"__MACOSX/", b"", Stored),
            (b"__MACOSX/._1.jpg", b"\x00\x05\x16\x07 not a jpeg", Stored),
            (b"__MACOSX/sub/2.png", b"junk", Stored),
            (b"._x.jpg", b"junk", Stored),
            (b"sub/.png", b"junk", Stored),
            (b"1.png", &png, Stored),
            (b"2.png", &png, Stored),
        ],
    );
    let work = t.join("work");
    let o = run(&work, &[&input]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(
        lines(&out, "warn"),
        [vec![
            "warn",
            input.to_str().unwrap(),
            "4 non-image entries left out"
        ]]
    );
    let pdf = files_with_ext(&work, "pdf");
    assert_eq!(pdf.len(), 1);
    page_sizes(&t, &pdf[0], 2);
}

#[test]
fn name_and_ratio_limits_fail_with_their_copy() {
    let t = tmp("zipshape");
    // A 513-byte name (the limit is 512).
    let long = format!("{}.png", "a".repeat(509));
    assert_eq!(long.len(), 513);
    let input = cbz(&t, "name.cbz", &[(long.as_bytes(), b"x", Stored)]);
    let (_, f) = one_input(&t, "fail", &input);
    assert_eq!(f[2], "damaged CBZ: an entry name is too long");

    // A deflated entry of 4 KiB whose header is forged to claim 1 GiB: the
    // ratio is far over 1032, and the header is refused before inflating.
    let png = rgb_png(5, 5);
    let mut z = testzip::zip(&[(b"1.png", &png, Deflated)]);
    let forged = (1u32 << 30).to_le_bytes();
    // Uncompressed size: offset 24 of the central directory header and 22
    // of the local header.
    let cd = z.windows(4).rposition(|w| w == b"PK\x01\x02").unwrap();
    z[cd + 24..cd + 28].copy_from_slice(&forged);
    z[22..26].copy_from_slice(&forged);
    let input = t.join("ratio.cbz");
    fs::write(&input, z).unwrap();
    let (_, f) = one_input(&t, "fail", &input);
    assert_eq!(
        f[2],
        "damaged CBZ: an entry's compression ratio is too high"
    );
}

#[test]
fn batch_rule() {
    let t = tmp("batch");
    let png = rgb_png(6, 6);
    let good = cbz(&t, "good.cbz", &[(b"1.png", &png, Stored)]);
    let empty = cbz(&t, "empty.cbz", &[]);
    let work = t.join("work");
    let o = run(&work, &[&good, &empty]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    assert_eq!(lines(&out, "done").len(), 1);
    assert_eq!(lines(&out, "skip").len(), 1);
    assert!(work.join("0/good.pdf").is_file());

    let work2 = t.join("work2");
    fs::create_dir_all(&work2).unwrap();
    let o = run(&work2, &[&empty, &empty]);
    assert_eq!(o.status.code(), Some(3));
    assert_eq!(lines(&stdout(&o), "skip").len(), 2);
    assert_no_pdf(&work2);
}

/// Peak memory of the child, sampled from `/proc/<pid>/status` (`VmHWM`,
/// the kernel's own high-water mark, which only grows) while the child
/// runs; the last sample before it exits is the answer. Not `wait4`'s
/// `ru_maxrss`: after `exec` the child's value starts from the parent's
/// (this test process's) peak, so it overstates. Not a single read at the
/// end: a zombie has no `VmHWM`.
fn peak_kb(child: &mut std::process::Child) -> (u64, String) {
    let pid = child.id();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut text = String::new();
    let mut peak = 0u64;
    let sample = |peak: &mut u64| {
        if let Ok(s) = fs::read_to_string(format!("/proc/{pid}/status"))
            && let Some(l) = s.lines().find(|l| l.starts_with("VmHWM:"))
        {
            let kb: u64 = l.split_whitespace().nth(1).unwrap().parse().unwrap();
            *peak = (*peak).max(kb);
        }
    };
    // One sample per line the child prints (a progress line per page), so
    // the sampling follows the work instead of a timer.
    let mut line = String::new();
    while out.read_line(&mut line).unwrap() > 0 {
        text.push_str(&line);
        line.clear();
        sample(&mut peak);
    }
    sample(&mut peak);
    (peak, text)
}

#[test]
fn many_pages_stay_within_memory() {
    let t = tmp("many");
    // 5000 pages of 64 x 64 RGB (a flat page, ~100 B each in the CBZ).
    let png = rgb_png(64, 64);
    let names: Vec<String> = (0..5000)
        .map(|i| format!("Vol {}/{i:04}.png", i / 500))
        .collect();
    let entries: Vec<(&[u8], &[u8], testzip::Store)> = names
        .iter()
        .map(|n| (n.as_bytes(), png.as_slice(), Stored))
        .collect();
    let input = cbz(&t, "many.cbz", &entries);
    let work = t.join("work");
    let mut child = command(&work)
        .arg(&input)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let (peak_kb, text) = peak_kb(&mut child);
    let status = child.wait().unwrap();
    assert!(status.success(), "{text}");
    assert_eq!(lines(&text, "done").len(), 1, "{text}");
    println!("many_pages_stay_within_memory: 5000 pages, VmHWM {peak_kb} kB");
    // Measured 3.7-4.0 MB (about 1.7 MB process baseline plus 2.2 MB for
    // 5000 entries, page order and PDF offsets; docs/resource-limits.md).
    // The 8 MB ceiling is twice that and still catches per-page retention
    // (5000 x 64 KiB would be 320 MB).
    assert!(peak_kb <= 8 * 1024, "VmHWM {peak_kb} kB");
    if let Some(s) = page_sizes(&t, &work.join("0/many.pdf"), 5000) {
        assert_eq!(s.len(), 5000);
    }
}

/// Release-mode timing of one large page (run by hand, see the task report):
/// `cargo test --release -p inacircle-pages -- --ignored --nocapture`.
#[test]
#[ignore = "timing probe, run by hand in release mode"]
fn large_page_timing() {
    let t = tmp("timing");
    // A ~42 MB page of incompressible pixels (3500 x 4000 RGB): the PNG
    // file is as large as its data, so the time is the ZIP read with its
    // CRC-32, the probe, the PNG data check and the PDF write.
    let (w, h) = (3500u32, 4000u32);
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut rows = Vec::with_capacity(h as usize * (1 + 3 * w as usize));
    for _ in 0..h {
        rows.extend_from_slice(&[0]); // filter type 0
        for _ in 0..3 * w {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            rows.push(x as u8);
        }
    }
    let page = testzip::png(w, h, 2, None, false, false, &rows);
    let big = cbz(&t, "big.cbz", &[(b"1.png", &page, Stored)]);
    let start = std::time::Instant::now();
    let mut child = command(&t.join("work"))
        .arg(&big)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let (peak, _) = peak_kb(&mut child);
    let status = child.wait().unwrap();
    println!(
        "large_page_timing: {} B page, exit {:?}, {:?}, VmHWM {peak} kB",
        page.len(),
        status.code(),
        start.elapsed()
    );
}

// ---------- boundaries, progress, cleanup ----------

/// The values of the `progress` events, in order.
fn progress_values(out: &str) -> Vec<String> {
    lines(out, "progress")
        .iter()
        .map(|f| f[1].to_string())
        .collect()
}

#[test]
fn exactly_the_page_limit_converts() {
    let t = tmp("pagelimit");
    let png = rgb_png(1, 1);
    let names: Vec<String> = (0..PAGES).map(|i| format!("{i:05}.png")).collect();
    let entries: Vec<(&[u8], &[u8], testzip::Store)> = names
        .iter()
        .map(|n| (n.as_bytes(), png.as_slice(), Stored))
        .collect();
    let input = cbz(&t, "limit.cbz", &entries);
    let o = run(&t.join("work"), &[&input]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{}", &out[..out.len().min(500)]);
    assert_eq!(lines(&out, "done").len(), 1);
}

#[test]
fn a_page_of_exactly_64_mib_passes_the_size_check() {
    // The size check is `>`: 64 MiB is allowed, so the page is read and
    // then refused for what it is (zeros are no image), not for its size.
    let t = tmp("pagebytes");
    let zeros = vec![0u8; 64 << 20];
    let input = cbz(&t, "edge.cbz", &[(b"1.png", &zeros, Deflated)]);
    let (_, f) = one_input(&t, "fail", &input);
    assert_eq!(f[2], "page 1.png: not a JPEG or PNG");
}

#[test]
fn the_output_ceiling_is_inclusive_and_checked_before_any_page() {
    // Overheads mirror FILE_OVERHEAD and PAGE_OVERHEAD in main.rs.
    let t = tmp("ceiling");
    let (w, h) = (100usize, 100usize);
    let mut x = 0x1357_9BDFu32;
    let mut rows = Vec::new();
    for _ in 0..h {
        rows.extend_from_slice(&[0]); // filter type 0
        for _ in 0..3 * w {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            rows.push((x >> 24) as u8);
        }
    }
    let png = testzip::png(w as u32, h as u32, 2, None, false, false, &rows);
    let input = cbz(&t, "one.cbz", &[(b"1.png", &png, Stored)]);
    let total = 1_024 + png.len() as u64 + 2_560;
    let work = t.join("work");

    // A ceiling equal to the estimate lets the page through.
    let o = run_with_fsize(&work, &input, total);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(files_with_ext(&work, "pdf").len(), 1);
    fs::remove_dir_all(&work).unwrap();
    fs::create_dir_all(&work).unwrap();

    // One byte less is refused from the directory alone: no page was read,
    // so there is no per-page progress.
    let o = run_with_fsize(&work, &input, total - 1);
    assert_eq!(o.status.code(), Some(1));
    let out = stdout(&o);
    let f = lines(&out, "fail");
    assert_eq!(f.len(), 1, "{out}");
    assert!(f[0][2].starts_with("PDF would be larger than"), "{out}");
    assert_eq!(progress_values(&out), ["0", "1"], "{out}");
    assert_no_pdf(&work);
}

#[test]
fn progress_counts_pages_within_inputs_and_inputs_within_the_batch() {
    let t = tmp("progress");
    let png = rgb_png(2, 2);
    let a = cbz(
        &t,
        "a.cbz",
        &[(b"1.png", &png, Stored), (b"2.png", &png, Stored)],
    );
    let b = cbz(
        &t,
        "b.cbz",
        &[(b"1.png", &png, Stored), (b"2.png", &png, Stored)],
    );
    let o = run(&t.join("work"), &[&a, &b]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(
        progress_values(&stdout(&o)),
        ["0", "0.25", "0.5", "0.5", "0.75", "1", "1"]
    );
}

/// A PNG with an `iCCP` chunk behind the header.
fn png_with_profile(w: u32, h: u32) -> Vec<u8> {
    let mut png = rgb_png(w, h);
    let data = b"icc\0\0profile";
    let mut body = b"iCCP".to_vec();
    body.extend_from_slice(data);
    let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
    chunk.extend_from_slice(&body);
    chunk.extend_from_slice(&testzip::crc32(&body).to_be_bytes());
    png.splice(33..33, chunk);
    png
}

#[test]
fn colour_profile_warning_is_counted() {
    let t = tmp("profile");
    let with = png_with_profile(3, 3);
    let without = rgb_png(3, 3);
    let input = cbz(
        &t,
        "p.cbz",
        &[
            (b"1.png", &with, Stored),
            (b"2.png", &with, Stored),
            (b"3.png", &without, Stored),
        ],
    );
    let o = run(&t.join("work"), &[&input]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(
        lines(&stdout(&o), "warn"),
        [vec![
            "warn",
            input.to_str().unwrap(),
            "colour profile not embedded on 2 pages"
        ]]
    );
}

#[test]
fn a_fifo_input_fails_without_blocking() {
    use std::os::unix::ffi::OsStrExt;
    let t = tmp("fifo");
    let fifo = t.join("pipe.cbz");
    let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let mut child = command(&t.join("work"))
        .arg(&fifo)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    // A blocking open would wait for a writer that never comes.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the input FIFO blocked the action");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1));
    let mut out = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut out).unwrap();
    let f = lines(&out, "fail");
    assert_eq!(f.len(), 1, "{out}");
    assert_eq!(f[0][2], "not a regular file");
}

// ---------- pages.images-to-pdf (join) ----------

fn join_command(work: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_inacircle-pages"));
    c.args(["run", "pages.images-to-pdf", "--work"])
        .arg(work)
        .arg("--");
    c
}

/// Write `data` as `name` in the test's `in` folder and return the path.
fn image_file(t: &Tmp, name: &str, data: &[u8]) -> PathBuf {
    fs::create_dir_all(t.join("in")).unwrap();
    let p = t.join("in").join(name);
    fs::write(&p, data).unwrap();
    p
}

fn run_join(work: &Path, inputs: &[PathBuf]) -> Output {
    join_command(work).args(inputs).output().unwrap()
}

fn no_join_output(work: &Path) {
    assert_no_pdf(work);
    assert!(!work.join("join/out.pdf").exists(), "output left in join/");
}

#[test]
fn images_to_pdf_joins_in_given_order() {
    let t = tmp("join-order");
    // The given order, not the name order: b before a.
    let ins = [
        image_file(&t, "b.jpg", &jpeg_fixture("rgb.jpg")), // 32 x 24
        image_file(&t, "a.png", &rgb_png(20, 20)),
        image_file(&t, "c.png", &rgb_png(50, 10)),
    ];
    let work = t.join("work");
    let o = run_join(&work, &ins);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.starts_with("progress\t0\n"), "{out}");
    let takes = lines(&out, "take");
    assert_eq!(takes.len(), 3, "{out}");
    for (tk, i) in takes.iter().zip(&ins) {
        assert_eq!(tk[1], i.to_str().unwrap());
    }
    let j = lines(&out, "joined");
    assert_eq!(j.len(), 1, "{out}");
    assert_eq!(j[0][1], "join/out.pdf");
    assert!(lines(&out, "done").is_empty(), "{out}");
    let pdf = work.join("join/out.pdf");
    assert!(fs::read(&pdf).unwrap().starts_with(b"%PDF-"));
    if let Some(sizes) = page_sizes(&t, &pdf, 3) {
        assert_eq!(sizes, [(32, 24), (20, 20), (50, 10)]);
    }
}

#[test]
fn unsupported_image_is_skipped_rest_joined() {
    let t = tmp("join-skip");
    let ins = [
        image_file(&t, "1.png", &rgb_png(20, 20)),
        image_file(&t, "2.png", &flat_png(8, 8, 6)),
        image_file(&t, "3.png", &rgb_png(50, 10)),
    ];
    let work = t.join("work");
    let o = run_join(&work, &ins);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    let s = lines(&out, "skip");
    assert_eq!(s.len(), 1, "{out}");
    assert_eq!(s[0][1], ins[1].to_str().unwrap());
    assert_eq!(s[0][2], "PNG with alpha needs the image family");
    assert_eq!(lines(&out, "take").len(), 2, "{out}");
    assert_eq!(lines(&out, "joined").len(), 1, "{out}");
    if let Some(sizes) = page_sizes(&t, &work.join("join/out.pdf"), 2) {
        assert_eq!(sizes, [(20, 20), (50, 10)]);
    }
}

#[test]
fn damaged_image_fails_rest_joined() {
    let t = tmp("join-damaged");
    let short = testzip::png(8, 8, 2, None, false, false, &[0u8; 3 * (1 + 8 * 3)]);
    let ins = [
        image_file(&t, "1.png", &rgb_png(20, 20)),
        image_file(&t, "2.png", &short),
        image_file(&t, "3.png", &rgb_png(50, 10)),
    ];
    let work = t.join("work");
    let o = run_join(&work, &ins);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    let f = lines(&out, "fail");
    assert_eq!(f.len(), 1, "{out}");
    assert_eq!(f[0][1], ins[1].to_str().unwrap());
    assert_eq!(f[0][2], "damaged image data");
    assert_eq!(lines(&out, "take").len(), 2, "{out}");
    assert_eq!(lines(&out, "joined").len(), 1, "{out}");
    if let Some(sizes) = page_sizes(&t, &work.join("join/out.pdf"), 2) {
        assert_eq!(sizes, [(20, 20), (50, 10)]);
    }
}

#[test]
fn nothing_acceptable_writes_nothing() {
    let t = tmp("join-none");
    let ins = [
        image_file(&t, "1.png", &flat_png(8, 8, 6)),
        image_file(&t, "2.png", &flat_png(9, 9, 6)),
    ];
    let work = t.join("work");
    let o = run_join(&work, &ins);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(3), "{out}");
    assert_eq!(lines(&out, "skip").len(), 2, "{out}");
    assert!(lines(&out, "take").is_empty(), "{out}");
    assert!(lines(&out, "joined").is_empty(), "{out}");
    no_join_output(&work);
}

#[test]
fn too_large_total_fails_whole() {
    let t = tmp("join-fsize");
    // Two incompressible ~750 KB PNGs: one fits in 1 MiB, two do not.
    let (w, h) = (500usize, 500usize);
    let mut x = 0x2545_F491u32;
    let mut rows = Vec::with_capacity(h * (1 + 3 * w));
    for _ in 0..h {
        rows.extend_from_slice(&[0]); // filter type 0
        for _ in 0..3 * w {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            rows.push((x >> 24) as u8);
        }
    }
    let png = testzip::png(w as u32, h as u32, 2, None, false, false, &rows);
    let a = image_file(&t, "a.png", &png);
    let b = image_file(&t, "b.png", &png);
    let work = t.join("work");
    let fsize = |ins: &[PathBuf]| {
        use std::os::unix::process::CommandExt;
        let mut c = join_command(&work);
        c.args(ins);
        // SAFETY: only `setrlimit` (async-signal-safe) on a local struct
        // runs between fork and exec.
        unsafe {
            c.pre_exec(|| {
                let rl = libc::rlimit {
                    rlim_cur: 1 << 20,
                    rlim_max: 1 << 20,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &rl) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        c.output().unwrap()
    };
    // Positive control: one image under the cap joins.
    let o = fsize(std::slice::from_ref(&a));
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(lines(&stdout(&o), "joined").len(), 1);
    fs::remove_dir_all(&work).unwrap();
    fs::create_dir_all(&work).unwrap();

    let o = fsize(&[a.clone(), b.clone()]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    let f = lines(&out, "fail");
    assert_eq!(f.len(), 2, "{out}");
    for (fl, i) in f.iter().zip([&a, &b]) {
        assert_eq!(fl[1], i.to_str().unwrap());
        assert_eq!(fl[2], "PDF would be larger than 1 MiB");
    }
    assert!(lines(&out, "take").is_empty(), "{out}");
    assert!(lines(&out, "joined").is_empty(), "{out}");
    no_join_output(&work);
}

/// `run_join` with the child's `RLIMIT_FSIZE` at `bytes`.
fn run_join_with_fsize(work: &Path, inputs: &[PathBuf], bytes: u64) -> Output {
    use std::os::unix::process::CommandExt;
    let mut c = join_command(work);
    c.args(inputs);
    // SAFETY: only `setrlimit` (async-signal-safe) on a local struct runs
    // between fork and exec.
    unsafe {
        c.pre_exec(move || {
            let rl = libc::rlimit {
                rlim_cur: bytes as libc::rlim_t,
                rlim_max: bytes as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &rl) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    c.output().unwrap()
}

#[test]
fn join_image_over_the_entry_limit_fails_and_a_missing_one_says_why() {
    let t = tmp("join-big");
    let big = image_file(&t, "big.png", b"");
    // Sparse: 64 MiB + 1 B on paper, nothing on disk.
    fs::OpenOptions::new()
        .write(true)
        .open(&big)
        .unwrap()
        .set_len((64 << 20) + 1)
        .unwrap();
    let gone = t.join("in/gone.png");
    let ok = image_file(&t, "ok.png", &rgb_png(20, 20));
    let ins = [big.clone(), gone.clone(), ok];
    let o = run_join(&t.join("work"), &ins);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    let f = lines(&out, "fail");
    assert_eq!(f.len(), 2, "{out}");
    assert_eq!(f[0][1], big.to_str().unwrap());
    assert_eq!(f[0][2], "image is larger than 64 MiB");
    assert_eq!(f[1][1], gone.to_str().unwrap());
    assert!(!f[1][2].contains("larger"), "{out}");
    assert_eq!(lines(&out, "take").len(), 1, "{out}");
}

#[test]
fn join_size_estimate_is_the_ceiling_exactly() {
    let t = tmp("join-estimate");
    let jpg = jpeg_fixture("rgb.jpg");
    let a = image_file(&t, "a.jpg", &jpg);
    let work = t.join("work");
    // The estimate of one image: file overhead + page overhead + its bytes
    // (FILE_OVERHEAD 1,024 and PAGE_OVERHEAD 2,560 in the action).
    let estimate = jpg.len() as u64 + 1_024 + 2_560;
    // A cap equal to the estimate lets it through.
    let o = run_join_with_fsize(&work, std::slice::from_ref(&a), estimate);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    fs::remove_dir_all(&work).unwrap();
    fs::create_dir_all(&work).unwrap();
    // A cap under the estimate refuses before writing, although the real
    // file (the JPEG plus a small structure) would still fit under it.
    let o = run_join_with_fsize(&work, std::slice::from_ref(&a), jpg.len() as u64 + 2_000);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert_eq!(lines(&out, "fail").len(), 1, "{out}");
    assert!(lines(&out, "joined").is_empty(), "{out}");
    no_join_output(&work);
}

#[test]
fn join_progress_counts_every_input_skipped_ones_included() {
    use std::os::unix::ffi::OsStrExt;
    let t = tmp("join-progress");
    let png = rgb_png(8, 8);
    let odd = image_file(&t, "x.png", &png);
    let odd_name = t.join("in").join(std::ffi::OsStr::from_bytes(b"\xff.png"));
    fs::rename(&odd, &odd_name).unwrap();
    let ins = [
        image_file(&t, "a.png", &png),
        odd_name,
        image_file(&t, "b.png", &png),
        image_file(&t, "c.png", &png),
    ];
    let o = run_join(&t.join("work"), &ins);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert_eq!(lines(&out, "skip").len(), 1, "{out}");
    assert_eq!(
        progress_values(&out),
        ["0", "0.25", "0.5", "0.75", "1"],
        "{out}"
    );
}

#[test]
fn many_images_stay_within_memory() {
    let t = tmp("join-many");
    let png = rgb_png(64, 64);
    let ins: Vec<PathBuf> = (0..1000)
        .map(|i| image_file(&t, &format!("{i:04}.png"), &png))
        .collect();
    let work = t.join("work");
    let mut child = join_command(&work)
        .args(&ins)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let (peak_kb, text) = peak_kb(&mut child);
    let status = child.wait().unwrap();
    assert!(status.success(), "{text}");
    assert_eq!(lines(&text, "take").len(), 1000, "{text}");
    assert_eq!(lines(&text, "joined").len(), 1, "{text}");
    println!("many_images_stay_within_memory: 1000 images, VmHWM {peak_kb} kB");
    // Ceiling 8 MB: the baseline plus one image plus ~100 B per page and
    // the accepted-input list (docs/resource-limits.md); per-image
    // retention (1000 x the image) would be far above.
    assert!(peak_kb <= 8 * 1024, "VmHWM {peak_kb} kB");
    if let Some(s) = page_sizes(&t, &work.join("join/out.pdf"), 1000) {
        assert_eq!(s.len(), 1000);
    }
}

// ---------- pages.images-to-cbz (join) ----------

fn cbz_command(work: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_inacircle-pages"));
    c.args(["run", "pages.images-to-cbz", "--work"])
        .arg(work)
        .arg("--");
    c
}

fn run_cbz(work: &Path, inputs: &[PathBuf]) -> Output {
    cbz_command(work).args(inputs).output().unwrap()
}

fn no_cbz_output(work: &Path) {
    assert!(
        files_with_ext(work, "cbz").is_empty(),
        "half-written CBZ left behind"
    );
    assert!(!work.join("join/out.cbz").exists(), "output left in join/");
}

/// Lists the archive with Python's `zipfile` (CRC-checked by `testzip`):
/// `names` prints one name per line, `hex` prints `name method hex`.
const LIST_PY: &str = r#"import sys, zipfile
z = zipfile.ZipFile(sys.argv[1])
assert z.testzip() is None
for i in z.infolist():
    if sys.argv[2] == "hex":
        print(i.filename, i.compress_type, z.read(i).hex())
    else:
        print(i.filename, i.compress_type, i.file_size)
"#;

/// An independent read of `archive`: `(name, method, data)` per entry in
/// order (`data` empty in `names` mode; then the size is in the third
/// field instead), after `unzip -t` accepted it. `None` when python3 or
/// unzip is missing (a skip, or a failure with
/// `INACIRCLE_REQUIRE_PDF_TOOLS=1`).
fn read_cbz(t: &Tmp, archive: &Path, hex: bool) -> Option<Vec<(String, String, String)>> {
    if !(have("python3") && have("unzip")) {
        return None;
    }
    let script = t.join("list.py");
    fs::write(&script, LIST_PY).unwrap();
    let mode = if hex { "hex" } else { "names" };
    let o = Command::new("python3")
        .arg("-I")
        .arg(&script)
        .arg(archive)
        .arg(mode)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "python zipfile: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let u = Command::new("unzip")
        .arg("-tq")
        .arg(archive)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        u.status.success(),
        "unzip -tq: {}{}",
        String::from_utf8_lossy(&u.stdout),
        String::from_utf8_lossy(&u.stderr)
    );
    Some(
        String::from_utf8(o.stdout)
            .unwrap()
            .lines()
            .map(|l| {
                let mut p = l.split(' ');
                let f = |p: &mut std::str::Split<'_, char>| p.next().unwrap().to_owned();
                (f(&mut p), f(&mut p), f(&mut p))
            })
            .collect(),
    )
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Assert the archive holds exactly `expected` (name, bytes) as stored
/// entries, read by independent tools.
fn assert_cbz(t: &Tmp, archive: &Path, expected: &[(String, &[u8])]) {
    let Some(got) = read_cbz(t, archive, true) else {
        return;
    };
    let want: Vec<(String, String, String)> = expected
        .iter()
        .map(|(n, d)| (n.clone(), "0".to_owned(), hex(d)))
        .collect();
    assert_eq!(got, want);
}

/// `png` with its bit depth rewritten (the IHDR CRC fixed): the header
/// check does not read pixel data, but the file stays a well-formed header.
fn with_depth(mut png: Vec<u8>, depth: u8) -> Vec<u8> {
    png[24] = depth;
    let crc = testzip::crc32(&png[12..29]);
    png[29..33].copy_from_slice(&crc.to_be_bytes());
    png
}

#[test]
fn cbz_joins_in_given_order_with_numbered_names() {
    let t = tmp("cbz-order");
    let jpg = jpeg_fixture("rgb.jpg");
    let (p1, p2) = (rgb_png(20, 20), rgb_png(50, 10));
    // The given order, not the name order.
    let ins = [
        image_file(&t, "z.jpg", &jpg),
        image_file(&t, "b.png", &p1),
        image_file(&t, "a.png", &p2),
    ];
    let work = t.join("work");
    let o = run_cbz(&work, &ins);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert!(out.starts_with("progress\t0\n"), "{out}");
    let takes = lines(&out, "take");
    assert_eq!(takes.len(), 3, "{out}");
    for (tk, i) in takes.iter().zip(&ins) {
        assert_eq!(tk[1], i.to_str().unwrap());
    }
    let j = lines(&out, "joined");
    assert_eq!(j.len(), 1, "{out}");
    assert_eq!(j[0][1], "join/out.cbz");
    assert!(lines(&out, "done").is_empty(), "{out}");
    assert!(lines(&out, "warn").is_empty(), "{out}");
    let want = [
        ("001.jpg".to_owned(), jpg.as_slice()),
        ("002.png".to_owned(), p1.as_slice()),
        ("003.png".to_owned(), p2.as_slice()),
    ];
    assert_cbz(&t, &work.join("join/out.cbz"), &want);
    // Independent of the tools: the stored bytes are the inputs' bytes.
    let raw = fs::read(work.join("join/out.cbz")).unwrap();
    assert!(raw.starts_with(b"PK\x03\x04"));
    for (_, d) in &want {
        assert!(raw.windows(d.len()).any(|w| w == *d));
    }
}

#[test]
fn cbz_keeps_bytes_of_every_variant() {
    let t = tmp("cbz-variants");
    let rows = |w: usize, bpp: usize, h: usize| vec![0u8; h * (1 + w * bpp)];
    let imgs: Vec<(&str, Vec<u8>)> = vec![
        ("alpha", flat_png(6, 4, 6)),
        (
            "16-bit",
            with_depth(
                testzip::png(5, 5, 2, None, false, false, &rows(5, 3, 5)),
                16,
            ),
        ),
        (
            "interlaced",
            testzip::png(7, 5, 2, None, false, true, &rows(7, 3, 5)),
        ),
        ("cmyk", jpeg_fixture("cmyk_adobe.jpg")),
        ("12-bit", jpeg_fixture("precision12.jpg")),
        ("arithmetic", jpeg_fixture("arithmetic.jpg")),
        ("progressive", jpeg_fixture("progressive.jpg")),
        ("exif", jpeg_fixture("exif_orientation6.jpg")),
    ];
    let ins: Vec<PathBuf> = imgs
        .iter()
        .enumerate()
        .map(|(i, (n, d))| image_file(&t, &format!("{i}-{n}.bin"), d))
        .collect();
    let work = t.join("work");
    let o = run_cbz(&work, &ins);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert_eq!(lines(&out, "take").len(), imgs.len(), "{out}");
    assert!(lines(&out, "warn").is_empty(), "{out}");
    assert!(lines(&out, "fail").is_empty(), "{out}");
    assert!(lines(&out, "skip").is_empty(), "{out}");
    let want: Vec<(String, &[u8])> = imgs
        .iter()
        .enumerate()
        .map(|(i, (_, d))| {
            let ext = if d.starts_with(b"\x89PNG") {
                "png"
            } else {
                "jpg"
            };
            (format!("{:03}.{ext}", i + 1), d.as_slice())
        })
        .collect();
    assert_cbz(&t, &work.join("join/out.cbz"), &want);
}

#[test]
fn name_width_follows_input_count() {
    let t = tmp("cbz-width");
    let png = rgb_png(4, 4);
    let mut ins: Vec<PathBuf> = (0..990)
        .map(|i| image_file(&t, &format!("{i:04}.png"), &png))
        .collect();
    for i in 0..10 {
        ins.push(image_file(&t, &format!("n{i}.txt"), b"not an image"));
    }
    let work = t.join("work");
    let o = run_cbz(&work, &ins);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    let f = lines(&out, "fail");
    assert_eq!(f.len(), 10, "{out}");
    assert!(f.iter().all(|l| l[2] == "not a JPEG or PNG"), "{out}");
    assert_eq!(lines(&out, "take").len(), 990, "{out}");
    if let Some(got) = read_cbz(&t, &work.join("join/out.cbz"), false) {
        let names: Vec<&str> = got.iter().map(|e| e.0.as_str()).collect();
        let want: Vec<String> = (1..=990).map(|i| format!("{i:04}.png")).collect();
        assert_eq!(names, want.iter().map(String::as_str).collect::<Vec<_>>());
    }
}

#[test]
fn bad_inputs_fail_rest_joined() {
    let t = tmp("cbz-bad");
    let good = rgb_png(8, 8);
    let truncated = rgb_png(8, 8)[..20].to_vec();
    let huge = testzip::png(20_000, 10_000, 2, None, false, false, &[0u8; 8]);
    let big = image_file(&t, "big.png", b"");
    // Sparse: 64 MiB + 1 B on paper, nothing on disk.
    fs::OpenOptions::new()
        .write(true)
        .open(&big)
        .unwrap()
        .set_len((64 << 20) + 1)
        .unwrap();
    let ins = [
        image_file(&t, "a.png", &good),
        image_file(&t, "trunc.png", &truncated),
        image_file(&t, "huge.png", &huge),
        big,
        image_file(&t, "text.png", b"hello"),
        image_file(&t, "b.png", &good),
    ];
    let work = t.join("work");
    let o = run_cbz(&work, &ins);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    let f = lines(&out, "fail");
    let reasons: Vec<&str> = f.iter().map(|l| l[2]).collect();
    assert_eq!(
        reasons,
        [
            "damaged image data",
            "image is larger than 180 megapixels",
            "image is larger than 64 MiB",
            "not a JPEG or PNG",
        ],
        "{out}"
    );
    let takes = lines(&out, "take");
    assert_eq!(takes.len(), 2, "{out}");
    assert_eq!(takes[0][1], ins[0].to_str().unwrap());
    assert_eq!(takes[1][1], ins[5].to_str().unwrap());
    // No hole in the numbering.
    assert_cbz(
        &t,
        &work.join("join/out.cbz"),
        &[
            ("001.png".to_owned(), good.as_slice()),
            ("002.png".to_owned(), good.as_slice()),
        ],
    );
}

#[test]
fn a_fifo_input_fails_without_blocking_and_the_rest_joins() {
    let t = tmp("cbz-fifo");
    let fifo = t.join("in/pipe.png");
    fs::create_dir_all(t.join("in")).unwrap();
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path for the whole call.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let good = image_file(&t, "a.png", &rgb_png(8, 8));
    let work = t.join("work");
    // A deadline: a regression that blocks on the FIFO is killed here and
    // fails the test instead of hanging the run. 20 s is far above the few
    // milliseconds a correct run takes.
    let child = cbz_command(&work)
        .args([&fifo, &good])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id() as libc::pid_t;
    let (done, wait) = std::sync::mpsc::channel::<()>();
    let dog = std::thread::spawn(move || {
        if wait
            .recv_timeout(std::time::Duration::from_secs(20))
            .is_err()
        {
            // SAFETY: plain kill(2) on the child of this test, which is
            // still unreaped here (the main thread waits on it).
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    });
    let o = child.wait_with_output().unwrap();
    let _ = done.send(());
    dog.join().unwrap();
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "blocked on the FIFO? {out}");
    let f = lines(&out, "fail");
    assert_eq!(f.len(), 1, "{out}");
    assert_eq!(f[0][1], fifo.to_str().unwrap());
    assert_eq!(lines(&out, "take").len(), 1, "{out}");
}

#[test]
fn nothing_acceptable_writes_no_cbz() {
    let t = tmp("cbz-none");
    let ins = [
        image_file(&t, "1.txt", b"one"),
        image_file(&t, "2.txt", b"two"),
    ];
    let work = t.join("work");
    let o = run_cbz(&work, &ins);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    let f = lines(&out, "fail");
    assert_eq!(f.len(), 2, "{out}");
    assert!(f.iter().all(|l| l[2] == "not a JPEG or PNG"), "{out}");
    assert!(lines(&out, "take").is_empty(), "{out}");
    assert!(lines(&out, "joined").is_empty(), "{out}");
    no_cbz_output(&work);
    assert!(!work.join("join").exists(), "join/ created for nothing");
}

#[test]
fn cbz_too_large_total_fails_whole() {
    let t = tmp("cbz-fsize");
    // Two incompressible ~750 KB PNGs: one fits in 1 MiB, two do not.
    let (w, h) = (500usize, 500usize);
    let mut x = 0x2545_F491u32;
    let mut rows = Vec::with_capacity(h * (1 + 3 * w));
    for _ in 0..h {
        rows.extend_from_slice(&[0]); // filter type 0
        for _ in 0..3 * w {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            rows.push((x >> 24) as u8);
        }
    }
    let png = testzip::png(w as u32, h as u32, 2, None, false, false, &rows);
    let a = image_file(&t, "a.png", &png);
    let b = image_file(&t, "b.png", &png);
    let work = t.join("work");
    let with_cap = |ins: &[PathBuf]| {
        use std::os::unix::process::CommandExt;
        let mut c = cbz_command(&work);
        c.args(ins);
        // SAFETY: only `setrlimit` (async-signal-safe) on a local struct
        // runs between fork and exec.
        unsafe {
            c.pre_exec(|| {
                let rl = libc::rlimit {
                    rlim_cur: 1 << 20,
                    rlim_max: 1 << 20,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &rl) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        c.output().unwrap()
    };
    // Positive control: one image under the cap joins.
    let o = with_cap(std::slice::from_ref(&a));
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(lines(&stdout(&o), "joined").len(), 1);
    fs::remove_dir_all(&work).unwrap();
    fs::create_dir_all(&work).unwrap();

    let o = with_cap(&[a.clone(), b.clone()]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    let f = lines(&out, "fail");
    assert_eq!(f.len(), 2, "{out}");
    for (fl, i) in f.iter().zip([&a, &b]) {
        assert_eq!(fl[1], i.to_str().unwrap());
        assert_eq!(fl[2], "CBZ would be larger than 1 MiB");
    }
    assert!(lines(&out, "take").is_empty(), "{out}");
    assert!(lines(&out, "joined").is_empty(), "{out}");
    no_cbz_output(&work);
}

#[test]
fn cbz_reads_back_as_pdf() {
    let t = tmp("cbz-pdf");
    let ins = [
        image_file(&t, "a.jpg", &jpeg_fixture("rgb.jpg")), // 32 x 24
        image_file(&t, "b.png", &rgb_png(20, 20)),
        image_file(&t, "c.png", &flat_png(50, 10, 0)),
    ];
    let work = t.join("work");
    let o = run_cbz(&work, &ins);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let archive = work.join("join/out.cbz");
    let work2 = t.join("work2");
    fs::create_dir_all(&work2).unwrap();
    let o = run(&work2, &[&archive]);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert_eq!(lines(&out, "done").len(), 1, "{out}");
    let pdf = work2.join("0/out.pdf");
    assert!(fs::read(&pdf).unwrap().starts_with(b"%PDF-"));
    if let Some(sizes) = page_sizes(&t, &pdf, 3) {
        assert_eq!(sizes, [(32, 24), (20, 20), (50, 10)]);
    }
}

#[test]
fn many_large_images_stay_within_memory() {
    let t = tmp("cbz-many");
    // 1024 sparse 128 KiB files that each start with a valid PNG header: a
    // 128 MiB archive, small enough for a RAM-backed temp dir.
    let png = rgb_png(64, 64);
    let ins: Vec<PathBuf> = (0..1024)
        .map(|i| {
            let p = image_file(&t, &format!("{i:04}.png"), &png);
            fs::OpenOptions::new()
                .write(true)
                .open(&p)
                .unwrap()
                .set_len(128 << 10)
                .unwrap();
            p
        })
        .collect();
    let work = t.join("work");
    let mut child = cbz_command(&work)
        .args(&ins)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let (peak_kb, text) = peak_kb(&mut child);
    let status = child.wait().unwrap();
    assert!(status.success(), "{text}");
    assert_eq!(lines(&text, "take").len(), 1024, "{text}");
    assert_eq!(lines(&text, "joined").len(), 1, "{text}");
    println!("many_large_images_stay_within_memory: 1024 x 128 KiB, VmHWM {peak_kb} kB");
    assert!(peak_kb <= 6 * 1024, "VmHWM {peak_kb} kB");
    if let Some(got) = read_cbz(&t, &work.join("join/out.cbz"), false) {
        assert_eq!(got.len(), 1024);
        assert!(got.iter().all(|e| e.1 == "0" && e.2 == "131072"));
    }
}
