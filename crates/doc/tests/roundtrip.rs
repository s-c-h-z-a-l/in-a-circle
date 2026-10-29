//! txt -> md -> read must not change the text. The Markdown writer escapes
//! everything that could be read as structure; these tests prove it on
//! fixtures and on a deterministic pseudo-random corpus.

use doc::{Block, Document, Inline, Loss, md, txt};
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
    let (d1, txt_losses) = txt::read(input.as_bytes()).unwrap();
    let (md_text, md_losses) = md::write(&d1);
    let d2 = md::read(md_text.as_bytes()).unwrap();
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
