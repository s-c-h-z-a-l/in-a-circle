#![no_main]
//! The document readers on arbitrary bytes, then every writer on the tree
//! they built: the two real conversions (`inacircle-text`: txt->md and
//! md->txt) plus `html::write` and the strict `xhtml::write_chapter` (both
//! modes) and `xhtml::write_book_chapter` (every image packaged, links to
//! another document resolved), so the tree walks run too. The GFM
//! Markdown writer must also reach a fixed point ([`gfm_stable`]), give
//! a txt tree back as it was ([`txt_round_trip`]), and write the marks of
//! an XHTML-shaped tree so that a reader shows its text and nothing more
//! ([`marks_read_back`]).

use libfuzzer_sys::fuzz_target;

/// Packages every image under its own destination, so the `<img>` path and
/// its URL escaping see every destination the reader builds.
struct Every;

impl doc::xhtml::Images for Every {
    fn src(&self, dest: &str) -> Option<String> {
        Some(dest.to_string())
    }
}

/// The strict XHTML writer on `d` as one chapter, in both modes and as a
/// book chapter, under `limits`.
fn xhtml(d: &doc::Document, limits: doc::Limits) {
    let ch = doc::xhtml::Chapter {
        blocks: &d.blocks,
        file: "ch0001.xhtml".to_string(),
    };
    let anchors = doc::xhtml::Anchors::build(std::slice::from_ref(&ch));
    let page = doc::xhtml::Mode::Standalone {
        lang: "und".to_string(),
        title: "t".to_string(),
    };
    for mode in [doc::xhtml::Mode::Fragment, page] {
        let _ = doc::xhtml::write_chapter(&ch, &anchors, &mode, limits);
    }
    let mut n = 0_usize;
    doc::xhtml::image_dests(&d.blocks, &mut |_| n += 1);
    // The document twice, as two parts, so a link may name the other.
    let keys = ["a.md".to_string(), "b/c.md".to_string()];
    let other = doc::xhtml::Chapter {
        blocks: &d.blocks,
        file: "ch0002.xhtml".to_string(),
    };
    let anchors = [
        anchors,
        doc::xhtml::Anchors::build(std::slice::from_ref(&other)),
    ];
    for (part, ch) in [&ch, &other].into_iter().enumerate() {
        let refs = doc::xhtml::Refs {
            anchors: &anchors,
            keys: &keys,
            part,
            images: &Every,
        };
        let _ = doc::xhtml::write_book_chapter(ch, &refs, limits);
    }
}

/// `blocks` with what the shared (CommonMark and GFM) writer is known not
/// to keep stable taken out, so the fixed point below checks the rest:
/// - an extended autolink becomes an ordinary link: text right after it
///   that the writer escapes lengthens the link when read again (it is
///   written as its bare text), by one backslash per round;
/// - emphasis and strong emphasis become their content: some nestings of
///   `*` runs (`****o*****o******b`) re-read differently each round, in
///   the CommonMark mode as well.
fn plain(blocks: &mut [doc::Block]) {
    fn inlines(c: &mut Vec<doc::Inline>) {
        let mut out = Vec::with_capacity(c.len());
        for mut i in std::mem::take(c) {
            match &mut i {
                doc::Inline::Emph(inner) | doc::Inline::Strong(inner) => {
                    inlines(inner);
                    out.append(inner);
                    continue;
                }
                doc::Inline::Link { auto, content, .. } => {
                    *auto = false;
                    inlines(content);
                }
                doc::Inline::Strikethrough(inner) => inlines(inner),
                doc::Inline::Image { alt, .. } => inlines(alt),
                _ => {}
            }
            out.push(i);
        }
        *c = out;
    }
    for b in blocks {
        match b {
            doc::Block::Paragraph(c) | doc::Block::Heading { content: c, .. } => inlines(c),
            doc::Block::BlockQuote(bs) => plain(bs),
            doc::Block::List { items, .. } => {
                for i in items {
                    plain(&mut i.blocks);
                }
            }
            doc::Block::Table { head, rows, .. } => {
                for c in head.iter_mut().chain(rows.iter_mut().flatten()) {
                    inlines(c);
                }
            }
            _ => {}
        }
    }
}

/// The GFM writer's output read back by the GFM reader and written again
/// is the same after one round, for the tree made [`plain`]:
/// `write(read(w2)) == w2` with `w2 = write(read(write(d)))`. The first
/// round may flatten what the writer cannot keep (a line break in a
/// heading or a cell, an empty block quote, a tight list that must be
/// written loose); after it nothing may change, so no escape or delimiter
/// is lost or added per round. Checked only where the CommonMark mode
/// (same tree, CommonMark reader) reaches its own fixed point: what is
/// unstable there is the code both modes share, not the GFM mode (the
/// fuzzer found such shapes, e.g. a backslash in a code block's info
/// string, which the writer copies as it is).
fn gfm_stable(d: &doc::Document) {
    let rounds = |dialect: doc::Dialect| {
        let write = |d: &doc::Document| {
            doc::md::write_limited_with(d, doc::Limits::UNLIMITED, dialect)
                .unwrap()
                .0
        };
        let read = |s: &str| doc::md::read_with(s.as_bytes(), dialect).unwrap();
        let mut d = d.clone();
        plain(&mut d.blocks);
        let w1 = write(&d);
        let w2 = write(&read(&w1));
        let w3 = write(&read(&w2));
        (w1, w2, w3)
    };
    let (_, c2, c3) = rounds(doc::Dialect::CommonMark);
    if c2 != c3 {
        return;
    }
    let (w1, w2, w3) = rounds(doc::Dialect::Gfm);
    assert_eq!(w2, w3, "GFM output not stable; first round {w1:?}");
}

/// Adjacent texts as one, as every reader builds them.
fn merge_texts(c: &mut Vec<doc::Inline>) {
    let mut out: Vec<doc::Inline> = Vec::with_capacity(c.len());
    for i in std::mem::take(c) {
        if let (Some(doc::Inline::Text(a)), doc::Inline::Text(b)) = (out.last_mut(), &i) {
            a.push_str(b);
        } else {
            out.push(i);
        }
    }
    *c = out;
}

/// A tree of the txt reader (paragraphs of text lines and hard breaks)
/// written in the GFM mode and read back by the GFM reader is the same
/// tree. Stated difference: text that a GFM reader links (`www.x.y`, a
/// URL, an address) comes back as an extended autolink, which is turned
/// back into its text here; the writer leaves such text as it is, so
/// that it links, and guards only the links that would hold an escape.
fn txt_round_trip(d: &doc::Document) {
    let (w, _) = doc::md::write_limited_with(d, doc::Limits::UNLIMITED, doc::Dialect::Gfm).unwrap();
    let mut back = doc::md::read_with(w.as_bytes(), doc::Dialect::Gfm).unwrap();
    for b in &mut back.blocks {
        if let doc::Block::Paragraph(c) = b {
            let mut out = Vec::with_capacity(c.len());
            for i in std::mem::take(c) {
                match i {
                    doc::Inline::Link {
                        auto: true,
                        content,
                        ..
                    } => out.extend(content),
                    i => out.push(i),
                }
            }
            merge_texts(&mut out);
            *c = out;
        }
    }
    assert_eq!(&back, d, "txt tree not read back from {w:?}");
}

/// The text a reader shows for `blocks`, line breaks left out (a break
/// at an edge of a paragraph is not written).
fn shown(blocks: &[doc::Block], out: &mut String) {
    fn inlines(c: &[doc::Inline], out: &mut String) {
        for i in c {
            match i {
                doc::Inline::Text(t) | doc::Inline::Code(t) | doc::Inline::Html(t) => {
                    out.push_str(t)
                }
                doc::Inline::Emph(c) | doc::Inline::Strong(c) | doc::Inline::Strikethrough(c) => {
                    inlines(c, out)
                }
                doc::Inline::Link { content: c, .. } | doc::Inline::Image { alt: c, .. } => {
                    inlines(c, out)
                }
                doc::Inline::SoftBreak | doc::Inline::HardBreak => {}
            }
        }
    }
    for b in blocks {
        match b {
            doc::Block::Paragraph(c) | doc::Block::Heading { content: c, .. } => inlines(c, out),
            doc::Block::BlockQuote(bs) => shown(bs, out),
            doc::Block::List { items, .. } => {
                for i in items {
                    shown(&i.blocks, out);
                }
            }
            doc::Block::Table { head, rows, .. } => {
                for c in head.iter().chain(rows.iter().flatten()) {
                    inlines(c, out);
                }
            }
            doc::Block::CodeBlock { text, .. } => out.push_str(text),
            doc::Block::HtmlBlock(h) => out.push_str(h),
            doc::Block::ThematicBreak => out.push_str("<hr>"),
        }
    }
}

/// An inline tree shaped like the XHTML reader's, from `data`: text with
/// white space and punctuation (the characters next to which a delimiter
/// cannot open or close) at the edges of emphasis, strong emphasis,
/// strikethrough and links, nested, and hard line breaks. As the reader
/// builds them: no mark or link is empty, and no link holds a link (HTML
/// forbids it). Code spans are left out: two written side by side join
/// (a limit of the shared writer the XHTML reader does not hit, as it
/// never puts them there).
fn xhtml_shaped(data: &[u8]) -> Vec<doc::Inline> {
    const PIECES: &[&str] = &[
        "a", "b", "7", " ", "  ", "\u{a0}", ".", ",", "\"", "(", ")", "'", "-", ":", "!", "?", "~",
        "*", "_", "\\", "1.", "www.x.y", "#", "\u{e9}", "\u{2014}", "\u{3000}", "+",
    ];
    // Deeper than any real book's inline nesting needs, short of the
    // readers' depth bound (256), so the walk stays cheap.
    const MAX_NESTING: usize = 16;
    let mut stack: Vec<(u8, Vec<doc::Inline>)> = vec![(0, Vec::new())];
    let close = |stack: &mut Vec<(u8, Vec<doc::Inline>)>| {
        let (kind, mut c) = stack.pop().expect("an open frame");
        merge_texts(&mut c);
        if c.is_empty() {
            return;
        }
        let node = match kind {
            0 => doc::Inline::Emph(c),
            1 => doc::Inline::Strong(c),
            2 => doc::Inline::Strikethrough(c),
            _ => doc::Inline::Link {
                dest: "u".to_string(),
                title: String::new(),
                content: c,
                auto: false,
            },
        };
        stack.last_mut().expect("the paragraph").1.push(node);
    };
    for &b in data {
        let depth = stack.len();
        let top = &mut stack.last_mut().expect("the paragraph").1;
        match b % 8 {
            0..=3 => top.push(doc::Inline::Text(
                PIECES[usize::from(b / 8) % PIECES.len()].to_string(),
            )),
            7 => top.push(doc::Inline::HardBreak),
            4 | 5 if depth <= MAX_NESTING => {
                let kind = b / 8 % 4;
                if kind < 3 || stack.iter().all(|&(k, _)| k < 3) {
                    stack.push((kind, Vec::new()));
                }
            }
            6 if depth > 1 => close(&mut stack),
            _ => {}
        }
    }
    while stack.len() > 1 {
        close(&mut stack);
    }
    let mut c = stack.pop().expect("the paragraph").1;
    merge_texts(&mut c);
    c
}

/// What applies to a character a reader shows (bits): emphasis, strong
/// emphasis and strikethrough may be lost (with their loss), code and a
/// link or image must stay. An extended autolink counts as no link: a GFM
/// reader links `www.` text as it reads it.
const EMPH: u8 = 1;
const STRONG: u8 = 2;
const STRIKE: u8 = 4;
const CODE: u8 = 8;
const LINK: u8 = 16;
const IMAGE: u8 = 32;

/// The characters [`shown`] gives, each with what applies to it.
fn styled(blocks: &[doc::Block], out: &mut Vec<(char, u8)>) {
    fn inlines(c: &[doc::Inline], on: u8, out: &mut Vec<(char, u8)>) {
        let text = |t: &str, on: u8, out: &mut Vec<(char, u8)>| {
            out.extend(t.chars().map(|ch| (ch, on)));
        };
        for i in c {
            match i {
                doc::Inline::Text(t) | doc::Inline::Html(t) => text(t, on, out),
                doc::Inline::Code(t) => text(t, on | CODE, out),
                doc::Inline::Emph(c) => inlines(c, on | EMPH, out),
                doc::Inline::Strong(c) => inlines(c, on | STRONG, out),
                doc::Inline::Strikethrough(c) => inlines(c, on | STRIKE, out),
                doc::Inline::Link {
                    content: c, auto, ..
                } => inlines(c, if *auto { on } else { on | LINK }, out),
                doc::Inline::Image { alt: c, .. } => inlines(c, on | IMAGE, out),
                doc::Inline::SoftBreak | doc::Inline::HardBreak => {}
            }
        }
    }
    for b in blocks {
        match b {
            doc::Block::Paragraph(c) | doc::Block::Heading { content: c, .. } => inlines(c, 0, out),
            doc::Block::BlockQuote(bs) => styled(bs, out),
            doc::Block::List { items, .. } => {
                for i in items {
                    styled(&i.blocks, out);
                }
            }
            doc::Block::Table { head, rows, .. } => {
                for c in head.iter().chain(rows.iter().flatten()) {
                    inlines(c, 0, out);
                }
            }
            other => {
                let mut s = String::new();
                shown(std::slice::from_ref(other), &mut s);
                out.extend(s.chars().map(|ch| (ch, 0)));
            }
        }
    }
}

/// The GFM writer on an XHTML-shaped paragraph, read back by a GFM
/// reader, character by character: the same text (no delimiter left as
/// text, no text lost), the same code and links, no formatting the tree
/// did not have (`Emph` in `Emph` written as `Strong`), and a mark lost
/// from a character that is not white space (white space at a mark's
/// edges moves out) only with its loss (a mark that cannot open or close
/// is left out, and says so).
fn marks_read_back(data: &[u8]) {
    let d = doc::Document {
        blocks: vec![doc::Block::Paragraph(xhtml_shaped(data))],
    };
    let (w, losses) =
        doc::md::write_limited_with(&d, doc::Limits::UNLIMITED, doc::Dialect::Gfm).unwrap();
    let back = doc::md::read_with(w.as_bytes(), doc::Dialect::Gfm).unwrap();
    let (mut want, mut got) = (Vec::new(), Vec::new());
    styled(&d.blocks, &mut want);
    styled(&back.blocks, &mut got);
    let text = |v: &[(char, u8)]| v.iter().map(|&(c, _)| c).collect::<String>();
    assert_eq!(text(&got), text(&want), "{w:?} from {:?}", d.blocks);
    for (k, (&(ch, was), &(_, is))) in want.iter().zip(&got).enumerate() {
        let at = || format!("char {k} {ch:?} in {w:?} from {:?}", d.blocks);
        assert_eq!(
            is & (CODE | LINK | IMAGE),
            was & (CODE | LINK | IMAGE),
            "{}",
            at()
        );
        assert_eq!(is & !was, 0, "new formatting: {}", at());
        let lost = was & !is;
        if !ch.is_whitespace() {
            if lost & (EMPH | STRONG) != 0 {
                assert!(losses.contains(&doc::Loss::Emphasis), "silent: {}", at());
            }
            if lost & STRIKE != 0 {
                assert!(
                    losses.contains(&doc::Loss::Strikethrough),
                    "silent: {}",
                    at()
                );
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    marks_read_back(data);
    for dialect in [doc::Dialect::CommonMark, doc::Dialect::Gfm] {
        if let Ok(d) = doc::md::read_with(data, dialect) {
            let _ = doc::html::write(&d);
            xhtml(&d, doc::Limits::UNLIMITED);
            let _ = doc::html::write_with(&d, doc::html::Options { tagfilter: true });
            let _ = doc::txt::write(&d);
            let _ = doc::md::write(&d);
            gfm_stable(&d);
        }
    }
    if let Ok((d, _losses)) = doc::txt::read(data) {
        let _ = doc::html::write(&d);
        xhtml(&d, doc::Limits::UNLIMITED);
        let _ = doc::md::write(&d);
        gfm_stable(&d);
        txt_round_trip(&d);
    }
    // The shipped, budgeted path under budgets small enough to be hit by
    // short inputs: it must stop with an error, never panic or hang. The
    // budgets are data-derived so every budget gets its turn.
    let n = data.len() % 64;
    let tight = doc::Limits {
        input_bytes: usize::MAX,
        nodes: n,
        text_bytes: n * 4,
        output_bytes: n * 8,
    };
    for dialect in [doc::Dialect::CommonMark, doc::Dialect::Gfm] {
        if let Ok(d) = doc::md::read_limited_with(data, tight, dialect) {
            let _ = doc::txt::write_limited(&d, tight);
            xhtml(&d, tight);
            let _ = doc::md::write_limited(&d, tight);
            let _ = doc::md::write_limited_with(&d, tight, doc::Dialect::Gfm);
        }
    }
    if let Ok((d, _)) = doc::txt::read_limited(data, tight) {
        let _ = doc::md::write_limited(&d, tight);
    }
    // The official budgets (what the shipped conversion uses): a short
    // input stays far below them, so this is the accept path.
    let official = doc::Limits::DEFAULT;
    for dialect in [doc::Dialect::CommonMark, doc::Dialect::Gfm] {
        if let Ok(d) = doc::md::read_limited_with(data, official, dialect) {
            let _ = doc::txt::write_limited(&d, official);
            xhtml(&d, official);
            let _ = doc::md::write_limited(&d, official);
            let _ = doc::md::write_limited_with(&d, official, doc::Dialect::Gfm);
        }
    }
    if let Ok((d, _)) = doc::txt::read_limited(data, official) {
        let _ = doc::md::write_limited(&d, official);
    }
});
