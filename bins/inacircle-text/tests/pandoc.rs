//! Public-domain EPUBs, converted by `text.epub-to-txt`: every book reads
//! cleanly (runs everywhere), and its text has the same word sequence as
//! pandoc's reading of the same book (runs where `pandoc` is installed;
//! `INACIRCLE_REQUIRE_BOOK_TOOLS=1` makes a missing one a failure).
//!
//! The corpus is in `tests/data/books/`; `SOURCES` there names each
//! book's origin, retrieval date, SHA-256 and licence.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One corpus book and what is known to differ from pandoc's reading.
struct Book {
    /// File name in `tests/data/books/`.
    file: &'static str,
    /// The head `text.epub-to-txt` writes: the title line, then the author
    /// line (what the book's package document names).
    head: &'static str,
    /// The `warn` messages of the conversion, in order (pinned: a change
    /// is a change of what the conversion reports and must be looked at).
    warns: &'static [&'static str],
    /// Word-sequence divergences from pandoc, each with its reason.
    diverges: &'static [Divergence],
}

/// A divergence: the words (normalised, see `words`) only we have, and
/// the words only pandoc has, at `times` places in the book.
struct Divergence {
    ours: &'static [&'static str],
    theirs: &'static [&'static str],
    times: usize,
    why: &'static str,
}

/// What every book reports: the markup our text drops. Same for all books
/// of the corpus because they all carry emphasis, links, an image or an
/// alt text, headings and a stylesheet.
const BASE_WARNS: &[&str] = &[
    "emphasis removed",
    "links flattened to text",
    "images replaced by their alt text",
    "heading levels lost",
    "book styles (fonts, colours, layout) not carried over",
];

/// The same plus the table note (one book has a table with spanning cells).
const TABLE_WARNS: &[&str] = &[
    "emphasis removed",
    "links flattened to text",
    "images replaced by their alt text",
    "heading levels lost",
    "book styles (fonts, colours, layout) not carried over",
    "merged table cells split",
];

const BOOKS: &[Book] = &[
    // The Prince has footnotes; its divergences from pandoc (where each
    // writer puts a note) are triaged from the CI run, so none is listed yet.
    Book {
        file: "the-prince-epub3.epub",
        head: "The Prince\nNiccolò Machiavelli",
        warns: BASE_WARNS,
        diverges: &[],
    },
    Book {
        file: "alice-epub2.epub",
        head: "Alice's Adventures in Wonderland\nLewis Carroll",
        warns: BASE_WARNS,
        diverges: &[],
    },
    Book {
        file: "alice-epub3.epub",
        head: "Alice's Adventures in Wonderland\nLewis Carroll",
        warns: BASE_WARNS,
        diverges: &[],
    },
    Book {
        file: "frankenstein-epub3.epub",
        head: "Frankenstein; or, The modern Prometheus\nMary Wollstonecraft Shelley",
        warns: BASE_WARNS,
        diverges: &[],
    },
    Book {
        file: "pride-and-prejudice-epub2.epub",
        head: "Pride and Prejudice\nJane Austen",
        warns: TABLE_WARNS,
        diverges: &[Divergence {
            ours: &["house", "156", "charing"],
            theirs: &["156", "charing", "house"],
            times: 1,
            why: "title-page table: the cells hold <br/> breaks (\"Ruskin<br/>House.\" | \"156. Charing<br/>Cross Road.\"); \
                  pandoc keeps the break inside a cell and prints the row line by line, \
                  interleaving the cells' lines, while our writer flattens each cell to one \
                  line: the same words in a different order",
        }],
    },
];

// ---------- the comparison (pure; tested below on fixed strings) ----------

/// Our text without the head `text.epub-to-txt` writes: the title and
/// author lines up to the first blank line (the output begins with them
/// when the book names a title or an author; these books do).
fn strip_header(text: &str) -> &str {
    match text.find("\n\n") {
        Some(i) if !text.starts_with('\n') => &text[i + 2..],
        _ => text,
    }
}

/// The head that `strip_header` drops (empty when there is none); each
/// book's must equal its pinned `head`, so a garbled or extra head line
/// fails instead of vanishing from the comparison.
fn head(text: &str) -> &str {
    let body = strip_header(text);
    text[..text.len() - body.len()].trim_end_matches('\n')
}

/// Our text without the lines that stand for a chapter that could not be
/// read (`[Chapter N could not be read: <reason>]`); pandoc has no
/// counterpart. A corpus book has none (`books_convert_cleanly`), the
/// function keeps the comparison honest for books that do.
fn strip_placeholders(text: &str) -> String {
    text.lines()
        .filter(|l| {
            !(l.starts_with("[Chapter ") && l.contains(" could not be read: ") && l.ends_with(']'))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The word sequence of `text`: split on Unicode whitespace; each word is
/// lower-cased and reduced to its letters and digits, and a word with
/// none left (a bullet, `>`, `----`) is dropped. This leaves out what the
/// two writers legitimately render differently (emphasis marks `_x_`,
/// pandoc's upper-cased level-1 headings, quote and list markers,
/// punctuation spacing) while the words and their order must agree. With
/// `drop_urls`, our ` (https://…)` that follows a link whose text is not
/// the address is dropped first (pandoc's plain text keeps only the link
/// text).
fn words(text: &str, drop_urls: bool) -> Vec<String> {
    text.split_whitespace()
        .filter(|w| {
            !(drop_urls
                && (w.starts_with("(http://")
                    || w.starts_with("(https://")
                    || w.starts_with("(mailto:")))
        })
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// A place where the two word sequences differ.
#[derive(Debug, PartialEq)]
struct Hunk {
    /// Word index in our sequence.
    at: usize,
    ours: Vec<String>,
    theirs: Vec<String>,
    /// No agreement was found again within `WINDOW` words: the rest of
    /// both sequences is in this hunk.
    unresolved: bool,
}

/// Words that must agree again for two sequences to count as back in step
/// after a divergence (a single equal word is chance: "the").
const RUN: usize = 8;

/// The most words, on both sides together, that a divergence may span
/// when looking for the next agreement (a one-sided gap of up to this many
/// words resolves; so do two gaps adding up to it). A divergence past it
/// (a whole page of words that only one side has, or both sides replaced)
/// is `unresolved`: it fails the book with the position instead of the
/// search growing quadratic in the book's length.
const WINDOW: usize = 2048;

/// The divergences between two word sequences, in order.
fn hunks(a: &[String], b: &[String]) -> Vec<Hunk> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    loop {
        while i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
        }
        if i == a.len() && j == b.len() {
            return out;
        }
        let back_in_step = |x: usize, y: usize| {
            let (p, q) = (i + x, j + y);
            (p == a.len() && q == b.len())
                || (a.len() - p.min(a.len()) >= RUN
                    && b.len() - q.min(b.len()) >= RUN
                    && a[p..p + RUN] == b[q..q + RUN])
        };
        // The smallest total skip first, so a short hunk is not widened.
        let found = (1..=WINDOW).find_map(|d| {
            (0..=d).find_map(|x| {
                let y = d - x;
                (i + x <= a.len() && j + y <= b.len() && back_in_step(x, y)).then_some((x, y))
            })
        });
        match found {
            Some((x, y)) => {
                out.push(Hunk {
                    at: i,
                    ours: a[i..i + x].to_vec(),
                    theirs: b[j..j + y].to_vec(),
                    unresolved: false,
                });
                i += x;
                j += y;
            }
            None => {
                out.push(Hunk {
                    at: i,
                    ours: a[i..].to_vec(),
                    theirs: b[j..].to_vec(),
                    unresolved: true,
                });
                return out;
            }
        }
    }
}

/// The hunks no divergence explains, and the divergences whose count did
/// not come out exactly (a stale entry is as wrong as a missing one).
fn unexplained<'a>(hs: &'a [Hunk], known: &'static [Divergence]) -> (Vec<&'a Hunk>, Vec<String>) {
    let mut seen = vec![0usize; known.len()];
    let mut left = Vec::new();
    for h in hs {
        let hit = known.iter().position(|k| {
            !h.unresolved
                && h.ours.iter().map(String::as_str).eq(k.ours.iter().copied())
                && h.theirs
                    .iter()
                    .map(String::as_str)
                    .eq(k.theirs.iter().copied())
        });
        match hit {
            Some(n) => seen[n] += 1,
            None => left.push(h),
        }
    }
    let wrong = known
        .iter()
        .zip(&seen)
        .filter(|(k, n)| k.times != **n)
        .map(|(k, n)| {
            format!(
                "{:?} -> {:?} ({}): expected {} times, found {n}",
                k.ours, k.theirs, k.why, k.times
            )
        })
        .collect();
    (left, wrong)
}

/// A hunk for a failure message: position, a few words each side.
fn show(h: &Hunk) -> String {
    let cut = |v: &[String]| {
        let mut s = v.iter().take(24).cloned().collect::<Vec<_>>().join(" ");
        if v.len() > 24 {
            s.push_str(&format!(" ... ({} words)", v.len()));
        }
        s
    };
    format!(
        "at word {}{}:\n  ours:   {}\n  pandoc: {}",
        h.at,
        if h.unresolved {
            " (not back in step)"
        } else {
            ""
        },
        cut(&h.ours),
        cut(&h.theirs)
    )
}

/// Our word sequence for the text `text.epub-to-txt` wrote.
fn our_words(written: &str) -> Vec<String> {
    words(&strip_placeholders(strip_header(written)), true)
}

// ---------- running the tools ----------

fn books_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/books")
}

/// A unique temporary directory, removed on drop.
struct Tmp(PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Every call gets its own directory: the tests of this file run in
/// parallel in one process, and a name shared by two of them (the same
/// book) let one test's `remove_dir_all` delete the other's work dir.
fn tmp(name: &str) -> Tmp {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!(
        "inacircle-pandoc-test-{}-{n}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

/// What the conversion of `book` gave: text and `warn` messages.
fn convert(book: &Book, work: &Path) -> (String, Vec<String>) {
    let path = books_dir().join(book.file);
    let o = Command::new(env!("CARGO_BIN_EXE_inacircle-text"))
        .args(["run", "text.epub-to-txt", "--work"])
        .arg(work)
        .arg("--")
        .arg(&path)
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert_eq!(o.status.code(), Some(0), "{}: {out}", book.file);
    let fields: Vec<Vec<&str>> = out.lines().map(|l| l.split('\t').collect()).collect();
    let done: Vec<_> = fields.iter().filter(|f| f[0] == "done").collect();
    assert_eq!(done.len(), 1, "{}: {out}", book.file);
    assert!(
        fields.iter().all(|f| f[0] != "fail" && f[0] != "skip"),
        "{}: {out}",
        book.file
    );
    let warns = fields
        .iter()
        .filter(|f| f[0] == "warn")
        .map(|f| f[2].to_string())
        .collect();
    let text = fs::read_to_string(work.join(done[0][2])).unwrap();
    (text, warns)
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

#[test]
fn books_convert_cleanly() {
    for book in BOOKS {
        let work = tmp(book.file);
        let (text, warns) = convert(book, &work.0);
        assert!(
            text.len() > 10_000,
            "{}: only {} bytes",
            book.file,
            text.len()
        );
        assert!(
            !text.contains("could not be read"),
            "{}: a chapter could not be read",
            book.file
        );
        assert_eq!(warns, book.warns, "{}", book.file);
        assert_eq!(head(&text), book.head, "{}", book.file);
        assert!(!our_words(&text).is_empty(), "{}", book.file);
    }
}

/// What `text.epub-to-md` gave for `book`: the md, every `done` path and
/// the `warn` messages.
fn convert_md(book: &Book, work: &Path) -> (String, Vec<String>, Vec<String>) {
    let path = books_dir().join(book.file);
    let o = Command::new(env!("CARGO_BIN_EXE_inacircle-text"))
        .args(["run", "text.epub-to-md", "--work"])
        .arg(work)
        .arg("--")
        .arg(&path)
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert_eq!(o.status.code(), Some(0), "{}: {out}", book.file);
    let fields: Vec<Vec<&str>> = out.lines().map(|l| l.split('\t').collect()).collect();
    assert!(
        fields.iter().all(|f| f[0] != "fail" && f[0] != "skip"),
        "{}: {out}",
        book.file
    );
    let done: Vec<String> = fields
        .iter()
        .filter(|f| f[0] == "done")
        .map(|f| f[2].to_string())
        .collect();
    let warns = fields
        .iter()
        .filter(|f| f[0] == "warn")
        .map(|f| f[2].to_string())
        .collect();
    let md = done.last().expect("a done line");
    assert!(md.ends_with(".md"), "{}: {out}", book.file);
    let text = fs::read_to_string(work.join(md)).unwrap();
    (text, done, warns)
}

/// Every corpus book converts to Markdown too, with its images beside it,
/// and the md read back (GFM) as plain text has the word sequence of the
/// book's own plain text: the two readings of a book agree. The in-book
/// links of the md (` (#id)` in plain text) are left out of the words.
#[test]
fn books_convert_to_md_and_read_back_to_the_txt_words() {
    for book in BOOKS {
        let txt_work = tmp(book.file);
        let (text, _) = convert(book, &txt_work.0);
        let work = tmp(book.file);
        let (md, done, warns) = convert_md(book, &work.0);
        assert!(!md.contains("could not be read"), "{}", book.file);
        assert!(
            warns.iter().all(|w| !w.contains("could not be read")),
            "{}: {warns:?}",
            book.file
        );
        // Every image the md names is a file beside it.
        for d in &done[..done.len() - 1] {
            assert!(work.0.join(d).is_file(), "{}: {d}", book.file);
            let name = Path::new(d).file_name().unwrap().to_string_lossy();
            assert!(
                md.contains(name.as_ref()),
                "{}: {name} not linked",
                book.file
            );
        }
        let doc =
            doc::md::read_limited_with(md.as_bytes(), doc::Limits::DEFAULT, doc::Dialect::Gfm)
                .unwrap();
        let (back, _) = doc::txt::write(&doc);
        let strip = |t: &str| -> String {
            t.split_whitespace()
                .filter(|w| !w.starts_with("(#"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let (ours, theirs) = (words(&strip(&back), false), words(&strip(&text), false));
        let hs = hunks(&ours, &theirs);
        assert!(
            hs.is_empty(),
            "{}: {}",
            book.file,
            hs.iter().map(show).collect::<Vec<_>>().join("\n")
        );
    }
}

#[test]
fn sources_file_names_every_book() {
    let sources = fs::read_to_string(books_dir().join("SOURCES")).unwrap();
    for book in BOOKS {
        assert!(
            sources.contains(book.file),
            "{} is not in SOURCES",
            book.file
        );
    }
    let files = fs::read_dir(books_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".epub"));
    for f in files {
        assert!(BOOKS.iter().any(|b| b.file == f), "{f} is not in BOOKS");
    }
}

#[test]
fn books_match_pandoc_word_sequence() {
    if !have("pandoc") {
        return;
    }
    let version = Command::new("pandoc").arg("--version").output().unwrap();
    println!(
        "pandoc: {}",
        String::from_utf8_lossy(&version.stdout)
            .lines()
            .next()
            .unwrap_or("?")
    );
    let mut failures = Vec::new();
    for book in BOOKS {
        let work = tmp(book.file);
        let (text, _) = convert(book, &work.0);
        assert_eq!(head(&text), book.head, "{}", book.file);
        // `--sandbox`: pandoc reads the book and nothing else.
        let o = Command::new("pandoc")
            .args(["--sandbox", "-f", "epub", "-t", "plain", "--wrap=none"])
            .arg(books_dir().join(book.file))
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "pandoc failed on {}: {}",
            book.file,
            String::from_utf8_lossy(&o.stderr)
        );
        let theirs = words(&String::from_utf8_lossy(&o.stdout), false);
        let ours = our_words(&text);
        assert!(
            theirs.len() > 1000,
            "{}: pandoc gave {} words",
            book.file,
            theirs.len()
        );
        let hs = hunks(&ours, &theirs);
        let (left, wrong) = unexplained(&hs, book.diverges);
        println!(
            "{}: {} words ours, {} pandoc, {} divergences ({} unexplained)",
            book.file,
            ours.len(),
            theirs.len(),
            hs.len(),
            left.len()
        );
        if !left.is_empty() || !wrong.is_empty() {
            let mut msg = format!("{}:", book.file);
            for h in left.iter().take(40) {
                msg.push_str("\n ");
                msg.push_str(&show(h));
            }
            if left.len() > 40 {
                msg.push_str(&format!("\n ... and {} more", left.len() - 40));
            }
            for w in wrong {
                msg.push_str("\n stale or miscounted divergence: ");
                msg.push_str(&w);
            }
            failures.push(msg);
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

// ---------- the comparison logic, on fixed strings ----------

#[cfg(test)]
mod cmp {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        words(s, false)
    }

    #[test]
    fn head_is_what_strip_header_drops() {
        assert_eq!(head("Title\nA, B\n\nbody\n"), "Title\nA, B");
        assert_eq!(head("\nbody\n"), "");
        assert_eq!(head("only one line"), "");
    }

    #[test]
    fn header_is_the_lines_up_to_the_first_blank_line() {
        assert_eq!(
            strip_header("Title\nA, B\n\nbody\n\nmore\n"),
            "body\n\nmore\n"
        );
        // No head (a book with neither title nor author) starts with the body.
        assert_eq!(strip_header("\nbody\n"), "\nbody\n");
        assert_eq!(strip_header("only one line"), "only one line");
    }

    #[test]
    fn placeholders_are_dropped_and_other_brackets_kept() {
        let t =
            "one\n[Chapter 2 could not be read: missing]\ntwo\n[Chapter 3 is nice]\n[Chapter x]\n";
        assert_eq!(
            strip_placeholders(t),
            "one\ntwo\n[Chapter 3 is nice]\n[Chapter x]"
        );
        let t = "[Chapter 12 could not be read: damaged (bad crc)]";
        assert_eq!(strip_placeholders(t), "");
    }

    #[test]
    fn words_split_on_unicode_whitespace_and_ignore_markup() {
        // NO-BREAK SPACE, IDEOGRAPHIC SPACE and a tab all separate words.
        assert_eq!(w("a\u{a0}b\u{3000}c\td"), ["a", "b", "c", "d"]);
        // Emphasis marks, case, quote and list markers, rules, punctuation.
        assert_eq!(w("> _Hello_, WORLD! ---- - * 1."), ["hello", "world", "1"]);
        // Letters with accents and non-Latin scripts stay words.
        assert_eq!(w("Café ÉCOLE Ελλάδα"), ["café", "école", "ελλάδα"]);
        assert!(w(" \n\t ").is_empty());
    }

    #[test]
    fn our_url_after_a_link_is_dropped_but_a_url_in_the_text_is_kept() {
        let ours =
            "see the site (https://x.org/a) or http://y.org, mail (mailto:a@b.c) (not a url)";
        assert_eq!(
            words(ours, true),
            [
                "see", "the", "site", "or", "httpyorg", "mail", "not", "a", "url"
            ]
        );
        // pandoc's side is never filtered.
        assert!(words("(https://x.org)", false) == ["httpsxorg"]);
    }

    #[test]
    fn our_words_strip_header_and_placeholders_first() {
        let t =
            "Title\nAuthor\n\nfirst words\n[Chapter 2 could not be read: missing]\nlast words\n";
        assert_eq!(our_words(t), ["first", "words", "last", "words"]);
    }

    fn seq(n: usize, tag: &str) -> Vec<String> {
        (0..n).map(|i| format!("{tag}{i}")).collect()
    }

    #[test]
    fn equal_sequences_have_no_hunk() {
        let a = seq(100, "w");
        assert!(hunks(&a, &a).is_empty());
        assert!(hunks(&[], &[]).is_empty());
    }

    #[test]
    fn an_extra_a_missing_and_a_changed_word_are_found_in_place() {
        let base = seq(60, "w");
        let mut extra = base.clone();
        extra.insert(20, "x".into());
        let h = hunks(&extra, &base);
        assert_eq!(h.len(), 1);
        assert_eq!(
            (h[0].at, h[0].ours.clone(), h[0].theirs.clone()),
            (20, vec!["x".to_string()], vec![])
        );
        let h = hunks(&base, &extra);
        assert_eq!(h.len(), 1);
        assert_eq!(
            (h[0].ours.len(), h[0].theirs.clone()),
            (0, vec!["x".to_string()])
        );
        let mut changed = base.clone();
        changed[30] = "y".into();
        let h = hunks(&base, &changed);
        assert_eq!(h.len(), 1);
        assert_eq!(
            (h[0].at, h[0].ours.clone(), h[0].theirs.clone()),
            (30, vec!["w30".to_string()], vec!["y".to_string()])
        );
    }

    #[test]
    fn several_hunks_and_tails_are_all_listed() {
        let base = seq(80, "w");
        let mut a = base.clone();
        a.insert(10, "p".into());
        a.remove(41); // base word 40 is missing (shifted by the insert)
        a.push("tail".into());
        let h = hunks(&a, &base);
        assert_eq!(h.len(), 3, "{h:?}");
        assert_eq!(h[2].ours, ["tail"]);
        assert!(h[2].theirs.is_empty());
        // The other way: we stop early.
        let h = hunks(&base[..70], &base);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].theirs.len(), 10);
    }

    #[test]
    fn a_divergence_longer_than_the_window_is_unresolved() {
        // Nothing in common and more than WINDOW apart: the search gives up
        // and says where, with the rest of both sides.
        let a = seq(WINDOW, "a");
        let b = seq(WINDOW, "b");
        let h = hunks(&a, &b);
        assert_eq!(h.len(), 1);
        assert!(h[0].unresolved);
        assert_eq!(
            (h[0].at, h[0].ours.len(), h[0].theirs.len()),
            (0, WINDOW, WINDOW)
        );
    }

    #[test]
    fn a_single_equal_word_inside_a_divergence_is_not_agreement() {
        // One shared word is chance; the hunk stays one (RUN in a row needed).
        let mut c = seq(30, "c");
        let mut d = seq(30, "d");
        c[5] = "same".into();
        d[5] = "same".into();
        let h = hunks(&c, &d);
        assert_eq!(h.len(), 1, "{h:?}");
        assert_eq!((h[0].ours.len(), h[0].theirs.len()), (30, 30));
        // RUN equal words in a row are agreement and split the hunk.
        let mut c = seq(40, "c");
        let mut d = seq(40, "d");
        for k in 10..10 + RUN {
            c[k] = format!("s{k}");
            d[k] = format!("s{k}");
        }
        let h = hunks(&c, &d);
        assert_eq!(h.len(), 2, "{h:?}");
        assert_eq!((h[0].at, h[0].ours.len(), h[0].theirs.len()), (0, 10, 10));
    }

    const KNOWN: &[Divergence] = &[Divergence {
        ours: &["x"],
        theirs: &[],
        times: 2,
        why: "test",
    }];

    #[test]
    fn known_divergences_are_matched_by_content_and_exact_count() {
        let base = seq(80, "w");
        let mut a = base.clone();
        a.insert(10, "x".into());
        a.insert(50, "x".into());
        let hs = hunks(&a, &base);
        let (left, wrong) = unexplained(&hs, KNOWN);
        assert!(left.is_empty() && wrong.is_empty(), "{left:?} {wrong:?}");
        // A different word is not explained.
        a[10] = "z".into();
        let hs = hunks(&a, &base);
        let (left, wrong) = unexplained(&hs, KNOWN);
        assert_eq!(left.len(), 1);
        // And the entry is now counted once instead of twice: stale.
        assert_eq!(wrong.len(), 1, "{wrong:?}");
        // An entry that never occurs is reported as wrong.
        let (left, wrong) = unexplained(&[], KNOWN);
        assert!(left.is_empty());
        assert_eq!(wrong.len(), 1);
    }

    #[test]
    fn an_unresolved_hunk_is_never_explained() {
        let hs = vec![Hunk {
            at: 0,
            ours: vec!["x".into()],
            theirs: vec![],
            unresolved: true,
        }];
        let (left, _) = unexplained(&hs, KNOWN);
        assert_eq!(left.len(), 1);
    }

    #[test]
    fn show_names_the_position_and_cuts_long_hunks() {
        let h = Hunk {
            at: 7,
            ours: seq(30, "o"),
            theirs: vec![],
            unresolved: false,
        };
        let s = show(&h);
        assert!(s.starts_with("at word 7:"), "{s}");
        assert!(s.contains("(30 words)"), "{s}");
    }
}
