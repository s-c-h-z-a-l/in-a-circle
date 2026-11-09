//! The EPUB packer: the container layout, the package metadata, the table
//! of contents, links across chapters, the chapter limit, determinism and
//! memory. `epubcheck` (a test-time tool, not a dependency) checks whole
//! books when it is installed; `INACIRCLE_REQUIRE_BOOK_TOOLS=1` makes a
//! missing one a failure.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use doc::{Dialect, Document, LimitError, Limits, Loss};
use epub::{BookError, Meta, fnv1a_128, uuid_v8, write};

// ---------- memory measurement ----------

/// Counts the live heap bytes of the measuring thread while it is armed,
/// and their peak. Thread-local, so the other tests running in parallel do
/// not disturb the numbers.
struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn track(delta: isize) {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            let live = LIVE.with(|l| {
                l.set(l.get() + delta);
                l.get()
            });
            PEAK.with(|p| p.set(p.get().max(live)));
        }
    });
}

// SAFETY: every call is forwarded to `System` unchanged; the counters are
// thread-local `Cell`s without destructors, so touching them never
// allocates or reenters the allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's layout.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            track(layout.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        track(-(layout.size() as isize));
        // SAFETY: `ptr` came from `alloc` with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let p = unsafe { System.realloc(ptr, layout, new) };
        if !p.is_null() {
            track(new as isize - layout.size() as isize);
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Peak heap bytes `f` holds at once beyond what was live when it started.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    LIVE.with(|l| l.set(0));
    PEAK.with(|p| p.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (out, PEAK.with(|p| p.get()).max(0) as usize)
}

// ---------- fixtures ----------

/// A book file path under the cargo test temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        Scratch(PathBuf::from(format!(
            "{}/epub-{}-{}.epub",
            env!("CARGO_TARGET_TMPDIR"),
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn create(&self) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&self.0)
            .unwrap()
    }
    fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.0).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn md(src: &str) -> Document {
    doc::md::read_with(src.as_bytes(), Dialect::Gfm).expect("fixture reads")
}

fn meta(title: &str) -> Meta {
    Meta {
        title: title.to_string(),
        lang: "hu".to_string(),
        modified_unix: 1_700_000_000,
        id_seed: b"seed".to_vec(),
    }
}

fn book_with(
    d: &Document,
    m: &Meta,
    limits: Limits,
) -> (Scratch, Result<BTreeSet<Loss>, BookError>) {
    let s = Scratch::new();
    let r = write(d, m, s.create(), limits);
    (s, r)
}

fn book(src: &str, title: &str) -> (Scratch, BTreeSet<Loss>) {
    let (s, r) = book_with(&md(src), &meta(title), Limits::DEFAULT);
    let losses = r.unwrap_or_else(|e| panic!("book fails: {e}"));
    (s, losses)
}

/// Every entry of the book, in archive order, read back by our reader.
fn entries(path: &Path) -> Vec<(String, Vec<u8>)> {
    let f = File::open(path).unwrap();
    let limits = zip::Limits {
        entries: 4096,
        entry_bytes: 64 << 20,
        ratio: 1,
        name_bytes: 255,
    };
    let a = zip::open(&f, limits).unwrap();
    (0..a.entries().len())
        .map(|i| {
            let mut v = Vec::new();
            a.read(i, &mut v).unwrap();
            (String::from_utf8(a.entries()[i].name.clone()).unwrap(), v)
        })
        .collect()
}

fn text(es: &[(String, Vec<u8>)], name: &str) -> String {
    let (_, v) = es
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no entry {name}"));
    String::from_utf8(v.clone()).unwrap()
}

/// A tool on `PATH`; a missing one skips with a visible line, unless
/// `INACIRCLE_REQUIRE_BOOK_TOOLS=1` makes it a failure.
fn have(tool: &str) -> bool {
    let found = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
        .unwrap_or(false);
    if !found {
        if std::env::var_os("INACIRCLE_REQUIRE_BOOK_TOOLS").is_some_and(|v| v == "1") {
            panic!("{tool} is required (INACIRCLE_REQUIRE_BOOK_TOOLS=1) but not installed");
        }
        println!("skipped: {tool} not installed");
    }
    found
}

/// `epubcheck` finds no error and no warning in the book (when installed).
fn epubcheck(path: &Path) {
    if !have("epubcheck") {
        return;
    }
    let out = Command::new("epubcheck")
        .arg("--failonwarnings")
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "epubcheck: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------- tests ----------

#[test]
fn epubcheck_passes() {
    let src = "\
# Áttekintés

Bevezető bekezdés, *kiemelés* és `kód`. [Lásd a tervet](#2026-terv).

## Részletek

| Név | Érték |
|:----|------:|
| egy | 1 |
| kettő | 2 |

```rust
fn main() {}
```

# 2026 terv

- első
- második

> idézet [vissza](#áttekintés) és [kint](https://example.org/).

# Függelék

Utolsó fejezet.
";
    let (s, losses) = book(src, "Jegyzetek 2026");
    assert!(losses.is_empty(), "{losses:?}");
    let es = entries(&s.0);
    assert!(es.iter().any(|(n, _)| n == "OEBPS/ch0003.xhtml"));
    epubcheck(&s.0);
}

#[test]
fn mimetype_first_and_stored() {
    let (s, _) = book("# A\n\ntext\n", "A");
    let b = s.bytes();
    // Local file header of the first entry (APPNOTE 4.3.7).
    assert_eq!(&b[0..4], b"PK\x03\x04");
    assert_eq!(u16::from_le_bytes([b[8], b[9]]), 0, "method: stored");
    assert_eq!(u16::from_le_bytes([b[26], b[27]]), 8, "name length");
    assert_eq!(u16::from_le_bytes([b[28], b[29]]), 0, "no extra field");
    assert_eq!(&b[30..38], b"mimetype");
    assert_eq!(&b[38..58], b"application/epub+zip");
    let names: Vec<String> = entries(&s.0).into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        names,
        [
            "mimetype",
            "META-INF/container.xml",
            "OEBPS/content.opf",
            "OEBPS/nav.xhtml",
            "OEBPS/style.css",
            "OEBPS/ch0001.xhtml",
        ]
    );
}

#[test]
fn opf_metadata() {
    let (s, _) = book("# A\n", "Jegyzetek & <terv>");
    let es = entries(&s.0);
    let opf = text(&es, "OEBPS/content.opf");
    let id = uuid_v8(fnv1a_128(&[b"seed"]));
    assert!(
        opf.contains("<dc:title>Jegyzetek &amp; &lt;terv&gt;</dc:title>"),
        "{opf}"
    );
    assert!(opf.contains("<dc:language>hu</dc:language>"), "{opf}");
    assert!(
        opf.contains(&format!(
            "<dc:identifier id=\"uid\">urn:uuid:{id}</dc:identifier>"
        )),
        "{opf}"
    );
    assert!(opf.contains("unique-identifier=\"uid\""), "{opf}");
    assert!(
        opf.contains("<meta property=\"dcterms:modified\">2023-11-14T22:13:20Z</meta>"),
        "{opf}"
    );
    assert!(opf.contains("properties=\"nav\""), "{opf}");
    assert!(!opf.contains("ncx"), "no NCX: {opf}");
    let container = text(&es, "META-INF/container.xml");
    assert!(
        container.contains("full-path=\"OEBPS/content.opf\""),
        "{container}"
    );

    // The time in UTC across the edges of the format, and a seed that
    // differs gives another identifier.
    for (t, want) in [
        (0, "1970-01-01T00:00:00Z"),
        (-1, "1969-12-31T23:59:59Z"),
        (951_782_400, "2000-02-29T00:00:00Z"),
        (-62_135_596_800, "0001-01-01T00:00:00Z"),
        (i64::MIN, "0001-01-01T00:00:00Z"),
        (253_402_300_799, "9999-12-31T23:59:59Z"),
        (i64::MAX, "9999-12-31T23:59:59Z"),
    ] {
        let mut m = meta("A");
        m.modified_unix = t;
        m.id_seed = b"other".to_vec();
        let (s, r) = book_with(&md("x\n"), &m, Limits::DEFAULT);
        r.unwrap();
        let opf = text(&entries(&s.0), "OEBPS/content.opf");
        assert!(opf.contains(&format!(">{want}</meta>")), "{t}: {opf}");
        assert!(!opf.contains(&id), "{opf}");
    }

    // A language that is not a well-formed tag is `und`, everywhere.
    for (lang, want) in [
        ("xx_YY", "und"),
        ("", "und"),
        ("hu\"x", "und"),
        ("pt-BR", "pt-BR"),
        // Language, script, region (letters or digits), variants.
        ("hu", "hu"),
        ("zh-Hant-TW", "zh-Hant-TW"),
        ("es-419", "es-419"),
        ("de-CH-1996", "de-CH-1996"),
        ("sl-rozaj", "sl-rozaj"),
        // Not well-formed: a lone singleton, a one-character subtag, a
        // three-character subtag that is neither region nor variant, a
        // singleton with nothing after it, an empty subtag, a script after
        // the region, extensions and private use (not taken).
        ("hu-a", "und"),
        ("hu-1", "und"),
        ("hu-ab1", "und"),
        ("hu-x", "und"),
        ("hu--HU", "und"),
        ("hu-", "und"),
        ("sr-RS-Latn", "und"),
        ("en-x-private", "und"),
        ("en-a-bbb", "und"),
        ("hu1", "und"),
    ] {
        let mut m = meta("A");
        m.lang = lang.to_string();
        let (s, r) = book_with(&md("# A\n"), &m, Limits::DEFAULT);
        r.unwrap();
        let es = entries(&s.0);
        let opf = text(&es, "OEBPS/content.opf");
        assert!(
            opf.contains(&format!("<dc:language>{want}</dc:language>")),
            "{lang}: {opf}"
        );
        let ch = text(&es, "OEBPS/ch0001.xhtml");
        assert!(
            ch.contains(&format!("xml:lang=\"{want}\" lang=\"{want}\"")),
            "{lang}: {ch}"
        );
    }
}

#[test]
fn nav_lists_chapters_and_one_level_below() {
    let src = "intro\n\n# A\n\n## a1\n\n### deep\n\n## a2\n\n> ## quoted\n\n# B\n\ntext\n";
    let (s, _) = book(src, "Book");
    let nav = text(&entries(&s.0), "OEBPS/nav.xhtml");
    assert!(nav.contains("<nav epub:type=\"toc\" id=\"toc\">"), "{nav}");
    let want = "<ol>\n\
<li><a href=\"ch0001.xhtml\">Chapter 1</a></li>\n\
<li><a href=\"ch0002.xhtml\">A</a>\n\
<ol>\n\
<li><a href=\"ch0002.xhtml#a1\">a1</a></li>\n\
<li><a href=\"ch0002.xhtml#a2\">a2</a></li>\n\
</ol>\n\
</li>\n\
<li><a href=\"ch0003.xhtml\">B</a></li>\n\
</ol>\n";
    assert!(nav.contains(want), "{nav}");
    assert!(!nav.contains("deep") && !nav.contains("quoted"), "{nav}");

    // Split at H2: the H3s are the level below; one chapter without a
    // heading is named after the book.
    let (s, _) = book("## x\n\n### x1\n\n## y\n", "Book");
    let nav = text(&entries(&s.0), "OEBPS/nav.xhtml");
    assert!(nav.contains("<a href=\"ch0001.xhtml#x1\">x1</a>"), "{nav}");
    let (s, _) = book("just text\n", "Plain & simple");
    let nav = text(&entries(&s.0), "OEBPS/nav.xhtml");
    assert!(
        nav.contains("<li><a href=\"ch0001.xhtml\">Plain &amp; simple</a></li>"),
        "{nav}"
    );
    epubcheck(&s.0);
}

#[test]
fn internal_links_resolve_across_chapters() {
    let src = "# Áttekintés\n\n[lásd](#2026-terv)\n\n# 2026 terv\n\n[x](#áttekintés)\n";
    let (s, losses) = book(src, "T");
    assert!(losses.is_empty(), "{losses:?}");
    let es = entries(&s.0);
    let c1 = text(&es, "OEBPS/ch0001.xhtml");
    let c2 = text(&es, "OEBPS/ch0002.xhtml");
    assert!(c1.contains("<h1 id=\"áttekintés\">Áttekintés</h1>"), "{c1}");
    assert!(
        c1.contains("<a href=\"ch0002.xhtml#h-2026-terv\">lásd</a>"),
        "{c1}"
    );
    assert!(c2.contains("<h1 id=\"h-2026-terv\">2026 terv</h1>"), "{c2}");
    assert!(
        c2.contains("<a href=\"ch0001.xhtml#%C3%A1ttekint%C3%A9s\">x</a>"),
        "{c2}"
    );
    // The chapter shell around the body.
    assert!(c1.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html>\n"));
    assert!(c1.contains("<title>Áttekintés</title>"), "{c1}");
    assert!(c1.contains("<link rel=\"stylesheet\" type=\"text/css\" href=\"style.css\"/>"));
    assert!(c1.ends_with("</body>\n</html>\n"), "{c1}");
    epubcheck(&s.0);
}

#[test]
fn chapter_limit() {
    let many = |n: usize| (1..=n).map(|i| format!("# C{i}\n\n")).collect::<String>();
    let (s, r) = book_with(&md(&many(1024)), &meta("Many"), Limits::DEFAULT);
    r.unwrap();
    let es = entries(&s.0);
    assert_eq!(es.len(), 5 + 1024);
    assert!(text(&es, "OEBPS/nav.xhtml").contains("<a href=\"ch1024.xhtml\">C1024</a>"));
    epubcheck(&s.0);

    let (s, r) = book_with(&md(&many(1025)), &meta("Too many"), Limits::DEFAULT);
    let e = r.unwrap_err();
    assert!(matches!(e, BookError::TooManyChapters), "{e:?}");
    assert_eq!(e.to_string(), "book would have more than 1024 chapters");
    assert_eq!(s.bytes().len(), 0, "the caller's file stays empty");
}

/// The first level the table of contents lists below the chapters is the
/// split's, not the first heading's: a preface heading of another level
/// does not shift it; a single chapter takes its first heading's level.
#[test]
fn nav_levels_follow_the_split() {
    // One H1, two H2: split at H2, the H3s are the level below.
    let (s, _) = book("# T\n\n## x\n\n### x1\n\n## y\n", "Book");
    let nav = text(&entries(&s.0), "OEBPS/nav.xhtml");
    assert!(
        nav.contains("<li><a href=\"ch0002.xhtml\">x</a>\n<ol>\n<li><a href=\"ch0002.xhtml#x1\">x1</a></li>\n</ol>\n</li>"),
        "{nav}"
    );
    assert!(!nav.contains("ch0002.xhtml#x\""), "{nav}");
    // One chapter: the level of its first heading, past a paragraph.
    let (s, _) = book("text\n\n## x\n\n### x1\n", "Book");
    let nav = text(&entries(&s.0), "OEBPS/nav.xhtml");
    assert!(nav.contains("<a href=\"ch0001.xhtml#x1\">x1</a>"), "{nav}");
}

/// The table of contents links to heading ids with the characters of
/// `-` and `_` as they are and every other byte percent-encoded.
#[test]
fn nav_fragments_are_percent_encoded() {
    let (s, _) = book(
        "# Top\n\n## a-b_c\n\n## \u{c1}ttekint\u{e9}s 9\n\n# Two\n",
        "Book",
    );
    let nav = text(&entries(&s.0), "OEBPS/nav.xhtml");
    for want in [
        "<a href=\"ch0001.xhtml#a-b_c\">a-b_c</a>",
        "<a href=\"ch0001.xhtml#%C3%A1ttekint%C3%A9s-9\">",
    ] {
        assert!(nav.contains(want), "{want}\n{nav}");
    }
    epubcheck(&s.0);
}

/// Quotes and carriage returns in a title are escaped in the package
/// document, the table of contents and the chapter head.
#[test]
fn title_is_escaped_where_it_is_written() {
    let (s, _) = book("x\n", "a\"b\rc&d<e>f");
    let es = entries(&s.0);
    let want = "a&quot;b&#13;c&amp;d&lt;e&gt;f";
    assert!(
        text(&es, "OEBPS/content.opf").contains(&format!("<dc:title>{want}</dc:title>")),
        "{}",
        text(&es, "OEBPS/content.opf")
    );
    let nav = text(&es, "OEBPS/nav.xhtml");
    assert!(nav.contains(&format!("<title>{want}</title>")), "{nav}");
    assert!(nav.contains(&format!("<h1>{want}</h1>")), "{nav}");
    assert!(nav.contains(&format!(">{want}</a></li>")), "{nav}");
    let ch = text(&es, "OEBPS/ch0001.xhtml");
    assert!(ch.contains(&format!("<title>{want}</title>")), "{ch}");
}

/// The output budget is charged for exactly the XHTML of the book (the
/// package document, the table of contents, every chapter's head, body and
/// end): a limit of that many bytes passes, one byte less fails, and so
/// does every smaller one.
#[test]
fn output_budget_is_charged_exactly() {
    let src = "# One\n\ntext & more\n\n# Two\n\n- a\n- b\n";
    let (s, _) = book(src, "T");
    let total: usize = entries(&s.0)
        .iter()
        .filter(|(n, _)| n.ends_with(".xhtml") || n.ends_with(".opf"))
        .map(|(_, v)| v.len())
        .sum();
    assert!(total > 1000, "{total}");
    for limit in 0..=total + 2 {
        let limits = Limits {
            output_bytes: limit,
            ..Limits::DEFAULT
        };
        let (s, r) = book_with(&md(src), &meta("T"), limits);
        if limit >= total {
            r.unwrap_or_else(|e| panic!("limit {limit} of {total}: {e}"));
        } else {
            let e = r.unwrap_err();
            assert!(
                matches!(e, BookError::Limit(LimitError::Exceeded("output"))),
                "limit {limit} of {total}: {e:?}"
            );
            assert_eq!(s.bytes().len(), 0, "limit {limit}");
        }
    }
}

#[test]
fn failed_book_leaves_the_file_empty() {
    // The output budget ends in the second chapter: entries were already
    // written, and the packer cuts the file back to nothing.
    let src = format!(
        "# A\n\n{}\n\n# B\n\n{}\n",
        "a".repeat(4000),
        "b".repeat(4000)
    );
    let limits = Limits {
        output_bytes: 6000,
        ..Limits::DEFAULT
    };
    let (s, r) = book_with(&md(&src), &meta("T"), limits);
    let e = r.unwrap_err();
    assert!(
        matches!(e, BookError::Limit(LimitError::Exceeded("output"))),
        "{e:?}"
    );
    assert_eq!(s.bytes().len(), 0);
}

#[test]
fn empty_input_makes_a_valid_book() {
    for src in ["", "   \n\t\n"] {
        let (s, losses) = book(src, "notes");
        assert!(losses.is_empty());
        let es = entries(&s.0);
        assert_eq!(es.len(), 6, "one chapter");
        let nav = text(&es, "OEBPS/nav.xhtml");
        assert!(nav.contains("<a href=\"ch0001.xhtml\">notes</a>"), "{nav}");
        let ch = text(&es, "OEBPS/ch0001.xhtml");
        assert!(ch.contains("<title>notes</title>"), "{ch}");
        epubcheck(&s.0);
    }
    // An empty title (and one of forbidden characters only) still gives a
    // book with a title.
    let (s, losses) = book("", "\u{1}");
    assert_eq!(losses, BTreeSet::from([Loss::ControlCharacters]));
    let opf = text(&entries(&s.0), "OEBPS/content.opf");
    assert!(opf.contains("<dc:title>Untitled</dc:title>"), "{opf}");
}

#[test]
fn losses_are_reported() {
    let (s, losses) = book("# A\u{1}\n\n<b>x</b>\n\n![pic](p.png)\n\n# B\n", "T");
    assert_eq!(
        losses,
        BTreeSet::from([Loss::ControlCharacters, Loss::Html, Loss::Images])
    );
    let nav = text(&entries(&s.0), "OEBPS/nav.xhtml");
    assert!(nav.contains(">A</a>"), "{nav}");
}

#[test]
fn deterministic_output() {
    let src = "# One\n\ntext\n\n# Two\n\n| a |\n|---|\n| 1 |\n";
    let (a, _) = book(src, "T");
    let (b, _) = book(src, "T");
    assert_eq!(a.bytes(), b.bytes());
}

#[test]
fn fnv_test_vectors() {
    // FNV-1a 128-bit reference values (the FNV authors' test vectors).
    assert_eq!(fnv1a_128(&[]), 0x6c62272e07bb014262b821756295c58d);
    assert_eq!(fnv1a_128(&[b""]), 0x6c62272e07bb014262b821756295c58d);
    assert_eq!(fnv1a_128(&[b"a"]), 0xd228cb696f1a8caf78912b704e4a8964);
    assert_eq!(fnv1a_128(&[b"foobar"]), 0x343e1662793c64bf6f0d3597ba446f18);
    // Chunks hash as their concatenation.
    assert_eq!(fnv1a_128(&[b"foo", b"bar"]), fnv1a_128(&[b"foobar"]));
    // UUID version 8 (RFC 9562): version nibble 8, variant bits 10.
    assert_eq!(uuid_v8(0), "00000000-0000-8000-8000-000000000000");
    assert_eq!(uuid_v8(u128::MAX), "ffffffff-ffff-8fff-bfff-ffffffffffff");
    assert_eq!(
        uuid_v8(0x343e1662793c64bf6f0d3597ba446f18),
        "343e1662-793c-84bf-af0d-3597ba446f18"
    );
}

#[test]
fn memory_is_one_chapter_at_a_time() {
    // 256 chapters of ~32 KiB each: ~8 MiB of chapter XHTML in all.
    const CHAPTERS: usize = 256;
    let body = "word ".repeat(32 * 1024 / 5);
    let src: String = (1..=CHAPTERS)
        .map(|i| format!("# Chapter {i}\n\n{body}\n\n"))
        .collect();
    let d = md(&src);
    let m = meta("Big");
    let s = Scratch::new();
    let f = s.create();
    let (r, peak) = peak_of(|| write(&d, &m, f, Limits::DEFAULT));
    r.unwrap();
    let total: usize = entries(&s.0)
        .iter()
        .filter(|(n, _)| n.ends_with(".xhtml"))
        .map(|(_, v)| v.len())
        .sum();
    let largest = body.len() + 1024;
    println!("memory_is_one_chapter_at_a_time: {total} B of XHTML, peak {peak} B");
    assert!(total > 8_000_000, "{total}");
    // A chapter's string may hold twice its length while it grows, plus
    // the writer's 64 KiB copy buffer, the anchors and the table of
    // contents: far below the sum of all chapters.
    assert!(
        peak < 4 * largest + 512 * 1024,
        "peak {peak} B, total {total} B"
    );
}
