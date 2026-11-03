//! txt -> md -> read must not change the text. The Markdown writer escapes
//! everything that could be read as structure; these tests prove it on
//! fixtures and on a deterministic pseudo-random corpus.

use doc::{Block, Dialect, Document, Inline, Limits, Loss, md, txt};
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

/// `t` as a CommonMark reader reads its backslash escapes and numeric
/// character references (the writer writes every `&` of the text as `\&`,
/// so a named one is never its own), and whether `t` ends in a backslash
/// with nothing after it to escape.
fn cm_unescape(t: &str) -> (String, bool) {
    let mut out = String::new();
    let mut rest = t;
    while let Some(c) = rest.chars().next() {
        if c == '\\' {
            match rest[1..].chars().next() {
                Some(n) if n.is_ascii_punctuation() => {
                    out.push(n);
                    rest = &rest[2..];
                    continue;
                }
                None => return (out + "\\", true),
                _ => {}
            }
        } else if let Some(r) = rest.strip_prefix("&#") {
            let (hex, digits) = match r.strip_prefix(['x', 'X']) {
                Some(h) => (true, h),
                None => (false, r),
            };
            let n = digits
                .bytes()
                .take_while(|c| {
                    if hex {
                        c.is_ascii_hexdigit()
                    } else {
                        c.is_ascii_digit()
                    }
                })
                .count();
            let ch = u32::from_str_radix(&digits[..n], if hex { 16 } else { 10 })
                .ok()
                .and_then(char::from_u32);
            if let Some(ch) = ch.filter(|_| n > 0 && digits[n..].starts_with(';')) {
                out.push(ch);
                rest = &digits[n + 1..];
                continue;
            }
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    (out, false)
}

/// Whether `block` holds a GFM extended autolink that ends in a writer's
/// backslash whose escaped character the reader trimmed off the link (a
/// trailing `_`, `*`, `~` or entity look-alike). That character is then
/// read outside the link without its escape: as a delimiter it can pair
/// with another, as an entity it is decoded. What it was is not in the
/// tree any more, so such a block is not compared.
fn splits_an_escape(block: &Block) -> bool {
    fn any(c: &[Inline]) -> bool {
        c.iter().enumerate().any(|(k, i)| match i {
            Inline::Link {
                auto: true,
                content,
                ..
            } => {
                let dangling = matches!(content.last(), Some(Inline::Text(t)) if cm_unescape(t).1);
                dangling
                    && matches!(c.get(k + 1), Some(Inline::Text(n))
                        if !n.starts_with(char::is_whitespace))
            }
            Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => any(c),
            Inline::Link { content: c, .. } | Inline::Image { alt: c, .. } => any(c),
            _ => false,
        })
    }
    let mut found = false;
    inline_lists(&mut [block.clone()], &mut |c| found |= any(c));
    found
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
/// around it. A link that holds a writer escape (see
/// [`has_escaped_autolink`]) is replaced by what a CommonMark reader makes
/// of its text ([`cm_unescape`]), so only that link is read differently.
fn unlink_autolinks(doc: &mut Document) {
    fn unlink(c: &mut Vec<Inline>) {
        let mut out: Vec<Inline> = Vec::new();
        // The last link ended in a backslash with nothing after it: before
        // a line break that is the writer's hard break, which the link
        // took.
        let mut dangling = false;
        for i in c.drain(..) {
            let (pieces, ends_in_backslash) = match i {
                Inline::Link {
                    auto: true,
                    content,
                    ..
                } => {
                    let mut end = false;
                    let pieces = content
                        .into_iter()
                        .map(|p| match p {
                            Inline::Text(t) => {
                                let (t, d) = cm_unescape(&t);
                                end = d;
                                Inline::Text(t)
                            }
                            other => other,
                        })
                        .collect();
                    (pieces, end)
                }
                Inline::SoftBreak if dangling => {
                    if let Some(Inline::Text(a)) = out.last_mut() {
                        a.pop();
                    }
                    (vec![Inline::HardBreak], false)
                }
                mut other => {
                    if let Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) =
                        &mut other
                    {
                        unlink(c);
                    }
                    (vec![other], false)
                }
            };
            for p in pieces {
                match (out.last_mut(), p) {
                    (Some(Inline::Text(a)), Inline::Text(b)) => a.push_str(&b),
                    (_, p) => out.push(p),
                }
            }
            dangling = ends_in_backslash;
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
    check_written(input, &d1, &txt_losses, &md_text, &md_losses, dialect)
}

/// The checks of [`check_with`] on what the writer made of `d1`.
fn check_written(
    input: &str,
    d1: &Document,
    txt_losses: &BTreeSet<Loss>,
    md_text: &str,
    md_losses: &BTreeSet<Loss>,
    dialect: Dialect,
) -> Option<String> {
    let mut d1 = d1.clone();
    let mut d2 = md::read_with(md_text.as_bytes(), dialect).unwrap();
    if dialect == Dialect::Gfm {
        // A bare URL or address is a GFM extended autolink: the same text,
        // so it is compared as its text. Known limit of the Markdown
        // writer (it writes CommonMark): the backslash it puts before a
        // `_ * & # [ ] < > \` in a bare URL, or the entity it writes for a
        // space it must keep, lands inside the autolink a GFM reader makes
        // of it, and shows. Such a link is compared as the text a
        // CommonMark reader makes of it, the rest of the document as it
        // is; `known_limit_escapes_inside_a_gfm_autolink` pins it.
        for (b1, b2) in d1.blocks.iter_mut().zip(&mut d2.blocks) {
            if splits_an_escape(b2) {
                (*b1, *b2) = (Block::ThematicBreak, Block::ThematicBreak);
            }
        }
        unlink_autolinks(&mut d2);
    }
    let d1 = &d1;
    let (e1, e2) = (extract(d1), extract(&d2));
    if e1 != e2 {
        return Some(format!(
            "text changed\n  md:   {md_text:?}\n  want: {e1:?}\n  got:  {e2:?}"
        ));
    }
    if *d1 != d2 {
        return Some(format!(
            "tree changed\n  md:   {md_text:?}\n  want: {d1:?}\n  got:  {d2:?}"
        ));
    }
    for l in md_losses {
        let allowed = *l == Loss::Whitespace && has_ws_run(input);
        if !allowed {
            return Some(format!("unexpected md loss {l:?} for md {md_text:?}"));
        }
    }
    for l in txt_losses {
        let allowed = *l == Loss::BlankLines && has_blank_run(input);
        if !allowed {
            return Some(format!("unexpected txt loss {l:?}"));
        }
    }
    // Writing the re-read tree as text gives the same text again.
    let (t1, _) = txt::write(d1);
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
    let mut d2 = md::read_with(md_text.as_bytes(), Dialect::Gfm).unwrap();
    assert!(has_escaped_autolink(&d2));
    // Read as CommonMark would, the escaped link is the text again.
    unlink_autolinks(&mut d2);
    assert_eq!(d1, d2);
    // Without an escape the URL comes back as the same text.
    let (d1, _) = txt::read(b"see http://x.y/a-b now").unwrap();
    let mut d2 = md::read_with(md::write(&d1).0.as_bytes(), Dialect::Gfm).unwrap();
    assert!(!has_escaped_autolink(&d2) && d1 != d2);
    unlink_autolinks(&mut d2);
    assert_eq!(d1, d2);
}

/// An escaped autolink is compared as its CommonMark text; the rest of the
/// document is still compared under GFM.
#[test]
fn escaped_autolink_leaves_the_rest_compared() {
    // `a_` is an escape the link splits: that paragraph is left out, the
    // next one still counts.
    for input in [
        "see http://x.y/a_b here\n\nnow",
        "see http://x.y/a_ here\n\nnow",
        "see www.x.y/a\\b&c#d_ here\n\nnow",
        "http://x.y/a_b\nnow",
    ] {
        let (d1, txt_losses) = txt::read(input.as_bytes()).unwrap();
        let (md_text, md_losses) = md::write(&d1);
        let d2 = md::read_with(md_text.as_bytes(), Dialect::Gfm).unwrap();
        assert!(has_escaped_autolink(&d2), "{md_text:?}");
        let check = |md_text: &str| {
            check_written(input, &d1, &txt_losses, md_text, &md_losses, Dialect::Gfm)
        };
        assert_eq!(check(&md_text), None, "{input:?}");
        // The same escaped link with the text around it changed is caught.
        let changed = md_text.replace("now", "later");
        assert!(check(&changed).is_some(), "{changed:?}");
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

/// The GFM 0.29 spec (cmark-gfm's unmodified `test/spec.txt`, the same
/// fixture `gfm.rs` reads; its 672 examples hold the CommonMark ones and
/// the extension ones).
const GFM_SPEC: &str = include_str!("data/gfm-spec-0.29.0.gfm.13.txt");

/// The Markdown of every example of [`GFM_SPEC`] (number from 1), tabs
/// restored from the spec's arrows.
fn spec_markdown() -> Vec<(u32, String)> {
    const FENCE: &str = "````````````````````````````````";
    let mut out = Vec::new();
    let mut lines = GFM_SPEC.lines();
    while let Some(line) = lines.next() {
        if !line.starts_with(&format!("{FENCE} example")) {
            continue;
        }
        let mut md = String::new();
        for l in lines.by_ref() {
            if l == "." {
                break;
            }
            md.push_str(l);
            md.push('\n');
        }
        for l in lines.by_ref() {
            if l == FENCE {
                break;
            }
        }
        out.push((out.len() as u32 + 1, md.replace('\u{2192}', "\t")));
    }
    assert_eq!(out.len(), 672, "the spec holds 672 examples");
    out
}

/// 64-bit FNV-1a, to pin a large output without storing it.
fn fnv1a(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(0x0100_0000_01b3);
    }
}

/// The CommonMark-mode writer (what txt -> md ships) is byte for byte what
/// it was before the GFM mode existed: its output and losses over every
/// spec example's tree, read in both dialects (so tables and strikethrough
/// take the CommonMark-mode flattening), are pinned by a hash taken from
/// the writer before the change. The txt -> md outputs of the CLI tests
/// and the writer's own unit tests pin the rest.
#[test]
fn commonmark_mode_unchanged() {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    let mut total = 0usize;
    for (_, md_in) in spec_markdown() {
        for dialect in [Dialect::CommonMark, Dialect::Gfm] {
            let d = md::read_with(md_in.as_bytes(), dialect).unwrap();
            let (out, losses) = md::write(&d);
            let limited = md::write_limited_with(&d, Limits::DEFAULT, Dialect::CommonMark);
            assert_eq!(limited.unwrap(), (out.clone(), losses.clone()));
            fnv1a(&mut h, out.as_bytes());
            fnv1a(&mut h, format!("{losses:?}").as_bytes());
            total += out.len();
        }
    }
    // Taken from the writer as it was before the GFM mode.
    assert_eq!((total, h), (31_195, 4_507_573_369_675_658_427));
}

/// Pieces a delimiter row is made of, added to [`ALPHABET`] for
/// [`txt_trees_commonmark_output_unchanged`] so its one change is hit.
const DELIMITER_ROW_PIECES: &[&str] = &[":", ":--", "--:", ":-:", "\n:-", ":\n", "-:"];

/// Whether `line` (no line ending) is what the CommonMark mode writes for
/// a txt line `:-+:?` that it escapes: `\:`, dashes, an optional `:`.
fn escaped_delimiter_row(line: &str) -> bool {
    line.strip_prefix("\\:").is_some_and(|r| {
        let r = r.strip_suffix(':').unwrap_or(r);
        !r.is_empty() && r.bytes().all(|b| b == b'-')
    })
}

/// The CommonMark mode is the shipped txt -> md writer, and every tree the
/// txt reader can build (paragraphs of text lines joined by hard breaks)
/// is written byte for byte as before the GFM mode, except one shape: a
/// paragraph line made only of `:` and dashes (`:--`, `:-:`), which a GFM
/// viewer would read as a table's delimiter row, gets its `:` escaped.
/// The property runs over 20,000 seeded txt inputs from [`ALPHABET`] and
/// [`DELIMITER_ROW_PIECES`]; the escape is taken back off those lines, and
/// the result (output and losses) is pinned by a hash taken from the
/// writer before any change.
#[test]
fn txt_trees_commonmark_output_unchanged() {
    let pieces: Vec<&str> = ALPHABET
        .iter()
        .chain(DELIMITER_ROW_PIECES)
        .copied()
        .collect();
    let mut rng = Rng(0x7E47_0C0D);
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    let (mut total, mut escaped) = (0usize, 0usize);
    for _ in 0..20_000 {
        let len = 1 + rng.below(32);
        let input: String = (0..len).map(|_| pieces[rng.below(pieces.len())]).collect();
        let (d, _) = txt::read(input.as_bytes()).unwrap();
        let (out, losses) = md::write(&d);
        let mut base = String::with_capacity(out.len());
        for line in out.split_inclusive('\n') {
            if escaped_delimiter_row(line.trim_end_matches('\n')) {
                escaped += 1;
                base.push_str(&line[1..]);
            } else {
                base.push_str(line);
            }
        }
        fnv1a(&mut h, base.as_bytes());
        fnv1a(&mut h, format!("{losses:?}").as_bytes());
        total += base.len();
    }
    assert!(
        escaped >= 100,
        "only {escaped} delimiter-row lines generated"
    );
    // Taken from the writer as it was before the GFM mode.
    assert_eq!((total, h), (780_144, 10_513_879_770_608_044_970));
}

/// Spec examples whose tree the GFM writer does not give back unchanged,
/// each a flattening of the code it shares with the CommonMark mode. No
/// table, strikethrough or task list example is among them.
const GFM_ROUNDTRIP_EXCEPTIONS: &[(u32, &str)] = &[
    (
        51,
        "a line break in a heading is a space (an ATX heading is one line)",
    ),
    (
        52,
        "a line break in a heading is a space (an ATX heading is one line)",
    ),
    (
        65,
        "a line break in a heading is a space (an ATX heading is one line)",
    ),
    (187, "an empty block quote writes nothing"),
    (217, "an empty block quote writes nothing"),
    (218, "an empty block quote writes nothing"),
    (
        301,
        "a block after a block quote in a tight item needs a blank line, so the list reads loose",
    ),
    (
        335,
        "a line feed inside text (`&#10;`) is written as a line break, so the paragraph splits",
    ),
    (
        626,
        "the backslash of an escaped `&` right after an extended autolink becomes part of the link",
    ),
    (
        627,
        "the backslash of an escaped `<` right after an extended autolink becomes part of the link",
    ),
];

/// Every spec example's GFM tree, written in GFM mode and read back by the
/// GFM reader, is the same tree, except the listed flattenings (each of
/// which must still differ, so the list cannot go stale).
#[test]
fn gfm_roundtrip_is_stable() {
    for (n, why) in GFM_ROUNDTRIP_EXCEPTIONS {
        assert!(!why.is_empty(), "exception {n} needs a reason");
    }
    let mut failed = Vec::new();
    let mut tables = 0;
    for (n, md_in) in spec_markdown() {
        let d1 = md::read_with(md_in.as_bytes(), Dialect::Gfm).unwrap();
        // Seven of the eight table examples make a table (one shows a
        // header and delimiter row of different widths), at any depth.
        tables += usize::from(format!("{d1:?}").contains("Table {"));
        let (w1, _) = md::write_limited_with(&d1, Limits::UNLIMITED, Dialect::Gfm).unwrap();
        let d2 = md::read_with(w1.as_bytes(), Dialect::Gfm).unwrap();
        let known = GFM_ROUNDTRIP_EXCEPTIONS.iter().any(|&(e, _)| e == n);
        if known {
            assert_ne!(d1, d2, "example {n} round-trips now: drop its exception");
        } else if d1 != d2 {
            failed.push(format!("{n}: {md_in:?} -> {w1:?}\n  {d1:?}\n  {d2:?}"));
        }
    }
    assert_eq!(tables, 7, "the spec's table examples were read as tables");
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}
