//! Plain-text reader and writer.

use crate::budget::{Meter, Sink};
use crate::{Block, Document, Inline, LimitError, Limits, Loss, ReadError};
use std::collections::BTreeSet;

/// Reads plain text. Paragraphs are separated by blank (whitespace-only)
/// lines; lines inside a paragraph are joined by `HardBreak`. Runs of more
/// than one blank line between paragraphs are merged, blank lines before
/// the first and after the last paragraph and the spaces of whitespace-only
/// lines are dropped; each is reported as `Loss::BlankLines`.
/// Text is valid UTF-8 without NUL bytes; anything else is
/// `ReadError::NotText`. Linear in the input; no recursion. Unbudgeted
/// compatibility entry point; the shipped conversion uses [`read_limited`].
pub fn read(bytes: &[u8]) -> Result<(Document, BTreeSet<Loss>), ReadError> {
    read_limited(bytes, Limits::UNLIMITED)
}

/// [`read`] under `limits`: the input size is checked first, then every
/// paragraph, `Text` and `HardBreak` is charged as one node and every line
/// as its bytes of owned text before it is built (`ReadError::Limit`).
pub fn read_limited(bytes: &[u8], limits: Limits) -> Result<(Document, BTreeSet<Loss>), ReadError> {
    if bytes.len() > limits.input_bytes {
        return Err(LimitError::Exceeded("input").into());
    }
    let s = std::str::from_utf8(bytes).map_err(|_| ReadError::NotText)?;
    if s.contains('\0') {
        return Err(ReadError::NotText);
    }
    let s = s.strip_prefix('\u{feff}').unwrap_or(s);
    let mut m = Meter::new(limits);
    let mut losses = BTreeSet::new();
    let mut blocks = Vec::new();
    let mut inlines: Vec<Inline> = Vec::new();
    let mut blanks = 0usize;

    // Split on LF, CRLF and lone CR without allocating a normalized copy.
    let mut rest = s;
    while !rest.is_empty() && !m.failed() {
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
            // A whitespace-only line keeps nothing of its spaces.
            if !line.is_empty() {
                losses.insert(Loss::BlankLines);
            }
            blanks += 1;
            continue;
        }
        // A paragraph is charged when its first line starts it.
        if inlines.is_empty() {
            // Blank lines before the first paragraph are dropped.
            if blanks > 0 {
                losses.insert(Loss::BlankLines);
            }
            m.node();
        } else if blanks > 0 {
            if blanks > 1 {
                losses.insert(Loss::BlankLines);
            }
            m.push(&mut blocks, Block::Paragraph(std::mem::take(&mut inlines)));
            m.node();
        } else if m.node() {
            m.push(&mut inlines, Inline::HardBreak);
        }
        blanks = 0;
        if m.node() {
            let t = m.own(line);
            m.push(&mut inlines, Inline::Text(t));
        }
    }
    // Blank lines after the last paragraph (or a text of blank lines only)
    // are dropped. The last line's own terminator is not a blank line.
    if blanks > 0 && !m.failed() {
        losses.insert(Loss::BlankLines);
    }
    if !inlines.is_empty() {
        m.push(&mut blocks, Block::Paragraph(inlines));
    }
    m.result()?;
    Ok((Document { blocks }, losses))
}

/// Writes plain text, reporting every piece of structure that was flattened.
/// Blocks are separated by one blank line (blocks that render to nothing are
/// skipped); non-empty output ends with exactly one `\n`, empty output is `""`.
/// List nesting: content under a bullet item is indented 2 spaces, under an
/// ordered item by the marker's width (3 spaces under `1. `).
/// Unbudgeted compatibility entry point; see [`write_limited`].
pub fn write(doc: &Document) -> (String, BTreeSet<Loss>) {
    write_limited(doc, Limits::UNLIMITED).expect("allocation failed while writing plain text")
}

/// [`write`] under `limits.output_bytes`: every piece is counted before it
/// is appended, so the output never grows past the budget.
pub fn write_limited(
    doc: &Document,
    limits: Limits,
) -> Result<(String, BTreeSet<Loss>), LimitError> {
    let mut w = Writer {
        sink: Sink::new(limits),
        losses: BTreeSet::new(),
    };
    let mut out = w.blocks(&doc.blocks, true);
    if !out.is_empty() {
        w.sink.push(&mut out, '\n');
    }
    w.sink.result()?;
    Ok((out, w.losses))
}

struct Writer {
    sink: Sink,
    losses: BTreeSet<Loss>,
}

impl Writer {
    fn blocks(&mut self, blocks: &[Block], loose: bool) -> String {
        let sep = if loose { "\n\n" } else { "\n" };
        let mut out = String::new();
        let mut any = false;
        for b in blocks {
            if self.sink.failed() {
                break;
            }
            if let Some(s) = self.block(b)
                && !s.is_empty()
            {
                if any {
                    self.sink.push_str(&mut out, sep);
                }
                self.sink.push_str(&mut out, &s);
                any = true;
            }
        }
        out
    }

    fn block(&mut self, b: &Block) -> Option<String> {
        Some(match b {
            Block::Paragraph(c) => self.inlines(c),
            Block::Heading { level, content } => {
                if *level != 1 {
                    self.losses.insert(Loss::HeadingLevels);
                }
                self.inlines(content)
            }
            Block::CodeBlock { text, .. } => {
                let text = text.strip_suffix('\n').unwrap_or(text);
                self.prefix_lines(text, "    ")
            }
            Block::BlockQuote(inner) => {
                let body = self.blocks(inner, true);
                if body.is_empty() {
                    return None;
                }
                let mut out = String::new();
                for (i, line) in body.split('\n').enumerate() {
                    if i > 0 {
                        self.sink.push(&mut out, '\n');
                    }
                    self.sink
                        .push_str(&mut out, if line.is_empty() { ">" } else { "> " });
                    self.sink.push_str(&mut out, line);
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
                    if self.sink.failed() {
                        break;
                    }
                    if i > 0 {
                        self.sink
                            .push_str(&mut out, if *tight { "\n" } else { "\n\n" });
                    }
                    let marker = match start {
                        None => "- ".to_string(),
                        Some(n) => format!("{}. ", n.saturating_add(i as u64)),
                    };
                    let mut body = self.blocks(&item.blocks, !*tight);
                    if let Some(done) = item.checked {
                        let mut marked = String::new();
                        self.sink
                            .push_str(&mut marked, if done { "[x] " } else { "[ ] " });
                        self.sink.push_str(&mut marked, &body);
                        body = marked;
                    }
                    let pad = if start.is_none() { 2 } else { marker.len() };
                    for (j, line) in body.split('\n').enumerate() {
                        if j == 0 {
                            self.sink.push_str(&mut out, &marker);
                        } else {
                            self.sink.push(&mut out, '\n');
                            if !line.is_empty() {
                                for _ in 0..pad {
                                    self.sink.push(&mut out, ' ');
                                }
                            }
                        }
                        self.sink.push_str(&mut out, line);
                    }
                    if body.is_empty() {
                        out.truncate(out.trim_end().len());
                    }
                }
                out
            }
            Block::Table { head, rows, .. } => {
                // One line per row, cells separated by two spaces.
                self.losses.insert(Loss::TableLayout);
                let mut out = String::new();
                for (i, row) in std::iter::once(head).chain(rows.iter()).enumerate() {
                    if i > 0 {
                        self.sink.push(&mut out, '\n');
                    }
                    for (j, cell) in row.iter().enumerate() {
                        if j > 0 {
                            self.sink.push_str(&mut out, "  ");
                        }
                        self.push_inlines(cell, &mut out);
                    }
                }
                out
            }
            Block::ThematicBreak => "----".to_string(),
            Block::HtmlBlock(_) => {
                self.losses.insert(Loss::Html);
                return None;
            }
        })
    }

    fn prefix_lines(&mut self, text: &str, prefix: &str) -> String {
        let mut out = String::new();
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                self.sink.push(&mut out, '\n');
            }
            if !line.is_empty() {
                self.sink.push_str(&mut out, prefix);
            }
            self.sink.push_str(&mut out, line);
        }
        out
    }

    fn inlines(&mut self, inlines: &[Inline]) -> String {
        let mut out = String::new();
        self.push_inlines(inlines, &mut out);
        out
    }

    fn push_inlines(&mut self, inlines: &[Inline], out: &mut String) {
        for i in inlines {
            if self.sink.failed() {
                return;
            }
            match i {
                Inline::Text(t) => self.sink.push_str(out, t),
                Inline::Emph(c) | Inline::Strong(c) => {
                    self.losses.insert(Loss::Emphasis);
                    self.push_inlines(c, out);
                }
                Inline::Strikethrough(c) => {
                    self.losses.insert(Loss::Strikethrough);
                    self.push_inlines(c, out);
                }
                Inline::Code(t) => {
                    self.losses.insert(Loss::InlineCode);
                    self.sink.push_str(out, t);
                }
                Inline::Link { dest, content, .. } => {
                    self.losses.insert(Loss::Links);
                    let text = self.inlines(content);
                    if dest.is_empty() || text == *dest {
                        self.sink
                            .push_str(out, if dest.is_empty() { &text } else { dest });
                    } else if text.is_empty() {
                        self.sink.push_str(out, dest);
                    } else {
                        self.sink.push_str(out, &text);
                        self.sink.push_str(out, " (");
                        self.sink.push_str(out, dest);
                        self.sink.push(out, ')');
                    }
                }
                Inline::Image { alt, .. } => {
                    self.losses.insert(Loss::Images);
                    self.push_inlines(alt, out);
                }
                Inline::SoftBreak => self.sink.push(out, ' '),
                Inline::HardBreak => self.sink.push(out, '\n'),
                Inline::Html(_) => {
                    self.losses.insert(Loss::Html);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plain_items;

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
        assert_eq!(
            ReadError::NotText.to_string(),
            "not text (invalid UTF-8 or a NUL byte)"
        );
        assert_eq!(read(b"\xff").unwrap_err(), ReadError::NotText);
    }

    #[test]
    fn nul_byte_is_error() {
        assert_eq!(read(b"a\0b").unwrap_err(), ReadError::NotText);
        assert_eq!(read(b"\0").unwrap_err(), ReadError::NotText);
    }

    #[test]
    fn empty_and_blank_input() {
        let (d, l) = read(b"").unwrap();
        assert!(d.blocks.is_empty() && l.is_empty());
        // Nothing but blank lines: they are dropped, and said so.
        let (d, l) = read(b"  \n\n").unwrap();
        assert!(d.blocks.is_empty());
        assert!(l.contains(&Loss::BlankLines));
    }

    #[test]
    fn edge_and_whitespace_only_lines_reported() {
        for input in [
            &b"\n\n\nalpha\n"[..],
            b"\nalpha\n",
            b"alpha\n\n\n\n",
            b"alpha\n\n",
            b"a\n   \nb\n",
            b"a\n\t\nb\n",
            b"a\r\n\r\n",
        ] {
            let (_, l) = read(input).unwrap();
            assert!(l.contains(&Loss::BlankLines), "{input:?}");
        }
        // Terminators and single blank separators lose nothing.
        for input in [&b"alpha\n"[..], b"alpha", b"a\n\nb\n", b"a\r\n\r\nb\r\n"] {
            let (_, l) = read(input).unwrap();
            assert!(l.is_empty(), "{input:?}");
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
            auto: false,
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
            items: plain_items(vec![vec![para("b")]]),
        };
        let outer = Block::List {
            start: None,
            tight: true,
            items: plain_items(vec![vec![para("a"), inner], vec![para("c")]]),
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
            items: plain_items(vec![vec![para("a")], vec![para("b")]]),
        };
        assert_eq!(write(&Document { blocks: vec![l] }).0, "3. a\n4. b\n");
    }

    #[test]
    fn writer_loose_list_separates_items() {
        let l = Block::List {
            start: None,
            tight: false,
            items: plain_items(vec![vec![para("a")], vec![para("b")]]),
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
                    items: plain_items(vec![]),
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
            items: plain_items(vec![vec![para("b")]]),
        };
        let l = Block::List {
            start: Some(1),
            tight: true,
            items: plain_items(vec![vec![para("a"), inner]]),
        };
        assert_eq!(write(&Document { blocks: vec![l] }).0, "1. a\n   - b\n");
    }
}
