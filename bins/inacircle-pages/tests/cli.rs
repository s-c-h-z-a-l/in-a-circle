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
action\tpages.cbz-to-pdf\tfamily=pages\tin=application/vnd.comicbook+zip\tout=application/pdf\text=pdf\tlabel=PDF\tgroup=book\tcost=10\tlossy=no\tbatch=yes\n";

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
fn manifest_line_is_exact() {
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
