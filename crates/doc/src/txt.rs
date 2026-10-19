//! Plain-text reader and writer.

use crate::{Block, Document, Inline, Loss, ReadError};
use std::collections::BTreeSet;

/// Reads plain text. Paragraphs are separated by blank (whitespace-only)
/// lines; lines inside a paragraph are joined by `HardBreak`. Runs of more
/// than one blank line between paragraphs are merged and reported.
/// Linear in the input; no recursion.
pub fn read(bytes: &[u8]) -> Result<(Document, BTreeSet<Loss>), ReadError> {
    let s = std::str::from_utf8(bytes).map_err(|_| ReadError::NotUtf8)?;
    let s = s.strip_prefix('\u{feff}').unwrap_or(s);
    let mut losses = BTreeSet::new();
    let mut blocks = Vec::new();
    let mut inlines: Vec<Inline> = Vec::new();
    let mut blanks = 0usize;

    // Split on LF, CRLF and lone CR without allocating a normalized copy.
    let mut rest = s;
    while !rest.is_empty() {
        let end = rest.find(['\n', '\r']).unwrap_or(rest.len());
        let line = &rest[..end];
        let mut next = end;
        if rest[end..].starts_with("\r\n") {
            next += 2;
        } else if end < rest.len() {
            next += 1;
        }
        rest = &rest[next..];

        if line.trim().is_empty() {
            blanks += 1;
            continue;
        }
        if !inlines.is_empty() {
            if blanks > 0 {
                if blanks > 1 {
                    losses.insert(Loss::BlankLines);
                }
                blocks.push(Block::Paragraph(std::mem::take(&mut inlines)));
            } else {
                inlines.push(Inline::HardBreak);
            }
        }
        blanks = 0;
        inlines.push(Inline::Text(line.to_string()));
    }
    if !inlines.is_empty() {
        blocks.push(Block::Paragraph(inlines));
    }
    Ok((Document { blocks }, losses))
}

/// Writes plain text, reporting every piece of structure that was flattened.
/// Blocks are separated by one blank line (blocks that render to nothing are
/// skipped); non-empty output ends with exactly one `\n`, empty output is `""`.
/// List nesting: content under a bullet item is indented 2 spaces, under an
/// ordered item by the marker's width (3 spaces under `1. `).
pub fn write(doc: &Document) -> (String, BTreeSet<Loss>) {
    let mut losses = BTreeSet::new();
    let mut out = blocks_to_string(&doc.blocks, true, &mut losses);
    if !out.is_empty() {
        out.push('\n');
    }
    (out, losses)
}

fn blocks_to_string(blocks: &[Block], loose: bool, losses: &mut BTreeSet<Loss>) -> String {
    let sep = if loose { "\n\n" } else { "\n" };
    let mut parts: Vec<String> = Vec::new();
    for b in blocks {
        if let Some(s) = block_to_string(b, losses)
            && !s.is_empty()
        {
            parts.push(s);
        }
    }
    parts.join(sep)
}

fn block_to_string(b: &Block, losses: &mut BTreeSet<Loss>) -> Option<String> {
    Some(match b {
        Block::Paragraph(c) => inlines_to_string(c, losses),
        Block::Heading { level, content } => {
            if *level != 1 {
                losses.insert(Loss::HeadingLevels);
            }
            inlines_to_string(content, losses)
        }
        Block::CodeBlock { text, .. } => {
            let text = text.strip_suffix('\n').unwrap_or(text);
            prefix_lines(text, "    ")
        }
        Block::BlockQuote(inner) => {
            let body = blocks_to_string(inner, true, losses);
            if body.is_empty() {
                return None;
            }
            let mut out = String::new();
            for (i, line) in body.split('\n').enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                out.push_str(if line.is_empty() { ">" } else { "> " });
                out.push_str(line);
            }
            out
        }
        Block::List {
            start,
            tight,
            items,
        } => {
            let mut out = String::new();
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(if *tight { "\n" } else { "\n\n" });
                }
                let marker = match start {
                    None => "- ".to_string(),
                    Some(n) => format!("{}. ", n.saturating_add(i as u64)),
                };
                let body = blocks_to_string(item, !*tight, losses);
                let pad = " ".repeat(if start.is_none() { 2 } else { marker.len() });
                for (j, line) in body.split('\n').enumerate() {
                    if j == 0 {
                        out.push_str(&marker);
                    } else {
                        out.push('\n');
                        if !line.is_empty() {
                            out.push_str(&pad);
                        }
                    }
                    out.push_str(line);
                }
                if body.is_empty() {
                    out.truncate(out.trim_end().len());
                }
            }
            out
        }
        Block::ThematicBreak => "----".to_string(),
        Block::HtmlBlock(_) => {
            losses.insert(Loss::Html);
            return None;
        }
    })
}

fn prefix_lines(text: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(text.len() + prefix.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !line.is_empty() {
            out.push_str(prefix);
        }
        out.push_str(line);
    }
    out
}

fn inlines_to_string(inlines: &[Inline], losses: &mut BTreeSet<Loss>) -> String {
    let mut out = String::new();
    push_inlines(inlines, &mut out, losses);
    out
}

fn push_inlines(inlines: &[Inline], out: &mut String, losses: &mut BTreeSet<Loss>) {
    for i in inlines {
        match i {
            Inline::Text(t) => out.push_str(t),
            Inline::Emph(c) | Inline::Strong(c) => {
                losses.insert(Loss::Emphasis);
                push_inlines(c, out, losses);
            }
            Inline::Code(t) => {
                losses.insert(Loss::InlineCode);
                out.push_str(t);
            }
            Inline::Link { dest, content, .. } => {
                losses.insert(Loss::Links);
                let text = inlines_to_string(content, losses);
                if dest.is_empty() || text == *dest {
                    out.push_str(if dest.is_empty() { &text } else { dest });
                } else if text.is_empty() {
                    out.push_str(dest);
                } else {
                    out.push_str(&text);
                    out.push_str(" (");
                    out.push_str(dest);
                    out.push(')');
                }
            }
            Inline::Image { alt, .. } => {
                losses.insert(Loss::Images);
                push_inlines(alt, out, losses);
            }
            Inline::SoftBreak => out.push(' '),
            Inline::HardBreak => out.push('\n'),
            Inline::Html(_) => {
                losses.insert(Loss::Html);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Inline {
        Inline::Text(s.to_string())
    }
    fn para(s: &str) -> Block {
        Block::Paragraph(vec![t(s)])
    }

    #[test]
    fn crlf_equals_lf() {
        assert_eq!(
            read(b"a\r\nb\r\n\r\nc").unwrap(),
            read(b"a\nb\n\nc").unwrap()
        );
        assert_eq!(read(b"a\rb").unwrap(), read(b"a\nb").unwrap());
    }

    #[test]
    fn lines_in_paragraph_are_hard_breaks() {
        let (d, _) = read(b"a\nb\n\nc").unwrap();
        assert_eq!(
            d.blocks,
            vec![
                Block::Paragraph(vec![t("a"), Inline::HardBreak, t("b")]),
                para("c")
            ]
        );
    }

    #[test]
    fn bom_stripped() {
        assert_eq!(read(b"\xef\xbb\xbfa").unwrap(), read(b"a").unwrap());
    }

    #[test]
    fn invalid_utf8_is_error() {
        assert_eq!(ReadError::NotUtf8.to_string(), "input is not valid UTF-8");
        assert_eq!(ReadError::NotUtf8.to_string(), "input is not valid UTF-8");
        assert_eq!(read(b"\xff").unwrap_err(), ReadError::NotUtf8);
    }

    #[test]
    fn empty_and_blank_input() {
        for input in [&b""[..], b"  \n\n"] {
            let (d, l) = read(input).unwrap();
            assert!(d.blocks.is_empty());
            assert!(l.is_empty());
        }
    }

    #[test]
    fn blank_lines_merged_reported() {
        let (d, l) = read(b"a\n\n\n\nb").unwrap();
        assert_eq!(d.blocks, vec![para("a"), para("b")]);
        assert!(l.contains(&Loss::BlankLines));
        let (_, l) = read(b"a\n\nb").unwrap();
        assert!(l.is_empty());
    }

    #[test]
    fn leading_spaces_kept() {
        let (d, _) = read(b"  x").unwrap();
        assert_eq!(d.blocks, vec![para("  x")]);
    }

    #[test]
    fn many_lines_linear_and_flat() {
        let input = "x\n".repeat(200_000);
        let (d, _) = read(input.as_bytes()).unwrap();
        assert_eq!(d.blocks.len(), 1);
    }

    #[test]
    fn writer_drops_emphasis_reports_loss() {
        let d = Document {
            blocks: vec![Block::Paragraph(vec![Inline::Strong(vec![t("a")])])],
        };
        let (s, l) = write(&d);
        assert_eq!(s, "a\n");
        assert_eq!(l, BTreeSet::from([Loss::Emphasis]));
    }

    #[test]
    fn writer_link_text_and_url() {
        let link = |text: &str| Inline::Link {
            dest: "https://x".into(),
            title: String::new(),
            content: vec![t(text)],
        };
        let d = Document {
            blocks: vec![Block::Paragraph(vec![link("x")])],
        };
        let (s, l) = write(&d);
        assert_eq!(s, "x (https://x)\n");
        assert_eq!(l, BTreeSet::from([Loss::Links]));
        let d = Document {
            blocks: vec![Block::Paragraph(vec![link("https://x")])],
        };
        assert_eq!(write(&d).0, "https://x\n");
    }

    #[test]
    fn writer_image_code_html_breaks() {
        let d = Document {
            blocks: vec![Block::Paragraph(vec![
                Inline::Image {
                    dest: "u".into(),
                    title: String::new(),
                    alt: vec![t("alt")],
                },
                Inline::SoftBreak,
                Inline::Code("c".into()),
                Inline::HardBreak,
                Inline::Html("<b>".into()),
                t("z"),
            ])],
        };
        let (s, l) = write(&d);
        assert_eq!(s, "alt c\nz\n");
        assert_eq!(
            l,
            BTreeSet::from([Loss::Images, Loss::InlineCode, Loss::Html])
        );
    }

    #[test]
    fn writer_heading_levels() {
        let h = |level| Block::Heading {
            level,
            content: vec![t("H")],
        };
        let (s, l) = write(&Document {
            blocks: vec![h(1), para("p")],
        });
        assert_eq!(s, "H\n\np\n");
        assert!(l.is_empty());
        let (_, l) = write(&Document { blocks: vec![h(2)] });
        assert_eq!(l, BTreeSet::from([Loss::HeadingLevels]));
    }

    #[test]
    fn writer_nested_list_indent() {
        let inner = Block::List {
            start: None,
            tight: true,
            items: vec![vec![para("b")]],
        };
        let outer = Block::List {
            start: None,
            tight: true,
            items: vec![vec![para("a"), inner], vec![para("c")]],
        };
        assert_eq!(
            write(&Document {
                blocks: vec![outer]
            })
            .0,
            "- a\n  - b\n- c\n"
        );
    }

    #[test]
    fn writer_ordered_list_numbers() {
        let l = Block::List {
            start: Some(3),
            tight: true,
            items: vec![vec![para("a")], vec![para("b")]],
        };
        assert_eq!(write(&Document { blocks: vec![l] }).0, "3. a\n4. b\n");
    }

    #[test]
    fn writer_loose_list_separates_items() {
        let l = Block::List {
            start: None,
            tight: false,
            items: vec![vec![para("a")], vec![para("b")]],
        };
        assert_eq!(write(&Document { blocks: vec![l] }).0, "- a\n\n- b\n");
    }

    #[test]
    fn writer_code_quote_rule_html_block() {
        let d = Document {
            blocks: vec![
                Block::CodeBlock {
                    info: "rs".into(),
                    text: "x\n\ny\n".into(),
                },
                Block::BlockQuote(vec![para("q1"), para("q2")]),
                Block::ThematicBreak,
                Block::HtmlBlock("<p>".into()),
            ],
        };
        let (s, l) = write(&d);
        assert_eq!(s, "    x\n\n    y\n\n> q1\n>\n> q2\n\n----\n");
        assert_eq!(l, BTreeSet::from([Loss::Html]));
    }

    #[test]
    fn loss_messages_cover_every_variant() {
        use Loss::*;
        for l in [
            Emphasis,
            InlineCode,
            Links,
            Images,
            Html,
            HeadingLevels,
            BlankLines,
            Whitespace,
        ] {
            assert!(!l.message().is_empty());
        }
        assert_eq!(Emphasis.message(), "emphasis removed");
    }

    #[test]
    fn writer_trailing_newline_exactly_one() {
        assert_eq!(write(&Document { blocks: vec![] }).0, "");
        let html_only = Document {
            blocks: vec![
                Block::HtmlBlock("<p>".into()),
                Block::Paragraph(vec![Inline::Html("<b>".into())]),
            ],
        };
        assert_eq!(write(&html_only).0, "");
        let s = write(&Document {
            blocks: vec![para("a"), para("b")],
        })
        .0;
        assert!(s.ends_with('\n') && !s.ends_with("\n\n"));
    }

    #[test]
    fn writer_skips_empty_blocks() {
        let d = Document {
            blocks: vec![
                para("a"),
                Block::Paragraph(vec![Inline::Html("<b>".into())]),
                Block::BlockQuote(vec![]),
                Block::List {
                    start: None,
                    tight: true,
                    items: vec![],
                },
                para("b"),
            ],
        };
        assert_eq!(write(&d).0, "a\n\nb\n");
    }

    #[test]
    fn writer_ordered_list_nesting_uses_marker_width() {
        let inner = Block::List {
            start: None,
            tight: true,
            items: vec![vec![para("b")]],
        };
        let l = Block::List {
            start: Some(1),
            tight: true,
            items: vec![vec![para("a"), inner]],
        };
        assert_eq!(write(&Document { blocks: vec![l] }).0, "1. a\n   - b\n");
    }
}
