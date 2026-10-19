//! HTML writer reproducing the CommonMark reference renderer (cmark).
//!
//! Exists so a Markdown reader can be checked against the CommonMark spec
//! examples (markdown -> expected HTML); not a user-facing format.

use crate::{Block, Document, Inline};

/// Renders `doc` the way cmark does.
pub fn write(doc: &Document) -> String {
    let mut out = String::new();
    blocks(&mut out, &doc.blocks, false);
    out
}

/// Ensures the output ends with a newline (cmark's `cr`).
fn cr(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn escape(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

fn href(out: &mut String, s: &str) {
    let b = s.as_bytes();
    for &c in b {
        match c {
            b'&' => out.push_str("&amp;"),
            b'\'' => out.push_str("&#x27;"),
            c if c.is_ascii_alphanumeric() || b"-_.+!*(),%#@?=;:/$~".contains(&c) => {
                out.push(c as char)
            }
            c => out.push_str(&format!("%{c:02X}")),
        }
    }
}

fn blocks(out: &mut String, bs: &[Block], tight: bool) {
    for b in bs {
        block(out, b, tight);
    }
}

fn block(out: &mut String, b: &Block, tight: bool) {
    match b {
        Block::Paragraph(c) => {
            if tight {
                inlines(out, c);
            } else {
                cr(out);
                out.push_str("<p>");
                inlines(out, c);
                out.push_str("</p>\n");
            }
        }
        Block::Heading { level, content } => {
            cr(out);
            out.push_str(&format!("<h{level}>"));
            inlines(out, content);
            out.push_str(&format!("</h{level}>\n"));
        }
        Block::CodeBlock { info, text } => {
            cr(out);
            out.push_str("<pre><code");
            if !info.is_empty() {
                let end = info
                    .find(|c: char| c.is_ascii_whitespace() || c == '\u{b}')
                    .unwrap_or(info.len());
                let lang = &info[..end];
                out.push_str(" class=\"language-");
                escape(out, lang);
                out.push('"');
            }
            out.push('>');
            escape(out, text);
            out.push_str("</code></pre>\n");
        }
        Block::BlockQuote(c) => {
            cr(out);
            out.push_str("<blockquote>\n");
            blocks(out, c, false);
            cr(out);
            out.push_str("</blockquote>\n");
        }
        Block::List {
            start,
            tight,
            items,
        } => {
            cr(out);
            let tag = if start.is_some() { "ol" } else { "ul" };
            match start {
                Some(n) if *n != 1 => out.push_str(&format!("<ol start=\"{n}\">\n")),
                _ => out.push_str(&format!("<{tag}>\n")),
            }
            for item in items {
                cr(out);
                out.push_str("<li>");
                blocks(out, item, *tight);
                out.push_str("</li>\n");
            }
            cr(out);
            out.push_str(&format!("</{tag}>\n"));
        }
        Block::ThematicBreak => {
            cr(out);
            out.push_str("<hr />\n");
        }
        Block::HtmlBlock(s) => {
            cr(out);
            out.push_str(s);
            cr(out);
        }
    }
}

fn inlines(out: &mut String, is: &[Inline]) {
    for i in is {
        match i {
            Inline::Text(s) => escape(out, s),
            Inline::Emph(c) => {
                out.push_str("<em>");
                inlines(out, c);
                out.push_str("</em>");
            }
            Inline::Strong(c) => {
                out.push_str("<strong>");
                inlines(out, c);
                out.push_str("</strong>");
            }
            Inline::Code(s) => {
                out.push_str("<code>");
                escape(out, s);
                out.push_str("</code>");
            }
            Inline::Link {
                dest,
                title,
                content,
            } => {
                out.push_str("<a href=\"");
                href(out, dest);
                out.push('"');
                if !title.is_empty() {
                    out.push_str(" title=\"");
                    escape(out, title);
                    out.push('"');
                }
                out.push('>');
                inlines(out, content);
                out.push_str("</a>");
            }
            Inline::Image { dest, title, alt } => {
                out.push_str("<img src=\"");
                href(out, dest);
                out.push_str("\" alt=\"");
                plain(out, alt);
                out.push('"');
                if !title.is_empty() {
                    out.push_str(" title=\"");
                    escape(out, title);
                    out.push('"');
                }
                out.push_str(" />");
            }
            Inline::SoftBreak => out.push('\n'),
            Inline::HardBreak => out.push_str("<br />\n"),
            Inline::Html(s) => out.push_str(s),
        }
    }
}

/// Plain-text rendering of inlines, for image `alt`.
fn plain(out: &mut String, is: &[Inline]) {
    for i in is {
        match i {
            Inline::Text(s) | Inline::Code(s) | Inline::Html(s) => escape(out, s),
            Inline::Emph(c) | Inline::Strong(c) => plain(out, c),
            Inline::Link { content, .. } => plain(out, content),
            Inline::Image { alt, .. } => plain(out, alt),
            Inline::SoftBreak | Inline::HardBreak => out.push(' '),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Vec<Inline> {
        vec![Inline::Text(s.into())]
    }
    fn p(s: &str) -> Block {
        Block::Paragraph(t(s))
    }
    fn doc(blocks: Vec<Block>) -> String {
        write(&Document { blocks })
    }
    fn link(dest: &str) -> Block {
        Block::Paragraph(vec![Inline::Link {
            dest: dest.into(),
            title: String::new(),
            content: t("x"),
        }])
    }

    #[test]
    fn paragraph() {
        assert_eq!(doc(vec![p("hi")]), "<p>hi</p>\n");
        assert_eq!(doc(vec![]), "");
    }

    #[test]
    fn headings_and_hr() {
        let h = Block::Heading {
            level: 3,
            content: t("a"),
        };
        assert_eq!(doc(vec![h, Block::ThematicBreak]), "<h3>a</h3>\n<hr />\n");
    }

    #[test]
    fn inline_forms() {
        let d = doc(vec![Block::Paragraph(vec![
            Inline::Emph(t("a")),
            Inline::Strong(t("b")),
            Inline::Code("<c>".into()),
            Inline::SoftBreak,
            Inline::HardBreak,
            Inline::Html("<b>".into()),
        ])]);
        assert_eq!(
            d,
            "<p><em>a</em><strong>b</strong><code>&lt;c&gt;</code>\n<br />\n<b></p>\n"
        );
    }

    #[test]
    fn tight_vs_loose_list() {
        let items = vec![vec![p("a")], vec![p("b")]];
        let tight = Block::List {
            start: None,
            tight: true,
            items: items.clone(),
        };
        assert_eq!(doc(vec![tight]), "<ul>\n<li>a</li>\n<li>b</li>\n</ul>\n");
        let loose = Block::List {
            start: None,
            tight: false,
            items,
        };
        assert_eq!(
            doc(vec![loose]),
            "<ul>\n<li>\n<p>a</p>\n</li>\n<li>\n<p>b</p>\n</li>\n</ul>\n"
        );
    }

    #[test]
    fn tight_nested_and_mixed_items() {
        let inner = Block::List {
            start: None,
            tight: true,
            items: vec![vec![p("b")]],
        };
        let code = Block::CodeBlock {
            info: String::new(),
            text: "c\n".into(),
        };
        let l = Block::List {
            start: None,
            tight: true,
            items: vec![vec![p("a"), inner], vec![code], vec![]],
        };
        assert_eq!(
            doc(vec![l]),
            "<ul>\n<li>a\n<ul>\n<li>b</li>\n</ul>\n</li>\n<li>\n<pre><code>c\n</code></pre>\n</li>\n<li></li>\n</ul>\n"
        );
    }

    #[test]
    fn blockquote_in_tight_item_keeps_p() {
        let l = Block::List {
            start: None,
            tight: true,
            items: vec![vec![Block::BlockQuote(vec![p("a")])]],
        };
        assert_eq!(
            doc(vec![l]),
            "<ul>\n<li>\n<blockquote>\n<p>a</p>\n</blockquote>\n</li>\n</ul>\n"
        );
        assert_eq!(
            doc(vec![Block::BlockQuote(vec![])]),
            "<blockquote>\n</blockquote>\n"
        );
    }

    #[test]
    fn ordered_start() {
        let mk = |n| Block::List {
            start: Some(n),
            tight: true,
            items: vec![vec![p("a")]],
        };
        assert_eq!(doc(vec![mk(1)]), "<ol>\n<li>a</li>\n</ol>\n");
        assert_eq!(doc(vec![mk(3)]), "<ol start=\"3\">\n<li>a</li>\n</ol>\n");
        assert_eq!(doc(vec![mk(0)]), "<ol start=\"0\">\n<li>a</li>\n</ol>\n");
    }

    #[test]
    fn code_block_language_class() {
        let c = |info: &str| Block::CodeBlock {
            info: info.into(),
            text: "x < y\n".into(),
        };
        assert_eq!(doc(vec![c("")]), "<pre><code>x &lt; y\n</code></pre>\n");
        assert_eq!(
            doc(vec![c("rust no_run")]),
            "<pre><code class=\"language-rust\">x &lt; y\n</code></pre>\n"
        );
        assert_eq!(
            doc(vec![Block::CodeBlock {
                info: String::new(),
                text: String::new()
            }]),
            "<pre><code></code></pre>\n"
        );
    }

    #[test]
    fn code_info_splits_on_ascii_whitespace_only() {
        let c = |info: &str| {
            doc(vec![Block::CodeBlock {
                info: info.into(),
                text: String::new(),
            }])
        };
        assert_eq!(c("a\tb"), "<pre><code class=\"language-a\"></code></pre>\n");
        assert_eq!(
            c("a\u{a0}b c"),
            "<pre><code class=\"language-a\u{a0}b\"></code></pre>\n"
        );
    }

    #[test]
    fn html_block_verbatim() {
        let h = Block::HtmlBlock("<div>\n*x*".into());
        assert_eq!(
            doc(vec![p("a"), h, p("b")]),
            "<p>a</p>\n<div>\n*x*\n<p>b</p>\n"
        );
    }

    #[test]
    fn escapes() {
        assert_eq!(
            doc(vec![p("a & <b> \"c\" 'd'")]),
            "<p>a &amp; &lt;b&gt; &quot;c&quot; 'd'</p>\n"
        );
    }

    #[test]
    fn link_url_percent_encoding() {
        assert_eq!(doc(vec![link("a b")]), "<p><a href=\"a%20b\">x</a></p>\n");
        assert_eq!(
            doc(vec![link("foo%20b\u{e4}")]),
            "<p><a href=\"foo%20b%C3%A4\">x</a></p>\n"
        );
        assert_eq!(doc(vec![link("100%")]), "<p><a href=\"100%\">x</a></p>\n");
        assert_eq!(doc(vec![link("%4")]), "<p><a href=\"%4\">x</a></p>\n");
        assert_eq!(
            doc(vec![link("a?x=1&y='2'#f")]),
            "<p><a href=\"a?x=1&amp;y=&#x27;2&#x27;#f\">x</a></p>\n"
        );
        assert_eq!(
            doc(vec![link("[\\\"]")]),
            "<p><a href=\"%5B%5C%22%5D\">x</a></p>\n"
        );
        assert_eq!(doc(vec![link("")]), "<p><a href=\"\">x</a></p>\n");
    }

    #[test]
    fn link_title_only_when_non_empty() {
        let l = Block::Paragraph(vec![Inline::Link {
            dest: "u".into(),
            title: "a\"b".into(),
            content: t("x"),
        }]);
        assert_eq!(
            doc(vec![l]),
            "<p><a href=\"u\" title=\"a&quot;b\">x</a></p>\n"
        );
    }

    #[test]
    fn image_alt_is_plain_text() {
        let i = Block::Paragraph(vec![Inline::Image {
            dest: "a b".into(),
            title: "t".into(),
            alt: vec![
                Inline::Text("foo ".into()),
                Inline::Emph(t("bar")),
                Inline::Code("<".into()),
                Inline::Link {
                    dest: "u".into(),
                    title: String::new(),
                    content: t("l"),
                },
            ],
        }]);
        assert_eq!(
            doc(vec![i]),
            "<p><img src=\"a%20b\" alt=\"foo bar&lt;l\" title=\"t\" /></p>\n"
        );
        let n = Block::Paragraph(vec![Inline::Image {
            dest: "u".into(),
            title: String::new(),
            alt: vec![],
        }]);
        assert_eq!(doc(vec![n]), "<p><img src=\"u\" alt=\"\" /></p>\n");
    }

    #[test]
    fn image_alt_plain_branches() {
        let i = Block::Paragraph(vec![Inline::Image {
            dest: "u".into(),
            title: String::new(),
            alt: vec![
                Inline::Text("a".into()),
                Inline::Html("<b>".into()),
                Inline::SoftBreak,
                Inline::HardBreak,
                Inline::Image {
                    dest: "v".into(),
                    title: String::new(),
                    alt: t("in\""),
                },
            ],
        }]);
        assert_eq!(
            doc(vec![i]),
            "<p><img src=\"u\" alt=\"a&lt;b&gt;  in&quot;\" /></p>\n"
        );
    }
}
