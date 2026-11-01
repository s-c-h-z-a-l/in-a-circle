//! Book structure on the document tree: the title and the chapter split.
//! Pure functions; only top-level headings count, so a heading inside a
//! list or a quote never starts a chapter.

use std::ops::Range;

use crate::xhtml::plain;
use crate::{Block, Document};

/// Most chapters one book may have. 1024 is the input-count limit of the
/// engine: a book built from that many inputs is the largest one accepted,
/// and the split counts toward it. Past it the caller fails with
/// `book would have more than 1024 chapters`.
pub const MAX_CHAPTERS: usize = 1024;

fn heading_text(content: &[crate::Inline]) -> String {
    let mut s = String::new();
    plain(content, &mut s);
    s.trim().to_string()
}

/// The plain text of the first top-level H1, if it has any.
pub fn title(doc: &Document) -> Option<String> {
    doc.blocks.iter().find_map(|b| match b {
        Block::Heading { level: 1, content } => {
            Some(heading_text(content)).filter(|t| !t.is_empty())
        }
        _ => None,
    })
}

/// Block ranges of the chapters. Splits at the top-level H1s when there
/// are at least two, else at the H2s when there are at least two, else the
/// whole document is one chapter. Content before the first split heading is
/// its own chapter. An empty document is one empty chapter.
pub fn split(doc: &Document) -> Vec<Range<usize>> {
    let count = |lv: u8| {
        doc.blocks
            .iter()
            .filter(|b| matches!(b, Block::Heading { level, .. } if *level == lv))
            .count()
    };
    let level = if count(1) >= 2 {
        1
    } else if count(2) >= 2 {
        2
    } else {
        // One chapter: a one-element Vec of ranges is meant, not a Vec of indices.
        #[allow(clippy::single_range_in_vec_init)]
        return vec![0..doc.blocks.len()];
    };
    let starts: Vec<usize> = doc
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| matches!(b, Block::Heading { level: l, .. } if *l == level))
        .map(|(i, _)| i)
        .collect();
    let mut out = Vec::with_capacity(starts.len() + 1);
    if starts[0] > 0 {
        out.push(0..starts[0]);
    }
    for (k, &s) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(doc.blocks.len());
        out.push(s..end);
    }
    out
}

/// The text of the first top-level heading of the chapter `blocks`, else
/// `Chapter <n>` with `n = index + 1`.
pub fn chapter_title(blocks: &[Block], index: usize) -> String {
    blocks
        .iter()
        .find_map(|b| match b {
            Block::Heading { content, .. } => Some(heading_text(content)).filter(|t| !t.is_empty()),
            _ => None,
        })
        .unwrap_or_else(|| format!("Chapter {}", index + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Inline, ListItem};

    fn h(level: u8, t: &str) -> Block {
        Block::Heading {
            level,
            content: vec![Inline::Text(t.into())],
        }
    }
    fn p(t: &str) -> Block {
        Block::Paragraph(vec![Inline::Text(t.into())])
    }
    fn doc(blocks: Vec<Block>) -> Document {
        Document { blocks }
    }

    #[test]
    fn title_from_first_h1() {
        let d = doc(vec![
            h(2, "no"),
            Block::Heading {
                level: 1,
                content: vec![
                    Inline::Text("A ".into()),
                    Inline::Emph(vec![Inline::Text("b".into())]),
                ],
            },
            h(1, "second"),
        ]);
        assert_eq!(title(&d).as_deref(), Some("A b"));
    }

    #[test]
    fn title_none_without_h1() {
        assert_eq!(title(&doc(vec![h(2, "x"), p("y")])), None);
        assert_eq!(title(&doc(vec![])), None);
    }

    #[test]
    fn split_on_h1_when_two_or_more() {
        let d = doc(vec![h(1, "a"), p("x"), h(2, "s"), h(1, "b"), p("y")]);
        assert_eq!(split(&d), vec![0..3, 3..5]);
    }

    #[test]
    fn split_on_h2_when_one_h1() {
        let d = doc(vec![h(1, "t"), h(2, "a"), p("x"), h(2, "b")]);
        assert_eq!(split(&d), vec![0..1, 1..3, 3..4]);
    }

    #[test]
    fn no_split_without_repeats() {
        let d = doc(vec![h(1, "t"), h(2, "a"), h(3, "c"), h(3, "d"), p("x")]);
        assert_eq!(split(&d), vec![0..5]);
    }

    #[test]
    fn preamble_is_its_own_chapter() {
        let d = doc(vec![p("intro"), h(1, "a"), h(1, "b")]);
        assert_eq!(split(&d), vec![0..1, 1..2, 2..3]);
        assert_eq!(chapter_title(&d.blocks[0..1], 0), "Chapter 1");
        assert_eq!(chapter_title(&d.blocks[1..2], 1), "a");
    }

    #[test]
    fn empty_doc_one_chapter() {
        assert_eq!(split(&doc(vec![])), vec![0..0]);
        assert_eq!(chapter_title(&[], 0), "Chapter 1");
    }

    #[test]
    fn nested_headings_do_not_split() {
        let nested = |lv| {
            vec![
                Block::BlockQuote(vec![h(lv, "q")]),
                Block::List {
                    start: None,
                    tight: true,
                    items: vec![ListItem {
                        checked: None,
                        blocks: vec![h(lv, "l")],
                    }],
                },
            ]
        };
        let mut b = vec![h(1, "top")];
        b.extend(nested(1));
        assert_eq!(split(&doc(b)), vec![0..3]);
        let mut b = vec![h(2, "top")];
        b.extend(nested(2));
        assert_eq!(split(&doc(b)), vec![0..3]);
    }

    #[test]
    fn chapter_title_first_heading_any_level() {
        let b = [p("x"), h(2, "Sub"), h(1, "Later")];
        assert_eq!(chapter_title(&b, 4), "Sub");
        assert_eq!(chapter_title(&[p("x")], 4), "Chapter 5");
    }
}
