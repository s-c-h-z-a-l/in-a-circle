//! The EPUB reader: container, package, metadata, spine, paths, limits.
//! Books are built here with `testzip` (independent of the crate's own ZIP
//! code), written to the cargo test temp dir and unlinked once open.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};

use epub::read::{BookError, ChapterError, MAX_MEDIA_TYPE, MAX_PATH, open};
use testzip::Store::{self, Deflated, Stored};

const MIB: usize = 1024 * 1024;

/// `bytes` as an open file (the path is removed at once; the descriptor
/// keeps the data).
fn file_of(bytes: &[u8]) -> File {
    static N: AtomicU32 = AtomicU32::new(0);
    let path = format!(
        "{}/epub-read-{}-{}.epub",
        env!("CARGO_TARGET_TMPDIR"),
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    f.write_all(bytes).unwrap();
    std::fs::remove_file(&path).unwrap();
    f
}

/// A ZIP of `(name, data, method)` entries as an open file.
fn zip_file(entries: &[(&str, &[u8], Store)]) -> File {
    file_of(&zip_bytes(entries))
}

fn zip_bytes(entries: &[(&str, &[u8], Store)]) -> Vec<u8> {
    let e: Vec<(&[u8], &[u8], Store)> = entries
        .iter()
        .map(|(n, d, s)| (n.as_bytes(), *d, *s))
        .collect();
    testzip::zip(&e)
}

fn container(opf_path: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="{opf_path}" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#
    )
}

/// An EPUB 3 package document with this metadata, manifest and spine body.
fn package(meta: &str, manifest: &str, spine: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">{meta}</metadata>
  <manifest>{manifest}</manifest>
  <spine>{spine}</spine>
</package>"#
    )
}

fn item(id: &str, href: &str) -> String {
    format!(r#"<item id="{id}" href="{href}" media-type="application/xhtml+xml"/>"#)
}

fn itemref(id: &str) -> String {
    format!(r#"<itemref idref="{id}"/>"#)
}

fn xhtml(text: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>t</title></head><body><p>{text}</p></body></html>"#
    )
    .into_bytes()
}

/// A book with its package at `OEBPS/content.opf`, chapters `c1..cN` at
/// `OEBPS/cN.xhtml`, in spine order, plus `extra` entries.
fn simple(n: usize, extra: &[(&str, &[u8], Store)]) -> File {
    let manifest: String = (1..=n)
        .map(|i| item(&format!("c{i}"), &format!("c{i}.xhtml")))
        .collect();
    let spine: String = (1..=n).map(|i| itemref(&format!("c{i}"))).collect();
    let opf = package("", &manifest, &spine);
    let cont = container("OEBPS/content.opf");
    let bodies: Vec<(String, Vec<u8>)> = (1..=n)
        .map(|i| (format!("OEBPS/c{i}.xhtml"), xhtml(&format!("chapter {i}"))))
        .collect();
    let mut entries: Vec<(&str, &[u8], Store)> = vec![
        ("mimetype", b"application/epub+zip", Stored),
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("OEBPS/content.opf", opf.as_bytes(), Stored),
    ];
    for (n, b) in &bodies {
        entries.push((n, b, Stored));
    }
    entries.extend_from_slice(extra);
    zip_file(&entries)
}

/// Every chapter's bytes, or its error, in spine order.
fn read_all(book: &epub::read::Book<'_>) -> Vec<Result<Vec<u8>, ChapterError>> {
    book.chapters
        .iter()
        .map(|c| {
            let mut v = Vec::new();
            book.chapter(c.index, &mut v).map(|()| v)
        })
        .collect()
}

/// The path of every spine item, in spine order (empty for none).
fn paths_of<'b>(book: &'b epub::read::Book<'_>) -> Vec<&'b str> {
    book.chapters.iter().map(|c| path(book, c.index)).collect()
}

/// The path of spine item `n` (1-based; empty when it names no item).
fn path<'b>(book: &'b epub::read::Book<'_>, n: usize) -> &'b str {
    book.item(&book.chapters[n - 1])
        .map_or("", |i| i.path.as_str())
}

fn err(r: Result<epub::read::Book<'_>, BookError>) -> BookError {
    match r {
        Ok(_) => panic!("the book opened"),
        Err(e) => e,
    }
}

#[test]
fn reads_epub3_and_epub2_spines() {
    // EPUB 3.
    let f = simple(3, &[]);
    let book = open(&f).unwrap();
    let paths = paths_of(&book);
    assert_eq!(
        paths,
        ["OEBPS/c1.xhtml", "OEBPS/c2.xhtml", "OEBPS/c3.xhtml"]
    );
    let idx: Vec<_> = book.chapters.iter().map(|c| c.index).collect();
    assert_eq!(idx, [1, 2, 3]);
    assert!(book.chapters.iter().all(|c| c.linear));
    assert!(
        book.chapters
            .iter()
            .all(|c| book.item(c).unwrap().media_type == "application/xhtml+xml")
    );
    let got = read_all(&book);
    assert_eq!(got[1].as_ref().unwrap(), &xhtml("chapter 2"));

    // EPUB 2: OPF 2.0 with an NCX in the manifest and `toc` on the spine,
    // `dc:` prefixed metadata, the spine order unlike the manifest order.
    let opf = r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:title>Old Book</dc:title>
    <dc:creator opf:role="aut">A. Author</dc:creator>
  </metadata>
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="b" href="b.html" media-type="application/xhtml+xml"/>
    <item id="a" href="a.html" media-type="application/xhtml+xml"/>
  </manifest>
  <spine toc="ncx"><itemref idref="a"/><itemref idref="b"/></spine>
  <guide><reference type="cover" href="a.html" title="Cover"/></guide>
</package>"#;
    let cont = container("content.opf");
    let (a, b) = (xhtml("A"), xhtml("B"));
    let f = zip_file(&[
        ("mimetype", b"application/epub+zip", Stored),
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        ("toc.ncx", b"<ncx/>", Stored),
        ("a.html", &a, Deflated),
        ("b.html", &b, Stored),
    ]);
    let book = open(&f).unwrap();
    let paths = paths_of(&book);
    assert_eq!(paths, ["a.html", "b.html"]);
    assert_eq!(book.title.as_deref(), Some("Old Book"));
    assert_eq!(book.authors, ["A. Author"]);
    let got = read_all(&book);
    assert_eq!(got[0].as_ref().unwrap(), &a);
    assert_eq!(got[1].as_ref().unwrap(), &b);
}

#[test]
fn chapter_buffer_is_replaced_and_index_checked() {
    let f = simple(2, &[]);
    let book = open(&f).unwrap();
    let mut v = b"left over".to_vec();
    book.chapter(1, &mut v).unwrap();
    assert_eq!(v, xhtml("chapter 1"));
    // Indexes are 1-based like `ChapterRef::index`; 0 and past the end are
    // not chapters.
    let mut v = b"left over".to_vec();
    assert_eq!(book.chapter(0, &mut v), Err(ChapterError::Missing));
    assert!(v.is_empty());
    assert_eq!(book.chapter(3, &mut v), Err(ChapterError::Missing));
    // The same chapter reads again (nothing is consumed).
    book.chapter(2, &mut v).unwrap();
    book.chapter(2, &mut v).unwrap();
    assert_eq!(v, xhtml("chapter 2"));
}

#[test]
fn metadata_title_and_authors() {
    let meta = r#"
      <dc:title>  </dc:title>
      <dc:title>The
        Real   Title</dc:title>
      <dc:title>Subtitle</dc:title>
      <dc:creator>First Author</dc:creator>
      <dc:creator></dc:creator>
      <dc:creator id="c2">Second &amp; Author</dc:creator>
      <dc:contributor>Not An Author</dc:contributor>
      <title>no namespace, not dc</title>"#;
    let opf = package(meta, &item("c1", "c1.xhtml"), &itemref("c1"));
    let cont = container("OEBPS/content.opf");
    let c1 = xhtml("x");
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("OEBPS/content.opf", opf.as_bytes(), Stored),
        ("OEBPS/c1.xhtml", &c1, Stored),
    ]);
    let book = open(&f).unwrap();
    assert_eq!(book.title.as_deref(), Some("The Real Title"));
    assert_eq!(book.authors, ["First Author", "Second & Author"]);

    // Neither: both absent.
    let f = simple(1, &[]);
    let book = open(&f).unwrap();
    assert_eq!(book.title, None);
    assert!(book.authors.is_empty());
}

#[test]
fn linear_no_kept_in_place() {
    let manifest = [
        item("a", "a.xhtml"),
        item("b", "b.xhtml"),
        item("c", "c.xhtml"),
    ]
    .concat();
    let spine = r#"<itemref idref="a"/><itemref idref="b" linear="no"/><itemref idref="c" linear="yes"/><itemref idref="a"/>"#;
    let opf = package("", &manifest, spine);
    let cont = container("content.opf");
    let (a, b, c) = (xhtml("a"), xhtml("b"), xhtml("c"));
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        ("a.xhtml", &a, Stored),
        ("b.xhtml", &b, Stored),
        ("c.xhtml", &c, Stored),
    ]);
    let book = open(&f).unwrap();
    let got: Vec<_> = book
        .chapters
        .iter()
        .map(|c| (c.index, path(&book, c.index), c.linear))
        .collect();
    // Every itemref in spine order; the repeated one is there twice.
    assert_eq!(
        got,
        [
            (1, "a.xhtml", true),
            (2, "b.xhtml", false),
            (3, "c.xhtml", true),
            (4, "a.xhtml", true)
        ]
    );
    let all = read_all(&book);
    assert_eq!(all[1].as_ref().unwrap(), &b);
    assert_eq!(all[3].as_ref().unwrap(), &a);
}

#[test]
fn hrefs_relative_to_opf_dir() {
    // Calibre's layout: the package in OEBPS/, chapters under Text/ with a
    // percent-encoded space, a fragment, and a `./` and `..` detour.
    let manifest = [
        item("c1", "Text/Chapter%201.xhtml"),
        item("c2", "Text/../Text/./Chapter2.xhtml#part"),
        item("css", "Styles/style.css").replace("application/xhtml+xml", "text/css"),
    ]
    .concat();
    let spine = [itemref("c1"), itemref("c2")].concat();
    let opf = package("", &manifest, &spine);
    let cont = container("OEBPS/content.opf");
    let (c1, c2) = (
        b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><img src=\"../Images/a.jpg\" alt=\"A\"/></body></html>".to_vec(),
        xhtml("two"),
    );
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("OEBPS/content.opf", opf.as_bytes(), Stored),
        ("OEBPS/Text/Chapter 1.xhtml", &c1, Stored),
        ("OEBPS/Text/Chapter2.xhtml", &c2, Stored),
        ("OEBPS/Images/a.jpg", b"\xFF\xD8\xFF", Stored),
        ("OEBPS/Styles/style.css", b"p{}", Stored),
    ]);
    let book = open(&f).unwrap();
    let paths = paths_of(&book);
    assert_eq!(
        paths,
        ["OEBPS/Text/Chapter 1.xhtml", "OEBPS/Text/Chapter2.xhtml"]
    );
    let all = read_all(&book);
    assert_eq!(all[0].as_ref().unwrap(), &c1);
    assert_eq!(all[1].as_ref().unwrap(), &c2);

    // The package at the root: hrefs are relative to the root.
    let f = simple(1, &[]);
    assert_eq!(path(&open(&f).unwrap(), 1), "OEBPS/c1.xhtml");
}

#[test]
fn href_case_mismatch_is_missing() {
    let manifest = [item("c1", "Chapter1.xhtml"), item("c2", "c2.xhtml")].concat();
    let spine = [itemref("c1"), itemref("c2"), itemref("nope")].concat();
    let opf = package("", &manifest, &spine);
    let cont = container("content.opf");
    let (c1, c2) = (xhtml("1"), xhtml("2"));
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        ("chapter1.xhtml", &c1, Stored),
        ("c2.xhtml", &c2, Stored),
    ]);
    let book = open(&f).unwrap();
    let all = read_all(&book);
    // The ZIP holds `chapter1.xhtml`; `Chapter1.xhtml` is not it.
    assert_eq!(all[0], Err(ChapterError::Missing));
    assert_eq!(all[1].as_ref().unwrap(), &c2);
    // An idref the manifest does not have is missing too.
    assert_eq!(all[2], Err(ChapterError::Missing));
    assert_eq!(ChapterError::Missing.reason(), "missing");
}

#[test]
fn missing_container_or_package_or_spine() {
    let c1 = xhtml("1");
    let opf = package("", &item("c1", "c1.xhtml"), &itemref("c1"));
    let cont = container("content.opf");

    // No container.xml (and one with the wrong case is not it).
    let f = zip_file(&[
        ("META-INF/Container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        ("c1.xhtml", &c1, Stored),
    ]);
    let e = err(open(&f));
    assert!(matches!(e, BookError::NoContainer));
    assert_eq!(e.to_string(), "not an EPUB (no META-INF/container.xml)");

    // A container whose rootfile names a missing package; one with no
    // package rootfile at all; one whose rootfile has another media type.
    for cont in [
        container("missing.opf"),
        r#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles/></container>"#.to_string(),
        container("content.opf").replace("application/oebps-package+xml", "application/pdf"),
    ] {
        let f = zip_file(&[
            ("META-INF/container.xml", cont.as_bytes(), Stored),
            ("content.opf", opf.as_bytes(), Stored),
            ("c1.xhtml", &c1, Stored),
        ]);
        let e = err(open(&f));
        assert!(matches!(e, BookError::NoPackage), "{cont}: {e:?}");
        assert_eq!(e.to_string(), "not an EPUB (no package document)");
    }

    // The first package rootfile counts, after one of another type.
    let cont2 = r#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles>
      <rootfile full-path="x.pdf" media-type="application/pdf"/>
      <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
      <rootfile full-path="other.opf" media-type="application/oebps-package+xml"/>
      </rootfiles></container>"#;
    let f = zip_file(&[
        ("META-INF/container.xml", cont2.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        ("c1.xhtml", &c1, Stored),
    ]);
    assert_eq!(open(&f).unwrap().chapters.len(), 1);

    // No spine element.
    let nospine = opf.replace("<spine>", "<x>").replace("</spine>", "</x>");
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", nospine.as_bytes(), Stored),
        ("c1.xhtml", &c1, Stored),
    ]);
    let e = err(open(&f));
    assert!(matches!(e, BookError::NoSpine));
    assert_eq!(e.to_string(), "book has no spine");

    // Not well-formed: the container and the package, each naming itself.
    let f = zip_file(&[
        (
            "META-INF/container.xml",
            b"<container>\n<rootfiles>",
            Stored,
        ),
        ("content.opf", opf.as_bytes(), Stored),
    ]);
    let e = err(open(&f));
    assert!(matches!(e, BookError::PackageXml { .. }));
    assert!(
        e.to_string()
            .starts_with("not an EPUB (not well-formed XML (line ")
            && e.to_string().ends_with(" in META-INF/container.xml)"),
        "{e}"
    );
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", b"<package><spine></package>", Stored),
    ]);
    let e = err(open(&f));
    assert!(e.to_string().ends_with(" in content.opf)"), "{e}");
    // A declared entity in the package is refused, never expanded.
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        (
            "content.opf",
            b"<!DOCTYPE p [<!ENTITY e \"x\">]><package>&e;</package>",
            Stored,
        ),
    ]);
    assert_eq!(
        err(open(&f)).to_string(),
        "not an EPUB (entity declarations are not supported in content.opf)"
    );

    // Not a ZIP at all.
    let f = file_of(b"just some text, not an archive");
    assert_eq!(err(open(&f)).to_string(), "damaged EPUB: not a ZIP archive");
}

#[test]
fn empty_spine_is_no_spine() {
    let opf = package("", &item("c1", "c1.xhtml"), "");
    let cont = container("content.opf");
    let c1 = xhtml("1");
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        ("c1.xhtml", &c1, Stored),
    ]);
    assert!(matches!(err(open(&f)), BookError::NoSpine));
}

fn spine_of(n: usize) -> File {
    let spine = itemref("c").repeat(n);
    let opf = package("", &item("c", "c.xhtml"), &spine);
    let cont = container("content.opf");
    let c = xhtml("c");
    zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Deflated),
        ("c.xhtml", &c, Stored),
    ])
}

#[test]
fn ten_thousand_and_one_chapters() {
    let f = spine_of(10_000);
    let book = open(&f).unwrap();
    assert_eq!(book.chapters.len(), 10_000);
    assert_eq!(book.chapters[9_999].index, 10_000);
    let f = spine_of(10_001);
    let e = err(open(&f));
    assert!(matches!(e, BookError::TooManyChapters));
    assert_eq!(e.to_string(), "book has more than 10000 chapters");
}

#[test]
fn package_document_too_large() {
    let cont = container("content.opf");
    let base = package("", &item("c1", "c1.xhtml"), &itemref("c1"));
    let c1 = xhtml("1");
    // Padded with a comment to exactly 8 MiB (opens) and one byte more.
    let pad = |len: usize| {
        let mut s = base.clone();
        let fill = len - s.len() - "<!---->".len();
        s.push_str(&format!("<!--{}-->", "x".repeat(fill)));
        assert_eq!(s.len(), len);
        s
    };
    for (len, ok) in [(8 * MIB, true), (8 * MIB + 1, false)] {
        let opf = pad(len);
        let f = zip_file(&[
            ("META-INF/container.xml", cont.as_bytes(), Stored),
            ("content.opf", opf.as_bytes(), Deflated),
            ("c1.xhtml", &c1, Stored),
        ]);
        match open(&f) {
            Ok(_) => assert!(ok, "{len}"),
            Err(e) => {
                assert!(!ok, "{len}: {e}");
                assert!(matches!(e, BookError::PackageTooLarge));
                assert_eq!(e.to_string(), "not an EPUB (package document too large)");
            }
        }
    }
    // The container too.
    let mut big = container("content.opf");
    big.push_str(&format!("<!--{}-->", "x".repeat(8 * MIB)));
    let f = zip_file(&[
        ("META-INF/container.xml", big.as_bytes(), Deflated),
        ("content.opf", base.as_bytes(), Stored),
        ("c1.xhtml", &c1, Stored),
    ]);
    assert!(matches!(err(open(&f)), BookError::PackageTooLarge));
    // Small in bytes but past the node budget (524,288 elements): 600,000
    // empty elements in 2.4 MB.
    let dense = package(
        &"<x/>".repeat(600_000),
        &item("c1", "c1.xhtml"),
        &itemref("c1"),
    );
    assert!(dense.len() < 8 * MIB);
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", dense.as_bytes(), Deflated),
        ("c1.xhtml", &c1, Stored),
    ]);
    assert!(matches!(err(open(&f)), BookError::PackageTooLarge));
    // Just under it opens.
    let dense = package(
        &"<x/>".repeat(500_000),
        &item("c1", "c1.xhtml"),
        &itemref("c1"),
    );
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", dense.as_bytes(), Deflated),
        ("c1.xhtml", &c1, Stored),
    ]);
    assert!(open(&f).is_ok());
}

/// A chapter body of exactly `len` bytes.
fn body_of(len: usize) -> Vec<u8> {
    let head = b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>";
    let tail = b"</p></body></html>";
    let mut v = head.to_vec();
    v.resize(len - tail.len(), b'a');
    v.extend_from_slice(tail);
    v
}

#[test]
fn chapter_over_8_mib_is_too_large_alone() {
    let manifest = [
        item("a", "a.xhtml"),
        item("b", "b.xhtml"),
        item("c", "c.xhtml"),
    ]
    .concat();
    let spine = [itemref("a"), itemref("b"), itemref("c")].concat();
    let opf = package("", &manifest, &spine);
    let cont = container("content.opf");
    let (exact, over, small) = (body_of(8 * MIB), body_of(8 * MIB + 1), xhtml("c"));
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        ("a.xhtml", &exact, Deflated),
        ("b.xhtml", &over, Deflated),
        ("c.xhtml", &small, Stored),
    ]);
    let book = open(&f).unwrap();
    let all = read_all(&book);
    assert_eq!(all[0].as_ref().map(Vec::len), Ok(8 * MIB));
    assert_eq!(all[1], Err(ChapterError::TooLarge));
    assert_eq!(ChapterError::TooLarge.reason(), "too large");
    assert_eq!(all[2].as_ref().unwrap(), &small);
}

#[test]
fn book_over_64_mib_is_too_large() {
    // One 8 MiB chapter read eight times is 64 MiB (opens); nine is over.
    let cont = container("content.opf");
    let body = body_of(8 * MIB);
    for (n, ok) in [(8, true), (9, false)] {
        let opf = package("", &item("a", "a.xhtml"), &itemref("a").repeat(n));
        let f = zip_file(&[
            ("META-INF/container.xml", cont.as_bytes(), Stored),
            ("content.opf", opf.as_bytes(), Stored),
            ("a.xhtml", &body, Deflated),
        ]);
        match open(&f) {
            Ok(b) => {
                assert!(ok);
                assert_eq!(b.chapters.len(), n);
            }
            Err(e) => {
                assert!(!ok, "{e}");
                assert!(matches!(e, BookError::TooLarge));
                assert_eq!(e.to_string(), "book is too large");
            }
        }
    }
    // A chapter too large alone does not count toward the book.
    let manifest = [item("a", "a.xhtml"), item("b", "b.xhtml")].concat();
    let spine = [itemref("a").repeat(8), itemref("b")].concat();
    let opf = package("", &manifest, &spine);
    let over = body_of(8 * MIB + 1);
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        ("a.xhtml", &body, Deflated),
        ("b.xhtml", &over, Deflated),
    ]);
    assert!(open(&f).is_ok());
}

/// `zip` with entry `name`'s uncompressed size forged to `size` in both
/// headers.
fn forge_size(z: &mut [u8], name: &str, size: u32) {
    let (local, central) = headers(z, name);
    z[local + 22..local + 26].copy_from_slice(&size.to_le_bytes());
    z[central + 24..central + 28].copy_from_slice(&size.to_le_bytes());
}

/// Offsets of entry `name`'s local and central headers.
fn headers(z: &[u8], name: &str) -> (usize, usize) {
    let mut at = 0;
    while let Some(p) = z[at..].windows(4).position(|w| w == b"PK\x01\x02") {
        let c = at + p;
        let nlen = u16::from_le_bytes([z[c + 28], z[c + 29]]) as usize;
        if &z[c + 46..c + 46 + nlen] == name.as_bytes() {
            let local = u32::from_le_bytes(z[c + 42..c + 46].try_into().unwrap()) as usize;
            return (local, c);
        }
        at = c + 4;
    }
    panic!("no entry {name}");
}

#[test]
fn zip_bomb_ratio_fails_the_book() {
    // A deflated entry of a few KiB claiming 1 GiB: over 1032:1, refused by
    // the ZIP reader before anything is inflated, and the whole book fails
    // (even though the entry is not a chapter).
    let cont = container("content.opf");
    let opf = package("", &item("a", "a.xhtml"), &itemref("a"));
    let a = xhtml("a");
    let mut z = zip_bytes(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        ("a.xhtml", &a, Stored),
        ("font.otf", &[7u8; 4096], Deflated),
    ]);
    forge_size(&mut z, "font.otf", 1 << 30);
    let e = err(open(&file_of(&z)));
    assert!(matches!(e, BookError::Damaged(_)));
    assert_eq!(
        e.to_string(),
        "damaged EPUB: an entry's compression ratio is too high"
    );
    // Two entries with the same name: the whole book too.
    let z = zip_bytes(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        ("a.xhtml", &a, Stored),
        ("a.xhtml", &a, Stored),
    ]);
    assert_eq!(
        err(open(&file_of(&z))).to_string(),
        "damaged EPUB: two entries have the same name"
    );
}

#[test]
fn damaged_chapter_data_fails_only_that_chapter() {
    let cont = container("content.opf");
    let manifest = [item("a", "a.xhtml"), item("b", "b.xhtml")].concat();
    let opf = package("", &manifest, &[itemref("a"), itemref("b")].concat());
    let (a, b) = (xhtml("a"), xhtml("b"));
    let mut z = zip_bytes(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        ("a.xhtml", &a, Stored),
        ("b.xhtml", &b, Stored),
    ]);
    // Flip a data byte of a.xhtml: its CRC no longer matches.
    let (local, _) = headers(&z, "a.xhtml");
    z[local + 30 + "a.xhtml".len() + 5] ^= 1;
    let f = file_of(&z);
    let book = open(&f).unwrap();
    let all = read_all(&book);
    assert_eq!(
        all[0],
        Err(ChapterError::Damaged(
            "the entry's CRC-32 does not match".into()
        ))
    );
    assert_eq!(
        all[0].as_ref().unwrap_err().reason(),
        "damaged (the entry's CRC-32 does not match)"
    );
    assert_eq!(all[1].as_ref().unwrap(), &b);
}

#[test]
fn svg_spine_item_and_unsupported_type() {
    let manifest = [
        item("a", "a.xhtml"),
        r#"<item id="s" href="cover.svg" media-type="image/svg+xml"/>"#.to_string(),
        r#"<item id="h" href="old.html" media-type="text/html"/>"#.to_string(),
        r#"<item id="d" href="b.xml" media-type="application/x-dtbook+xml"/>"#.to_string(),
    ]
    .concat();
    let spine = [itemref("s"), itemref("a"), itemref("h"), itemref("d")].concat();
    let opf = package("", &manifest, &spine);
    let cont = container("content.opf");
    let a = xhtml("a");
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        ("a.xhtml", &a, Stored),
        (
            "cover.svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
            Stored,
        ),
        ("old.html", b"<html></html>", Stored),
        ("b.xml", b"<dtbook/>", Stored),
    ]);
    let book = open(&f).unwrap();
    assert_eq!(book.items[0].media_type, "image/svg+xml");
    let all = read_all(&book);
    // An SVG page carries no text: it reads as nothing, the caller reports
    // it by its media type.
    assert_eq!(all[0], Ok(Vec::new()));
    assert_eq!(all[1].as_ref().unwrap(), &a);
    assert_eq!(
        all[2],
        Err(ChapterError::UnsupportedType("text/html".into()))
    );
    assert_eq!(
        all[3].as_ref().unwrap_err().reason(),
        "unsupported type application/x-dtbook+xml"
    );
}

#[test]
fn mimetype_not_first_is_accepted() {
    // The mimetype entry last, and deflated.
    let opf = package("", &item("c1", "c1.xhtml"), &itemref("c1"));
    let cont = container("content.opf");
    let c1 = xhtml("1");
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        ("c1.xhtml", &c1, Stored),
        ("mimetype", b"application/epub+zip", Deflated),
    ]);
    assert_eq!(open(&f).unwrap().chapters.len(), 1);
    // And none at all.
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        ("c1.xhtml", &c1, Stored),
    ]);
    assert_eq!(open(&f).unwrap().chapters.len(), 1);
}

#[test]
fn open_is_linear_in_the_spine() {
    // The spine is resolved through a name table, not a scan per item:
    // 10,000 chapters over 10,000 distinct entries open in well under a
    // second even in a debug build (a scan per item would be 10^8 name
    // comparisons). The bound is generous; the point is the shape.
    let n = 10_000;
    let manifest: String = (0..n)
        .map(|i| item(&format!("c{i}"), &format!("c{i}.xhtml")))
        .collect();
    let spine: String = (0..n).map(|i| itemref(&format!("c{i}"))).collect();
    let opf = package("", &manifest, &spine);
    let cont = container("content.opf");
    let names: Vec<String> = (0..n).map(|i| format!("c{i}.xhtml")).collect();
    let body = xhtml("x");
    let mut entries: Vec<(&str, &[u8], Store)> = vec![
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
    ];
    for name in &names {
        entries.push((name, &body, Stored));
    }
    let f = zip_file(&entries);
    let t = std::time::Instant::now();
    let book = open(&f).unwrap();
    assert_eq!(book.chapters.len(), n);
    assert!(t.elapsed().as_secs_f64() < 2.0, "{:?}", t.elapsed());
}

#[test]
fn href_escaping_the_container_is_outside() {
    let hrefs = [
        "../../x.xhtml",
        "../%2E%2E/x.xhtml",
        "/abs.xhtml",
        "%2F..%2F..%2Fx.xhtml",
        "http://example.com/c.xhtml",
        "file:///etc/passwd",
        "mailto:a@b",
        "../in-root.xhtml",
    ];
    let manifest: String = hrefs
        .iter()
        .enumerate()
        .map(|(i, h)| item(&format!("i{i}"), h))
        .collect();
    let spine: String = (0..hrefs.len())
        .map(|i| itemref(&format!("i{i}")))
        .collect();
    let opf = package("", &manifest, &spine);
    let cont = container("OEBPS/content.opf");
    let inside = xhtml("root");
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("OEBPS/content.opf", opf.as_bytes(), Stored),
        ("in-root.xhtml", &inside, Stored),
        // Entries with the names an escaping path would normalize to must
        // not be found through it.
        ("x.xhtml", &inside, Stored),
        ("abs.xhtml", &inside, Stored),
    ]);
    let book = open(&f).unwrap();
    let all = read_all(&book);
    for (i, h) in hrefs.iter().enumerate().take(7) {
        assert_eq!(all[i], Err(ChapterError::OutsideTheBook), "{h}");
        // The path is the href as written: there is no entry to name.
        assert_eq!(path(&book, i + 1), *h);
    }
    // `..` from OEBPS/ to the root stays inside the container.
    assert_eq!(path(&book, 8), "in-root.xhtml");
    assert_eq!(all[7].as_ref().unwrap(), &inside);
    assert_eq!(ChapterError::OutsideTheBook.reason(), "outside the book");

    // A package path that leaves the container is no package.
    let cont = container("../content.opf");
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
    ]);
    assert!(matches!(err(open(&f)), BookError::NoPackage));
}

const AES: &str = "http://www.w3.org/2001/04/xmlenc#aes128-cbc";
const IDPF: &str = "http://www.idpf.org/2008/embedding";
const ADOBE: &str = "http://ns.adobe.com/pdf/enc#RC";

/// `META-INF/encryption.xml` listing `(algorithm, uri)` pairs, each in an
/// `EncryptedData` with a key whose own `EncryptionMethod` is RSA (it must
/// not be taken for the data's).
fn encryption(list: &[(&str, &str)]) -> String {
    let data: String = list
        .iter()
        .map(|(alg, uri)| {
            format!(
                r#"<enc:EncryptedData>
  <enc:EncryptionMethod Algorithm="{alg}"/>
  <ds:KeyInfo><enc:EncryptedKey>
    <enc:EncryptionMethod Algorithm="http://www.w3.org/2001/04/xmlenc#rsa-1_5"/>
    <enc:CipherData><enc:CipherValue>AAAA</enc:CipherValue></enc:CipherData>
  </enc:EncryptedKey></ds:KeyInfo>
  <enc:CipherData><enc:CipherReference URI="{uri}"/></enc:CipherData>
</enc:EncryptedData>"#
            )
        })
        .collect();
    format!(
        r#"<?xml version="1.0"?>
<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container"
  xmlns:enc="http://www.w3.org/2001/04/xmlenc#" xmlns:ds="http://www.w3.org/2000/09/xmldsig#">{data}</encryption>"#
    )
}

/// A book of two chapters (`OEBPS/a.xhtml`, `OEBPS/b.xhtml`), an SVG page
/// and a font, with this `encryption.xml` (if any).
fn encrypted_book(enc: Option<&str>) -> Vec<u8> {
    encrypted_book_with(enc, &[])
}

fn encrypted_book_with(enc: Option<&str>, extra: &[(&str, &[u8], Store)]) -> Vec<u8> {
    let manifest = [
        item("a", "a.xhtml"),
        item("b", "b.xhtml"),
        r#"<item id="s" href="s.svg" media-type="image/svg+xml"/>"#.to_string(),
        r#"<item id="f" href="f.otf" media-type="font/otf"/>"#.to_string(),
    ]
    .concat();
    let spine = [itemref("s"), itemref("a"), itemref("b")].concat();
    let opf = package("", &manifest, &spine);
    let cont = container("OEBPS/content.opf");
    let (a, b) = (xhtml("a"), xhtml("b"));
    let mut entries: Vec<(&str, &[u8], Store)> = vec![
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("OEBPS/content.opf", opf.as_bytes(), Stored),
        ("OEBPS/a.xhtml", &a, Stored),
        ("OEBPS/b.xhtml", &b, Stored),
        (
            "OEBPS/s.svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
            Stored,
        ),
        ("OEBPS/f.otf", b"OTTO", Stored),
    ];
    if let Some(enc) = enc {
        entries.push(("META-INF/encryption.xml", enc.as_bytes(), Stored));
    }
    entries.extend_from_slice(extra);
    zip_bytes(&entries)
}

#[test]
fn font_obfuscation_is_not_drm() {
    for alg in [IDPF, ADOBE] {
        // The font obfuscated, and (oddly) a chapter: obfuscation is not
        // encryption, the chapter is read as it is.
        let enc = encryption(&[(alg, "OEBPS/f.otf"), (alg, "OEBPS/a.xhtml")]);
        let f = file_of(&encrypted_book(Some(&enc)));
        let book = open(&f).unwrap();
        let all = read_all(&book);
        assert_eq!(all[1].as_ref().unwrap(), &xhtml("a"), "{alg}");
        assert_eq!(all[2].as_ref().unwrap(), &xhtml("b"), "{alg}");
    }
    // A real cipher on a resource that is not a chapter: no chapter fails.
    let enc = encryption(&[(AES, "OEBPS/f.otf")]);
    let f = file_of(&encrypted_book(Some(&enc)));
    let book = open(&f).unwrap();
    assert!(read_all(&book).iter().all(Result::is_ok));
}

#[test]
fn all_chapters_encrypted_is_drm() {
    // Both XHTML chapters encrypted; the SVG page is not, but it is no
    // readable chapter. The URIs are relative to the container root, one
    // percent-encoded.
    let enc = encryption(&[(AES, "OEBPS/a.xhtml"), (AES, "OEBPS/%62.xhtml")]);
    let e = err(open(&file_of(&encrypted_book(Some(&enc)))));
    assert!(matches!(e, BookError::DrmProtected));
    assert_eq!(e.to_string(), "book is DRM-protected");
    // An EncryptedData with no method at all is encrypted (fail closed).
    let enc = encryption(&[(AES, "OEBPS/a.xhtml"), (AES, "OEBPS/b.xhtml")]).replacen(
        &format!(r#"<enc:EncryptionMethod Algorithm="{AES}"/>"#),
        "",
        1,
    );
    assert!(matches!(
        err(open(&file_of(&encrypted_book(Some(&enc))))),
        BookError::DrmProtected
    ));
    // A `rights.xml` alone decides nothing.
    let rights: &[(&str, &[u8], Store)] = &[("META-INF/rights.xml", b"<rights/>", Stored)];
    let f = file_of(&encrypted_book_with(None, rights));
    assert!(read_all(&open(&f).unwrap()).iter().all(Result::is_ok));
}

#[test]
fn some_chapters_encrypted() {
    let enc = encryption(&[(AES, "OEBPS/b.xhtml")]);
    let f = file_of(&encrypted_book(Some(&enc)));
    let book = open(&f).unwrap();
    let all = read_all(&book);
    assert_eq!(all[0], Ok(Vec::new()));
    assert_eq!(all[1].as_ref().unwrap(), &xhtml("a"));
    assert_eq!(all[2], Err(ChapterError::Encrypted));
    assert_eq!(ChapterError::Encrypted.reason(), "encrypted");
}

#[test]
fn encryption_file_is_part_of_the_skeleton() {
    // Not well-formed: the book fails, naming the file.
    let f = file_of(&encrypted_book(Some("<encryption>")));
    let e = err(open(&f));
    assert!(
        e.to_string().ends_with(" in META-INF/encryption.xml)"),
        "{e}"
    );
    // Over 8 MiB.
    let big = format!("{}<!--{}-->", encryption(&[]), "x".repeat(8 * 1024 * 1024));
    let f = file_of(&encrypted_book(Some(&big)));
    assert!(matches!(err(open(&f)), BookError::PackageTooLarge));
}

/// Sets the traditional-encryption flag of entry `name` in both headers.
fn zip_encrypt(z: &mut [u8], name: &str) {
    let (local, central) = headers(z, name);
    z[local + 6] |= 1;
    z[central + 8] |= 1;
}

#[test]
fn zip_encrypted_entry_counts_as_encrypted() {
    let mut z = encrypted_book(None);
    zip_encrypt(&mut z, "OEBPS/a.xhtml");
    let f = file_of(&z);
    let book = open(&f).unwrap();
    let all = read_all(&book);
    assert_eq!(all[1], Err(ChapterError::Encrypted));
    assert_eq!(all[2].as_ref().unwrap(), &xhtml("b"));
    // Both: the book is DRM-protected.
    zip_encrypt(&mut z, "OEBPS/b.xhtml");
    assert!(matches!(err(open(&file_of(&z))), BookError::DrmProtected));
    // An encrypted skeleton file is damage, not DRM.
    let mut z = encrypted_book(None);
    zip_encrypt(&mut z, "OEBPS/content.opf");
    assert_eq!(
        err(open(&file_of(&z))).to_string(),
        "damaged EPUB: the entry is encrypted"
    );
}

#[test]
fn open_resolves_each_item_once() {
    // 10,000 itemrefs naming one manifest item whose href or media type
    // is most of the package: the item is resolved once and kept once, at
    // most `MAX_PATH` + `MAX_MEDIA_TYPE` bytes. Resolving it per itemref
    // copied the href and the type 10,000 times (10 GB for the 1 MiB href)
    // and normalized the `x/../` climb 10,000 times (4 * 10^9 steps).
    let n = 10_000;
    let spine: String = (0..n).map(|_| itemref("c")).collect();
    let long_href = "a".repeat(MIB);
    let long_type = "x".repeat(100 << 10);
    let climb = format!("{}c.xhtml", "x/../".repeat(80_000));
    let cont = container("content.opf");
    let body = xhtml("c");
    let cases = [
        item("c", &long_href),
        format!(r#"<item id="c" href="c.xhtml" media-type="{long_type}"/>"#),
        item("c", &climb),
    ];
    let mut got = Vec::new();
    for manifest in &cases {
        let opf = package("", manifest, &spine);
        let f = zip_file(&[
            ("META-INF/container.xml", cont.as_bytes(), Stored),
            ("content.opf", opf.as_bytes(), Stored),
            ("c.xhtml", &body, Stored),
        ]);
        let book = open(&f).unwrap();
        assert_eq!(book.chapters.len(), n);
        // Work: each item's href and type once, within the package's size.
        assert!(book.steps() <= opf.len() as u64, "{} steps", book.steps());
        assert!(book.chapters.iter().all(|c| c.item == Some(0)));
        // Memory: one item, its strings capped.
        assert_eq!(book.items.len(), 1);
        let it = &book.items[0];
        assert!(it.path.len() <= MAX_PATH + 3 && it.media_type.len() <= MAX_MEDIA_TYPE + 3);
        let first = book.chapter(1, &mut Vec::new());
        assert_eq!(book.chapter(n, &mut Vec::new()), first);
        got.push((first, it.clone()));
    }
    // A 1 MiB path names no entry: missing, its first 512 bytes kept.
    assert_eq!(got[0].0, Err(ChapterError::Missing));
    assert_eq!(got[0].1.path, format!("{}...", "a".repeat(MAX_PATH)));
    // A 100 KiB media type is no type: its first 255 bytes are shown.
    let shown = format!("{}...", "x".repeat(MAX_MEDIA_TYPE));
    assert_eq!(got[1].0, Err(ChapterError::UnsupportedType(shown.clone())));
    assert_eq!(
        got[1].0.as_ref().unwrap_err().reason(),
        format!("unsupported type {shown}")
    );
    assert_eq!(got[1].1.media_type, shown);
    // The climb resolves (once) to the chapter.
    assert_eq!(got[2].0, Ok(()));
    assert_eq!(got[2].1.path, "c.xhtml");
}

#[test]
fn path_and_media_type_at_their_caps() {
    // A resolved path of exactly 512 bytes is looked up and read; one byte
    // more names no entry. A media type of 255 bytes is a type (here not
    // a supported one, shown whole); 256 bytes is cut.
    let at = format!("{}.xhtml", "p".repeat(MAX_PATH - 6));
    let over = format!("{}.xhtml", "q".repeat(MAX_PATH - 5));
    assert_eq!((at.len(), over.len()), (MAX_PATH, MAX_PATH + 1));
    let t255 = format!("x/{}", "y".repeat(MAX_MEDIA_TYPE - 2));
    let t256 = format!("x/{}", "y".repeat(MAX_MEDIA_TYPE - 1));
    let manifest = [
        item("a", &at),
        item("b", &over),
        format!(r#"<item id="c" href="{at}" media-type="{t255}"/>"#),
        format!(r#"<item id="d" href="{at}" media-type="{t256}"/>"#),
    ]
    .concat();
    let spine = [itemref("a"), itemref("b"), itemref("c"), itemref("d")].concat();
    let opf = package("", &manifest, &spine);
    let cont = container("content.opf");
    let body = xhtml("a");
    let f = zip_file(&[
        ("META-INF/container.xml", cont.as_bytes(), Stored),
        ("content.opf", opf.as_bytes(), Stored),
        (&at, &body, Stored),
    ]);
    let book = open(&f).unwrap();
    let all = read_all(&book);
    assert_eq!(all[0].as_ref().unwrap(), &body);
    assert_eq!(all[1], Err(ChapterError::Missing));
    assert_eq!(path(&book, 2), format!("{}...", &over[..MAX_PATH]));
    assert_eq!(all[2], Err(ChapterError::UnsupportedType(t255.clone())));
    let cut = format!("{}...", &t256[..MAX_MEDIA_TYPE]);
    assert_eq!(all[3], Err(ChapterError::UnsupportedType(cut)));
}
