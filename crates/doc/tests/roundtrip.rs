//! txt -> md -> read must not change the text. The Markdown writer escapes
//! everything that could be read as structure; these tests prove it on
//! fixtures and on a deterministic pseudo-random corpus.

use doc::{Block, Dialect, Document, Inline, Loss, md, txt};
use std::collections::BTreeSet;

/// Inline text concatenated, `HardBreak` = "\n"; blocks separated by a blank
/// line. Anything else (a soft break, emphasis, a nested block) is rendered
/// as a visible marker so it cannot hide a difference.
fn extract(doc: &Document) -> String {
    let mut parts = Vec::new();
    for b in &doc.blocks {
        let mut s = String::new();
        match b {
            Block::Paragraph(c) => inlines(c, &mut s),
            other => s.push_str(&format!("<<{other:?}>>")),
        }
        parts.push(s);
    }
    parts.join("\n\n")
}

fn inlines(c: &[Inline], out: &mut String) {
    for i in c {
        match i {
            Inline::Text(t) => out.push_str(t),
            Inline::HardBreak => out.push('\n'),
            other => out.push_str(&format!("<<{other:?}>>")),
        }
    }
}

/// Every inline list of `doc`, depth first.
fn inline_lists(blocks: &mut [Block], f: &mut impl FnMut(&mut Vec<Inline>)) {
    for b in blocks {
        match b {
            Block::Paragraph(c) | Block::Heading { content: c, .. } => f(c),
            Block::BlockQuote(bs) => inline_lists(bs, f),
            Block::List { items, .. } => {
                for i in items {
                    inline_lists(&mut i.blocks, f);
                }
            }
            Block::Table { head, rows, .. } => {
                head.iter_mut()
                    .chain(rows.iter_mut().flatten())
                    .for_each(&mut *f);
            }
            Block::CodeBlock { .. } | Block::ThematicBreak | Block::HtmlBlock(_) => {}
        }
    }
}

/// Whether a GFM extended autolink of `doc` holds a backslash or an `&`:
/// an escape of the Markdown writer (it writes every `&` of the text as
/// `\&`, and `&` starts the entities it writes).
fn has_escaped_autolink(doc: &Document) -> bool {
    fn any(c: &[Inline]) -> bool {
        c.iter().any(|i| match i {
            Inline::Link {
                auto: true,
                content,
                ..
            } => content
                .iter()
                .any(|t| matches!(t, Inline::Text(t) if t.contains(['\\', '&']))),
            Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => any(c),
            Inline::Link { content: c, .. } | Inline::Image { alt: c, .. } => any(c),
            _ => false,
        })
    }
    let mut found = false;
    inline_lists(&mut doc.blocks.clone(), &mut |c| found |= any(c));
    found
}

/// Replaces every GFM extended autolink by its text, merged with the text
/// around it.
fn unlink_autolinks(doc: &mut Document) {
    fn unlink(c: &mut Vec<Inline>) {
        let mut out: Vec<Inline> = Vec::new();
        for i in c.drain(..) {
            let pieces = match i {
                Inline::Link {
                    auto: true,
                    content,
                    ..
                } => content,
                mut other => {
                    if let Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) =
                        &mut other
                    {
                        unlink(c);
                    }
                    vec![other]
                }
            };
            for p in pieces {
                match (out.last_mut(), p) {
                    (Some(Inline::Text(a)), Inline::Text(b)) => a.push_str(&b),
                    (_, p) => out.push(p),
                }
            }
        }
        *c = out;
    }
    inline_lists(&mut doc.blocks, &mut unlink);
}

/// Whether the text has an interior run of two or more spaces/tabs, the only
/// thing the Markdown writer may report as `Whitespace`.
fn has_ws_run(s: &str) -> bool {
    ["  ", "\t\t", " \t", "\t "].iter().any(|p| s.contains(p))
}

/// Whether the text has a blank-line run, a blank line before the first or
/// after the last paragraph, or a whitespace-only line: what the txt reader
/// may report as `BlankLines`.
fn has_blank_run(s: &str) -> bool {
    let norm = s.replace("\r\n", "\n").replace('\r', "\n");
    let norm = norm.strip_suffix('\n').unwrap_or(&norm);
    let lines: Vec<&str> = norm.split('\n').collect();
    let blank = |l: &str| l.trim().is_empty();
    blank(lines[0])
        || blank(lines[lines.len() - 1])
        || lines.iter().any(|l| !l.is_empty() && blank(l))
        || lines.windows(2).any(|w| blank(w[0]) && blank(w[1]))
}

/// `None` when the round trip is lossless, else a description.
fn check(input: &str) -> Option<String> {
    check_with(input, Dialect::CommonMark).or_else(|| check_with(input, Dialect::Gfm))
}

fn check_with(input: &str, dialect: Dialect) -> Option<String> {
    let (d1, txt_losses) = txt::read(input.as_bytes()).unwrap();
    let (md_text, md_losses) = md::write(&d1);
    let mut d2 = md::read_with(md_text.as_bytes(), dialect).unwrap();
    if dialect == Dialect::Gfm {
        // A bare URL or address is a GFM extended autolink: the same text,
        // so it is compared as its text. Known limit of the Markdown
        // writer (it writes CommonMark): the backslash it puts before a
        // `_ * & # [ ] < > \` in a bare URL, or the entity it writes for a
        // space it must keep, lands inside the autolink a GFM reader makes
        // of it, and shows. Only that is not compared;
        // `known_limit_escapes_inside_a_gfm_autolink` pins it.
        if has_escaped_autolink(&d2) {
            return None;
        }
        unlink_autolinks(&mut d2);
    }
    let (e1, e2) = (extract(&d1), extract(&d2));
    if e1 != e2 {
        return Some(format!(
            "text changed\n  md:   {md_text:?}\n  want: {e1:?}\n  got:  {e2:?}"
        ));
    }
    if d1 != d2 {
        return Some(format!(
            "tree changed\n  md:   {md_text:?}\n  want: {d1:?}\n  got:  {d2:?}"
        ));
    }
    for l in &md_losses {
        let allowed = *l == Loss::Whitespace && has_ws_run(input);
        if !allowed {
            return Some(format!("unexpected md loss {l:?} for md {md_text:?}"));
        }
    }
    for l in &txt_losses {
        let allowed = *l == Loss::BlankLines && has_blank_run(input);
        if !allowed {
            return Some(format!("unexpected txt loss {l:?}"));
        }
    }
    // Writing the re-read tree as text gives the same text again.
    let (t1, _) = txt::write(&d1);
    let (t2, _) = txt::write(&d2);
    if t1 != t2 {
        return Some(format!("txt output changed: {t1:?} vs {t2:?}"));
    }
    None
}

fn assert_round_trips(input: &str) {
    if let Some(why) = check(input) {
        panic!("round trip failed for {input:?}: {why}");
    }
}

#[test]
fn tildes_survive_txt_to_md_under_gfm() {
    for input in [
        "~5 min",
        "~~kész~~",
        "wait ~5 min, then ~~kész~~ and ~~~three~~~ ok",
        "~~a\n~~b~~\n~",
        "- ~~x~~\n- ~y",
    ] {
        let (d1, _) = txt::read(input.as_bytes()).unwrap();
        let (md_text, _) = md::write(&d1);
        let d2 = md::read_with(md_text.as_bytes(), Dialect::Gfm).unwrap();
        assert_eq!(
            txt::write(&d2).0,
            txt::write(&d1).0,
            "{input:?} via {md_text:?}"
        );
        assert_eq!(d1, d2, "{input:?} via {md_text:?}");
        assert_round_trips(input);
    }
}

/// The known limit `check_with` leaves out: in a bare URL the Markdown
/// writer escapes what CommonMark would read as syntax, and a GFM reader
/// keeps the backslash inside its autolink. A Markdown writer for GFM
/// would write the URL as it is.
#[test]
fn known_limit_escapes_inside_a_gfm_autolink() {
    let (d1, _) = txt::read(b"see http://x.y/a_b now").unwrap();
    let (md_text, _) = md::write(&d1);
    assert_eq!(md_text, "see http://x.y/a\\_b now\n");
    let d2 = md::read_with(md_text.as_bytes(), Dialect::Gfm).unwrap();
    assert!(has_escaped_autolink(&d2));
    // Without an escape the URL comes back as the same text.
    let (d1, _) = txt::read(b"see http://x.y/a-b now").unwrap();
    let mut d2 = md::read_with(md::write(&d1).0.as_bytes(), Dialect::Gfm).unwrap();
    assert!(!has_escaped_autolink(&d2) && d1 != d2);
    unlink_autolinks(&mut d2);
    assert_eq!(d1, d2);
}

#[test]
fn txt_md_txt_text_equal() {
    let fixtures = [
        "plain",
        "two\nlines",
        "para one\n\npara two",
        "para one\n\n\n\npara two",
        "crlf\r\nlines\r\n\r\nnext",
        "lone\rcr",
        "\u{feff}bom first",
        "  leading spaces\n\tleading tab\n    four spaces",
        "trailing spaces  \nmore\t",
        "inner   spaces  and\t\ttabs",
        "see https://example.com/a_b*c?x=1&y=2#frag (now)",
        "<https://example.com> and <b>x</b> &amp; &#35; &copy;",
        "*star* _under_ **bold** __bold__ `code` ``a`b``",
        "# heading\n## two\n#hash",
        "text\n# heading in paragraph",
        "1. one\n2) two\n10. ten\n123456789. big\n1234567890. toolong",
        "3\\.x and 1\\. back",
        "> quote\n>> nested\ntext > mid",
        "[link](http://x) ![img](y) [ref]\n\n[ref]: http://z",
        "a | b\n--- | ---\n1 | 2",
        "- dash\n+ plus\n* star\n-dash\n+plus",
        "---\n***\n___\n- - -\n===\n~~~\n```\n```rust",
        "para\n---",
        "para\n===",
        "para\n\n---\n\npara",
        "title\n---\n",
        "\\ backslash \\* \\\\ end\\",
        "<!-- comment -->\n<div>\nhtml\n</div>",
        "    indented code\n\ttab code",
        "a\\\nb",
        "line with trailing backslash\\\nnext",
        "[x]: /url \"title\"",
        "![alt\n",
        "| a | b |\n|---|---|",
        "# \n#\n- \n-\n1.\n>",
        "~~~~\ntilde",
        "tab\tinside",
        "emoji \u{1f600} and caf\u{e9}",
        "",
        "\n\n\n",
        "   \n \t \n",
        "x",
    ];
    for f in fixtures {
        assert_round_trips(f);
    }
}

#[test]
fn unicode_whitespace_at_line_edges() {
    for ws in [
        '\u{a0}', '\u{2003}', '\u{3000}', '\u{2009}', '\u{85}', '\u{b}', '\u{c}',
    ] {
        for input in [
            format!("{ws}start"),
            format!("end{ws}"),
            format!("a\n{ws}b"),
            format!("a{ws}\nb"),
            format!("{ws}{ws}x{ws}{ws}"),
            format!("a{ws}b"),
            format!("{ws}\u{20}{ws}y"),
            format!("x\n\n{ws}z{ws}\n\n{ws}w"),
            format!("x{ws}"),
        ] {
            assert_round_trips(&input);
        }
    }
}

#[test]
fn md_txt_reports_losses() {
    let src = "*em* **strong** [site](https://e.com) ![pic](p.png) `code`\n";
    let d = md::read(src.as_bytes()).unwrap();
    let (text, losses) = txt::write(&d);
    assert_eq!(
        losses,
        BTreeSet::from([Loss::Emphasis, Loss::Links, Loss::Images, Loss::InlineCode])
    );
    assert_eq!(text, "em strong site (https://e.com) pic code\n");

    let (_, l) = txt::write(&md::read(b"plain\n\ntext").unwrap());
    assert!(l.is_empty());
    let (_, l) = txt::write(&md::read(b"# h\n\n<div>x</div>\n\n## h2").unwrap());
    assert_eq!(l, BTreeSet::from([Loss::HeadingLevels, Loss::Html]));
}

/// SplitMix64: tiny, deterministic, good enough for test input.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const ALPHABET: &[&str] = &[
    "*",
    "_",
    "#",
    "1.",
    "2)",
    ">",
    "`",
    "[",
    "]",
    "<",
    "&",
    "|",
    "-",
    "+",
    "---",
    "===",
    "~",
    "!",
    "\\",
    "(",
    ")",
    "\"",
    "a",
    "b",
    "z",
    "0",
    "9",
    "http://x.y/",
    "&amp;",
    " ",
    " ",
    "  ",
    "\t",
    "\u{a0}",
    "\u{2003}",
    "\u{3000}",
    "\n",
    "\n",
    "\n\n",
    "\r\n",
    "é",
];

/// Drops one piece at a time while the failure persists.
fn minimize(pieces: &mut Vec<&'static str>) {
    let mut i = 0;
    while i < pieces.len() {
        let mut shorter = pieces.clone();
        shorter.remove(i);
        if check(&shorter.concat()).is_some() {
            *pieces = shorter;
        } else {
            i += 1;
        }
    }
}

#[test]
fn random_inputs_round_trip() {
    let mut rng = Rng(0x1AC1_2C1E);
    for case in 0..4000 {
        let len = 1 + rng.below(24);
        let mut pieces: Vec<&'static str> = (0..len)
            .map(|_| ALPHABET[rng.below(ALPHABET.len())])
            .collect();
        if check(&pieces.concat()).is_some() {
            minimize(&mut pieces);
            let min = pieces.concat();
            panic!(
                "case {case}: round trip failed for minimal input {min:?}: {}",
                check(&min).unwrap()
            );
        }
    }
}
