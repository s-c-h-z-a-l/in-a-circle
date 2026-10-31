//! HTML writer reproducing the CommonMark and GFM reference renderers
//! (cmark, cmark-gfm).
//!
//! Exists so a Markdown reader can be checked against the spec examples
//! (markdown -> expected HTML); not a user-facing format.

use crate::{Align, Block, Document, Inline, ListItem};

/// Writer switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    /// The GFM tagfilter: the `<` of nine raw HTML tags (see [`FILTERED`])
    /// is written as `&lt;`. Off by default, as in cmark.
    pub tagfilter: bool,
}

/// The tags the GFM tagfilter neutralizes, lowercase.
const FILTERED: [&str; 9] = [
    "title",
    "textarea",
    "style",
    "xmp",
    "iframe",
    "noembed",
    "noframes",
    "script",
    "plaintext",
];

/// Renders `doc` the way cmark does.
pub fn write(doc: &Document) -> String {
    write_with(doc, Options::default())
}

/// Renders `doc` the way cmark-gfm does, with `opts`.
pub fn write_with(doc: &Document, opts: Options) -> String {
    let mut out = String::new();
    blocks(&mut out, &doc.blocks, false, opts);
    out
}

/// Raw HTML `h` into `out`; under the tagfilter, a `<` that opens or closes
/// one of [`FILTERED`] (the name case-insensitive, followed by whitespace,
/// `>`, `/>` or the end) becomes `&lt;`.
fn raw(out: &mut String, h: &str, opts: Options) {
    if !opts.tagfilter {
        out.push_str(h);
        return;
    }
    let b = h.as_bytes();
    let mut last = 0;
    for (i, _) in h.match_indices('<') {
        let name = &b[i + 1 + usize::from(b.get(i + 1) == Some(&b'/'))..];
        let len = name
            .iter()
            .position(|c| !c.is_ascii_alphanumeric())
            .unwrap_or(name.len());
        let (tag, after) = name.split_at(len);
        let ends = match after {
            [] => true,
            [c, ..] if c.is_ascii_whitespace() || *c == b'>' => true,
            [b'/', b'>', ..] => true,
            _ => false,
        };
        if ends
            && FILTERED
                .iter()
                .any(|f| f.as_bytes().eq_ignore_ascii_case(tag))
        {
            out.push_str(&h[last..i]);
            out.push_str("&lt;");
            last = i + 1;
        }
    }
    out.push_str(&h[last..]);
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

fn blocks(out: &mut String, bs: &[Block], tight: bool, opts: Options) {
    for b in bs {
        block(out, b, tight, opts);
    }
}

fn block(out: &mut String, b: &Block, tight: bool, opts: Options) {
    match b {
        Block::Paragraph(c) => {
            if tight {
                inlines(out, c, opts);
            } else {
                cr(out);
                out.push_str("<p>");
                inlines(out, c, opts);
                out.push_str("</p>\n");
            }
        }
        Block::Heading { level, content } => {
            cr(out);
            out.push_str(&format!("<h{level}>"));
            inlines(out, content, opts);
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
            blocks(out, c, false, opts);
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
                list_item(out, item, *tight, opts);
                out.push_str("</li>\n");
            }
            cr(out);
            out.push_str(&format!("</{tag}>\n"));
        }
        Block::Table { align, head, rows } => {
            cr(out);
            out.push_str("<table>\n<thead>\n");
            table_row(out, "th", align, head, opts);
            out.push_str("</thead>\n");
            if !rows.is_empty() {
                out.push_str("<tbody>\n");
                for row in rows {
                    table_row(out, "td", align, row, opts);
                }
                out.push_str("</tbody>\n");
            }
            out.push_str("</table>\n");
        }
        Block::ThematicBreak => {
            cr(out);
            out.push_str("<hr />\n");
        }
        Block::HtmlBlock(s) => {
            cr(out);
            raw(out, s, opts);
            cr(out);
        }
    }
}

/// One `<tr>`; a row shorter than `align` is padded with empty cells, a
/// longer one cut (the reader already makes every row exactly as wide).
fn table_row(out: &mut String, tag: &str, align: &[Align], cells: &[Vec<Inline>], opts: Options) {
    out.push_str("<tr>\n");
    for (i, a) in align.iter().enumerate() {
        out.push_str(&format!("<{tag}"));
        match a {
            Align::None => {}
            Align::Left => out.push_str(" align=\"left\""),
            Align::Center => out.push_str(" align=\"center\""),
            Align::Right => out.push_str(" align=\"right\""),
        }
        out.push('>');
        if let Some(cell) = cells.get(i) {
            inlines(out, cell, opts);
        }
        out.push_str(&format!("</{tag}>\n"));
    }
    out.push_str("</tr>\n");
}

/// The blocks of a list item; a task item gets its disabled checkbox at the
/// start of the first paragraph (inside it), or before the blocks when the
/// item does not start with one.
fn list_item(out: &mut String, item: &ListItem, tight: bool, opts: Options) {
    let Some(done) = item.checked else {
        blocks(out, &item.blocks, tight, opts);
        return;
    };
    let input = if done {
        "<input checked=\"\" disabled=\"\" type=\"checkbox\"> "
    } else {
        "<input disabled=\"\" type=\"checkbox\"> "
    };
    match item.blocks.split_first() {
        Some((Block::Paragraph(c), rest)) => {
            if tight {
                out.push_str(input);
                inlines(out, c, opts);
            } else {
                cr(out);
                out.push_str("<p>");
                out.push_str(input);
                inlines(out, c, opts);
                out.push_str("</p>\n");
            }
            blocks(out, rest, tight, opts);
        }
        _ => {
            out.push_str(input);
            blocks(out, &item.blocks, tight, opts);
        }
    }
}

fn inlines(out: &mut String, is: &[Inline], opts: Options) {
    for i in is {
        match i {
            Inline::Text(s) => escape(out, s),
            Inline::Emph(c) => {
                out.push_str("<em>");
                inlines(out, c, opts);
                out.push_str("</em>");
            }
            Inline::Strong(c) => {
                out.push_str("<strong>");
                inlines(out, c, opts);
                out.push_str("</strong>");
            }
            Inline::Strikethrough(c) => {
                out.push_str("<del>");
                inlines(out, c, opts);
                out.push_str("</del>");
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
                ..
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
                inlines(out, content, opts);
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
            Inline::Html(s) => raw(out, s, opts),
        }
    }
}

/// Plain-text rendering of inlines, for image `alt`.
fn plain(out: &mut String, is: &[Inline]) {
    for i in is {
        match i {
            Inline::Text(s) | Inline::Code(s) | Inline::Html(s) => escape(out, s),
            Inline::Emph(c) | Inline::Strong(c) | Inline::Strikethrough(c) => plain(out, c),
            Inline::Link { content, .. } => plain(out, content),
            Inline::Image { alt, .. } => plain(out, alt),
            Inline::SoftBreak | Inline::HardBreak => out.push(' '),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plain_items;

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
            auto: false,
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
            items: plain_items(items.clone()),
        };
        assert_eq!(doc(vec![tight]), "<ul>\n<li>a</li>\n<li>b</li>\n</ul>\n");
        let loose = Block::List {
            start: None,
            tight: false,
            items: plain_items(items),
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
            items: plain_items(vec![vec![p("b")]]),
        };
        let code = Block::CodeBlock {
            info: String::new(),
            text: "c\n".into(),
        };
        let l = Block::List {
            start: None,
            tight: true,
            items: plain_items(vec![vec![p("a"), inner], vec![code], vec![]]),
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
            items: plain_items(vec![vec![Block::BlockQuote(vec![p("a")])]]),
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
            items: plain_items(vec![vec![p("a")]]),
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
            auto: false,
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
                    auto: false,
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

    #[test]
    fn tagfilter_escapes_the_nine_tags() {
        let on = Options { tagfilter: true };
        for tag in FILTERED {
            let h = format!("<{tag}>x</{tag}>\n");
            let d = Document {
                blocks: vec![Block::HtmlBlock(h.clone())],
            };
            assert_eq!(
                write_with(&d, on),
                format!("&lt;{tag}>x&lt;/{tag}>\n"),
                "{tag}"
            );
            assert_eq!(write(&d), h, "off: {tag}");
        }
        // Case-insensitive, attributes, self-closing, inline HTML.
        let d = Document {
            blocks: vec![Block::Paragraph(vec![
                Inline::Html("<SCRIPT src=x>".into()),
                Inline::Html("<style/>".into()),
                Inline::Html("<scripts>".into()),
                Inline::Html("<b>".into()),
            ])],
        };
        assert_eq!(
            write_with(&d, on),
            "<p>&lt;SCRIPT src=x>&lt;style/><scripts><b></p>\n"
        );
    }

    #[test]
    fn write_is_write_with_default() {
        assert_eq!(Options::default(), Options { tagfilter: false });
        let d = Document {
            blocks: vec![
                Block::HtmlBlock("<script>\n".into()),
                p("a & b"),
                Block::Table {
                    align: vec![Align::None],
                    head: vec![t("h")],
                    rows: vec![],
                },
            ],
        };
        assert_eq!(write(&d), write_with(&d, Options::default()));
    }

    #[test]
    fn gfm_nodes_follow_the_reference() {
        let d = doc(vec![Block::Table {
            align: vec![Align::Left, Align::Center, Align::Right, Align::None],
            head: vec![t("a"), t("b"), t("c"), t("d")],
            rows: vec![vec![t("1"), t("2"), t("3"), vec![]]],
        }]);
        assert_eq!(
            d,
            "<table>\n<thead>\n<tr>\n<th align=\"left\">a</th>\n<th align=\"center\">b</th>\n\
<th align=\"right\">c</th>\n<th>d</th>\n</tr>\n</thead>\n<tbody>\n<tr>\n\
<td align=\"left\">1</td>\n<td align=\"center\">2</td>\n<td align=\"right\">3</td>\n\
<td></td>\n</tr>\n</tbody>\n</table>\n"
        );
        let s = doc(vec![Block::Paragraph(vec![Inline::Strikethrough(t("x"))])]);
        assert_eq!(s, "<p><del>x</del></p>\n");
        let item = |done, blocks| ListItem {
            checked: Some(done),
            blocks,
        };
        let l = |tight| Block::List {
            start: None,
            tight,
            items: vec![item(false, vec![p("a")]), item(true, vec![p("b")])],
        };
        assert_eq!(
            doc(vec![l(true)]),
            "<ul>\n<li><input disabled=\"\" type=\"checkbox\"> a</li>\n\
<li><input checked=\"\" disabled=\"\" type=\"checkbox\"> b</li>\n</ul>\n"
        );
        assert_eq!(
            doc(vec![l(false)]),
            "<ul>\n<li>\n<p><input disabled=\"\" type=\"checkbox\"> a</p>\n</li>\n\
<li>\n<p><input checked=\"\" disabled=\"\" type=\"checkbox\"> b</p>\n</li>\n</ul>\n"
        );
    }
}
