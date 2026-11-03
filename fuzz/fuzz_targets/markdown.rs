#![no_main]
//! The document readers on arbitrary bytes, then every writer on the tree
//! they built: the two real conversions (`inacircle-text`: txt->md and
//! md->txt) plus `html::write` and the strict `xhtml::write_chapter` (both
//! modes) and `xhtml::write_book_chapter` (every image packaged, links to
//! another document resolved), so the tree walks run too. The GFM
//! Markdown writer must also reach a fixed point: see [`gfm_stable`].

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

fuzz_target!(|data: &[u8]| {
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
