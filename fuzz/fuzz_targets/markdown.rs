#![no_main]
//! The document readers on arbitrary bytes, then every writer on the tree
//! they built: the two real conversions (`inacircle-text`: txt->md and
//! md->txt) plus `html::write` and the strict `xhtml::write_chapter` (both
//! modes) and `xhtml::write_book_chapter` (every image packaged, links to
//! another document resolved), so the tree walks run too.

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

fuzz_target!(|data: &[u8]| {
    for dialect in [doc::Dialect::CommonMark, doc::Dialect::Gfm] {
        if let Ok(d) = doc::md::read_with(data, dialect) {
            let _ = doc::html::write(&d);
            xhtml(&d, doc::Limits::UNLIMITED);
            let _ = doc::html::write_with(&d, doc::html::Options { tagfilter: true });
            let _ = doc::txt::write(&d);
            let _ = doc::md::write(&d);
        }
    }
    if let Ok((d, _losses)) = doc::txt::read(data) {
        let _ = doc::html::write(&d);
        xhtml(&d, doc::Limits::UNLIMITED);
        let _ = doc::md::write(&d);
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
        }
    }
    if let Ok((d, _)) = doc::txt::read_limited(data, official) {
        let _ = doc::md::write_limited(&d, official);
    }
});
