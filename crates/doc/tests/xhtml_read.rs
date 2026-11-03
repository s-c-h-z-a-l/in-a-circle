//! The XHTML chapter reader (`doc::xhtml_read`): the element subset it maps
//! to the document tree, what it drops (and reports), its budgets and its
//! linearity on hostile shapes.

use std::collections::BTreeSet;

use doc::xhtml_read::{
    Ids, XhtmlError, has_text, read_limited, read_steps, read_with_ids, read_with_ids_steps,
};
use doc::{Align, Block, Document, Inline, LimitError, Limits, ListItem, Loss, txt};

const XHTML: &str = "http://www.w3.org/1999/xhtml";

fn page(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="{XHTML}"><head><title>T</title></head><body>{body}</body></html>"#
    )
}

fn read(s: &str) -> (Document, BTreeSet<Loss>) {
    read_limited(s, Limits::DEFAULT).unwrap_or_else(|e| panic!("{e:?} for {s}"))
}

fn blocks(body: &str) -> Vec<Block> {
    read(&page(body)).0.blocks
}

fn t(s: &str) -> Inline {
    Inline::Text(s.to_string())
}

fn para(inlines: Vec<Inline>) -> Block {
    Block::Paragraph(inlines)
}

fn item(blocks: Vec<Block>) -> ListItem {
    ListItem {
        checked: None,
        blocks,
    }
}

#[test]
fn paragraphs_headings_lists_quotes_pre() {
    let b = blocks(
        "<h1>Title</h1><p>One two.</p><h3>Sub</h3>\
         <ul><li>a</li><li>b <em>c</em></li></ul>\
         <ol><li><p>x</p></li><li><p>y</p><p>z</p></li></ol>\
         <blockquote><p>q</p><blockquote>r</blockquote></blockquote>\
         <pre>\nline 1\n  line 2</pre>",
    );
    assert_eq!(
        b,
        vec![
            Block::Heading {
                level: 1,
                content: vec![t("Title")]
            },
            para(vec![t("One two.")]),
            Block::Heading {
                level: 3,
                content: vec![t("Sub")]
            },
            Block::List {
                start: None,
                tight: true,
                items: vec![
                    item(vec![para(vec![t("a")])]),
                    item(vec![para(vec![t("b "), Inline::Emph(vec![t("c")])])]),
                ],
            },
            Block::List {
                start: Some(1),
                tight: false,
                items: vec![
                    item(vec![para(vec![t("x")])]),
                    item(vec![para(vec![t("y")]), para(vec![t("z")])]),
                ],
            },
            Block::BlockQuote(vec![
                para(vec![t("q")]),
                Block::BlockQuote(vec![para(vec![t("r")])]),
            ]),
            // The line feed right after `<pre>` is dropped, as HTML does;
            // the text ends with one line feed, like a Markdown code block.
            Block::CodeBlock {
                info: String::new(),
                text: "line 1\n  line 2\n".to_string()
            },
        ]
    );
}

#[test]
fn ordered_list_start() {
    let start = |body: &str| match &blocks(body)[0] {
        Block::List { start, .. } => *start,
        other => panic!("{other:?}"),
    };
    assert_eq!(start(r#"<ol start="3"><li>x</li></ol>"#), Some(3));
    assert_eq!(start(r#"<ol start="0"><li>x</li></ol>"#), Some(0));
    assert_eq!(start("<ol><li>x</li></ol>"), Some(1));
    // Not a number HTML would take: the default start.
    assert_eq!(start(r#"<ol start="junk"><li>x</li></ol>"#), Some(1));
    assert_eq!(start(r#"<ol start="-2"><li>x</li></ol>"#), Some(1));
    assert_eq!(start("<ul><li>x</li></ul>"), None);
    // Clamped to the highest number a Markdown list marker holds (nine
    // digits), so the items still count up.
    assert_eq!(
        start(r#"<ol start="18446744073709551615"><li>x</li></ol>"#),
        Some(999_999_999)
    );
    assert_eq!(
        start(r#"<ol start="1000000000"><li>x</li></ol>"#),
        Some(999_999_999)
    );
    assert_eq!(
        start(r#"<ol start="999999999"><li>x</li></ol>"#),
        Some(999_999_999)
    );
}

#[test]
fn inline_emphasis_strong_strike_code_links_images() {
    let b = blocks(
        r#"<p><em>e</em> <i>i</i> <strong>s</strong> <b>b</b> <del>d</del> <s>s2</s> <strike>k</strike> <code>c   d</code> <kbd>K</kbd> <samp>S</samp> <a href="http://x.y/" title="T">l <b>m</b></a> <img src="a.jpg" alt="Alt" title="IT"/> <a name="n">plain</a></p>"#,
    );
    let sp = || t(" ");
    assert_eq!(
        b,
        vec![para(vec![
            Inline::Emph(vec![t("e")]),
            sp(),
            Inline::Emph(vec![t("i")]),
            sp(),
            Inline::Strong(vec![t("s")]),
            sp(),
            Inline::Strong(vec![t("b")]),
            sp(),
            Inline::Strikethrough(vec![t("d")]),
            sp(),
            Inline::Strikethrough(vec![t("s2")]),
            sp(),
            Inline::Strikethrough(vec![t("k")]),
            sp(),
            Inline::Code("c d".to_string()),
            sp(),
            Inline::Code("K".to_string()),
            sp(),
            Inline::Code("S".to_string()),
            sp(),
            Inline::Link {
                dest: "http://x.y/".to_string(),
                title: "T".to_string(),
                content: vec![t("l "), Inline::Strong(vec![t("m")])],
                auto: false,
            },
            sp(),
            Inline::Image {
                dest: "a.jpg".to_string(),
                title: "IT".to_string(),
                alt: vec![t("Alt")],
            },
            // An anchor without `href` is only its text.
            t(" plain"),
        ])]
    );
}

#[test]
fn nested_link_is_its_text() {
    let b = blocks(r#"<p><a href="o">a <a href="i">b</a></a></p>"#);
    assert_eq!(
        b,
        vec![para(vec![Inline::Link {
            dest: "o".to_string(),
            title: String::new(),
            content: vec![t("a b")],
            auto: false,
        }])]
    );
}

#[test]
fn br_and_hr() {
    let b = blocks("<p>a <br/> b<br/></p><hr/><p><br/>c</p><div>d<hr/>e</div>");
    assert_eq!(
        b,
        vec![
            // Spaces around a break and a break at either end of a
            // paragraph show nothing.
            para(vec![t("a"), Inline::HardBreak, t("b")]),
            Block::ThematicBreak,
            para(vec![t("c")]),
            para(vec![t("d")]),
            Block::ThematicBreak,
            para(vec![t("e")]),
        ]
    );
}

#[test]
fn transparent_containers_keep_text() {
    let b = blocks(
        r#"<div><section><p>a <span class="x">b</span></p><aside>c</aside><figure><img src="i.png" alt="x"/><figcaption>cap</figcaption></figure><foo>bar</foo><article><header>h</header>body</article></section></div>"#,
    );
    let img = Inline::Image {
        dest: "i.png".to_string(),
        title: String::new(),
        alt: vec![t("x")],
    };
    assert_eq!(
        b,
        vec![
            para(vec![t("a b")]),
            para(vec![t("c")]),
            para(vec![img]),
            para(vec![t("cap")]),
            para(vec![t("bar")]),
            para(vec![t("h")]),
            para(vec![t("body")]),
        ]
    );
}

#[test]
fn head_script_style_template_dropped() {
    let s = format!(
        r#"<html xmlns="{XHTML}"><head><title>Title</title><style>p {{ color: red }}</style><script>x()</script></head><body><p>a</p><script>alert()</script><template><p>t</p></template><style>s</style><noscript>n</noscript></body></html>"#
    );
    let (d, losses) = read(&s);
    assert_eq!(d.blocks, vec![para(vec![t("a")]), para(vec![t("n")])]);
    assert_eq!(losses, BTreeSet::from([Loss::Styles]));
}

#[test]
fn whitespace_collapses_except_pre() {
    let b = blocks("<p>\n  a   b\t\n c <em> d </em> e  </p>\n\n  <pre>  x   y\n</pre>");
    assert_eq!(
        b,
        vec![
            para(vec![t("a b c "), Inline::Emph(vec![t("d ")]), t("e")]),
            Block::CodeBlock {
                info: String::new(),
                text: "  x   y\n".to_string()
            },
        ]
    );
    // A no-break space is text, not white space.
    assert_eq!(
        blocks("<p>\u{a0}a\u{a0} </p>"),
        vec![para(vec![t("\u{a0}a\u{a0}")])]
    );
    // Inside `pre`, a break is a line feed and markup keeps its text.
    assert_eq!(
        blocks("<pre>a<br/><b>b</b>  c</pre>"),
        vec![Block::CodeBlock {
            info: String::new(),
            text: "a\nb  c\n".to_string()
        }]
    );
}

#[test]
fn blocks_inside_pre_keep_words_apart() {
    let code = |body: &str| match &blocks(body)[..] {
        [Block::CodeBlock { text, .. }] => text.clone(),
        other => panic!("{other:?}"),
    };
    // A block starts and ends a line, a cell is a space, inline markup
    // and an image's alt text stay where they are.
    assert_eq!(
        code(
            r#"<pre>a<b>b</b><br/>c<img alt="I"/><p>d</p><table><tr><td>e</td><td>f</td></tr></table>g</pre>"#
        ),
        "ab\ncI\nd\ne f\ng\n"
    );
    // A listing written one block per line.
    assert_eq!(
        code("<pre><div>line1</div><div>line2</div></pre>"),
        "line1\nline2\n"
    );
    // No line feed is doubled, none written before the first text.
    assert_eq!(code("<pre>x\n<div>y\n</div>\nz</pre>"), "x\ny\n\nz\n");
}

#[test]
fn inline_at_block_level_wrapped_in_paragraph() {
    let b = blocks(
        "loose <em>text</em><p>p</p>tail<blockquote>q <b>r</b></blockquote><ul>stray<li>i</li></ul>",
    );
    assert_eq!(
        b,
        vec![
            para(vec![t("loose "), Inline::Emph(vec![t("text")])]),
            para(vec![t("p")]),
            para(vec![t("tail")]),
            Block::BlockQuote(vec![para(vec![t("q "), Inline::Strong(vec![t("r")])])]),
            Block::List {
                start: None,
                tight: true,
                items: vec![
                    item(vec![para(vec![t("stray")])]),
                    item(vec![para(vec![t("i")])])
                ],
            },
        ]
    );
}

#[test]
fn blocks_inside_inline_content_become_line_breaks() {
    // A block where only inline content fits (a paragraph in a cell or in
    // emphasis) keeps its text, each block on its own line.
    let b = blocks("<p><em>a<p>b</p>c</em></p>");
    assert_eq!(
        b,
        vec![para(vec![Inline::Emph(vec![
            t("a"),
            Inline::HardBreak,
            t("b"),
            Inline::HardBreak,
            t("c")
        ])])]
    );
    let b = blocks("<table><tr><td><p>a</p><p>b</p><ul><li>c</li></ul></td></tr></table>");
    match &b[0] {
        Block::Table { head, .. } => {
            assert_eq!(head, &vec![vec![]]);
        }
        other => panic!("{other:?}"),
    }
    match &b[0] {
        Block::Table { rows, .. } => assert_eq!(
            rows,
            &vec![vec![vec![
                t("a"),
                Inline::HardBreak,
                t("b"),
                Inline::HardBreak,
                t("c")
            ]]]
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn table_header_from_th_or_thead() {
    let cell = |s: &str| vec![t(s)];
    let table = |head: Vec<Vec<Inline>>, rows: Vec<Vec<Vec<Inline>>>| Block::Table {
        align: vec![Align::None; head.len()],
        head,
        rows,
    };
    assert_eq!(
        blocks("<table><tr><th>h1</th><th>h2</th></tr><tr><td>a</td><td>b</td></tr></table>"),
        vec![table(
            vec![cell("h1"), cell("h2")],
            vec![vec![cell("a"), cell("b")]]
        )]
    );
    assert_eq!(
        blocks(
            "<table><caption>Cap</caption><thead><tr><td>H</td></tr></thead>\
             <tfoot><tr><td>F</td></tr></tfoot><tbody><tr><td>x</td></tr></tbody></table>"
        ),
        vec![
            para(vec![t("Cap")]),
            // The foot is the last row, wherever it is written.
            table(vec![cell("H")], vec![vec![cell("x")], vec![cell("F")]])
        ]
    );
    // No `th` row and no `thead`: no header (empty cells), every row kept.
    assert_eq!(
        blocks("<table><tr><td>a</td><td>b</td></tr><tr><td>c</td></tr></table>"),
        vec![table(
            vec![vec![], vec![]],
            vec![vec![cell("a"), cell("b")], vec![cell("c"), vec![]]]
        )]
    );
    // A first row mixing `th` and `td` is not a header.
    assert_eq!(
        blocks("<table><tr><th>a</th><td>b</td></tr></table>"),
        vec![table(
            vec![vec![], vec![]],
            vec![vec![cell("a"), cell("b")]]
        )]
    );
    // Text directly in a table or row is a cell of its own.
    assert_eq!(
        blocks("<table>loose<tr><td>a</td>x</tr></table>"),
        vec![table(
            vec![vec![], vec![]],
            vec![vec![cell("loose"), vec![]], vec![cell("a"), cell("x")]]
        )]
    );
    // A table without cells is nothing.
    assert_eq!(
        blocks("<table><tr></tr></table><p>p</p>"),
        vec![para(vec![t("p")])]
    );
}

#[test]
fn colspan_rowspan_merged_cells_loss() {
    let (d, losses) = read(&page(
        r#"<table><tr><th colspan="2">wide</th></tr><tr><td rowspan="2">tall</td><td>b</td></tr><tr><td>c</td></tr></table>"#,
    ));
    assert_eq!(
        d.blocks,
        vec![Block::Table {
            align: vec![Align::None; 2],
            head: vec![vec![t("wide")], vec![]],
            rows: vec![
                vec![vec![t("tall")], vec![t("b")]],
                vec![vec![], vec![t("c")]]
            ],
        }]
    );
    assert_eq!(losses, BTreeSet::from([Loss::MergedCells, Loss::Styles]));
    // Spans of 1, or ones that are not numbers, merge nothing.
    let (_, losses) = read(&page(
        r#"<table><tr><td colspan="1" rowspan="x">a</td></tr></table>"#,
    ));
    assert_eq!(losses, BTreeSet::from([Loss::Styles]));
    // A row span past the last row adds no rows.
    let (d, _) = read(&page(
        r#"<table><tr><td rowspan="65534">a</td></tr></table>"#,
    ));
    match &d.blocks[0] {
        Block::Table { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("{other:?}"),
    }
}

#[test]
fn huge_spans_stop_at_the_node_budget() {
    // HTML caps colspan at 1000 and rowspan at 65534; every empty cell a
    // span adds is a node, so a hostile grid stops at the budget.
    let mut s = String::from("<table><tr>");
    for _ in 0..1000 {
        s.push_str(r#"<td colspan="1000" rowspan="65534">a</td>"#);
    }
    s.push_str("</tr>");
    for _ in 0..100 {
        s.push_str("<tr/>");
    }
    s.push_str("</table>");
    let r = read_limited(&page(&s), Limits::DEFAULT);
    assert_eq!(r, Err(XhtmlError::Limit(LimitError::Exceeded("nodes"))));
}

#[test]
fn ruby_base_then_rt_in_parens() {
    assert_eq!(
        blocks("<p><ruby>漢<rp>(</rp><rt>kan</rt><rp>)</rp>字<rt>ji</rt></ruby>!</p>"),
        vec![para(vec![t("漢(kan)字(ji)!")])]
    );
}

#[test]
fn rp_dropped_only_when_it_holds_brackets() {
    let txt = |body: &str| txt::write(&read(&page(body)).0).0;
    // Brackets, ASCII or CJK, and white space: dropped, its white space
    // too (a hidden rp shows nothing).
    assert_eq!(
        txt("<p><ruby>漢<rp>（</rp><rt>kan</rt><rp> ） </rp></ruby>字</p>"),
        "漢(kan)字\n"
    );
    assert_eq!(txt("<p>a<rp> [ </rp>b <rp>{</rp> c</p>"), "ab c\n");
    // Any other text is kept, in its place, markup inside as its text.
    assert_eq!(
        txt("<p><ruby>b<rp>KEPT</rp><rt>r</rt></ruby> x</p>"),
        "bKEPT(r) x\n"
    );
    assert_eq!(txt("<p>a <rp>(<em>note</em>)</rp> b</p>"), "a (note) b\n");
    // At block level and inside pre.
    assert_eq!(txt("<div><rp>alone</rp></div>"), "alone\n");
    let b = blocks("<pre><ruby>a<rp>(</rp><rt>b</rt><rp>)</rp><rp>c</rp></ruby></pre>");
    assert_eq!(
        b,
        vec![Block::CodeBlock {
            info: String::new(),
            text: "a(b)c\n".into()
        }]
    );
}

#[test]
fn inline_svg_is_images_loss_mathml_text_kept() {
    let s = page(
        r#"<p>a <svg xmlns="http://www.w3.org/2000/svg" width="10"><text>drawn</text></svg> b <math xmlns="http://www.w3.org/1998/Math/MathML"><mi>x</mi><mo>+</mo><mn>1</mn></math></p>"#,
    );
    let (d, losses) = read(&s);
    let out = txt::write(&d).0;
    assert_eq!(out, "a b x+1\n");
    assert!(losses.contains(&Loss::Images), "{losses:?}");
    // Without SVG there is no image loss from the reader.
    let (_, losses) = read(&page("<p>a</p>"));
    assert!(!losses.contains(&Loss::Images));
}

#[test]
fn mathml_annotations_and_epub_cases_dropped_without_a_loss() {
    let read_txt = |body: &str| {
        let (d, losses) = read(&page(body));
        (txt::write(&d).0, losses)
    };
    let (out, losses) = read_txt(
        r#"<p>f <math xmlns="http://www.w3.org/1998/Math/MathML"><semantics><mi>x</mi><annotation encoding="application/x-tex">\alpha</annotation><annotation-xml encoding="MathML-Content"><ci>y</ci></annotation-xml></semantics></math> g</p>"#,
    );
    assert_eq!(out, "f x g\n");
    assert_eq!(losses, BTreeSet::from([Loss::Styles]));
    // MathML written without its namespace the same.
    let (out, _) = read_txt("<p><math><mi>x</mi><annotation>TEX</annotation></math></p>");
    assert_eq!(out, "x\n");
    let (d, _) = read_limited(
        "<html><body><p><math><mi>x</mi><annotation>TEX</annotation></math></p></body></html>",
        Limits::DEFAULT,
    )
    .unwrap();
    assert_eq!(txt::write(&d).0, "x\n");
    let (out, losses) = read_txt(
        r#"<div xmlns:epub="http://www.idpf.org/2007/ops"><epub:switch><epub:case required-namespace="http://www.w3.org/1998/Math/MathML"><p>case</p></epub:case><epub:default><p>fallback</p></epub:default></epub:switch></div>"#,
    );
    assert_eq!(out, "fallback\n");
    assert_eq!(losses, BTreeSet::from([Loss::Styles]));
    // Only those names, only in those namespaces.
    let (out, _) = read_txt(
        r#"<p><annotation xmlns="http://example.com/">kept</annotation> <case>kept2</case></p>"#,
    );
    assert_eq!(out, "kept kept2\n");
    // Outside a `math` an XHTML or un-namespaced `annotation` is an
    // unknown element: its text stays, also after a `math` has closed.
    let (out, losses) = read_txt(
        "<p>a<annotation>b</annotation><math><annotation>T</annotation></math><annotation-xml>c</annotation-xml></p>",
    );
    assert_eq!(out, "abc\n");
    assert_eq!(losses, BTreeSet::from([Loss::Styles]));
    let (d, _) = read_limited(
        "<html><body><p>x<annotation>y</annotation>z</p></body></html>",
        Limits::DEFAULT,
    )
    .unwrap();
    assert_eq!(txt::write(&d).0, "xyz\n");
}

#[test]
fn epub2_doctype_and_html_entities() {
    let bytes = br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN"
  "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd">

<html xmlns="http://www.w3.org/1999/xhtml">
<head>
  <title>Chapter 1</title>
  <link href="../Styles/stylesheet.css" rel="stylesheet" type="text/css"/>
</head>
<body>
  <h2 class="chapter">Chapter&nbsp;1</h2>
  <p class="calibre1">Caf&eacute;&nbsp;au lait &mdash; &copy;&#160;2001 &amp; &lt;x&gt; &#x263A;</p>
</body>
</html>
"#;
    let text = xml::decode(bytes).unwrap();
    let (d, _) = read(&text);
    assert_eq!(
        d.blocks,
        vec![
            Block::Heading {
                level: 2,
                content: vec![t("Chapter\u{a0}1")]
            },
            para(vec![t("Café\u{a0}au lait — ©\u{a0}2001 & <x> ☺")]),
        ]
    );
}

#[test]
fn unnamespaced_html_is_read_as_xhtml() {
    // A sloppy EPUB 2 chapter without the XHTML namespace: its HTML names
    // are taken as XHTML; other namespaces are not.
    let (d, _) = read(
        "<html><body><h1>T</h1><p>a <em>b</em></p><x:p xmlns:x=\"urn:x\">c</x:p></body></html>",
    );
    assert_eq!(
        d.blocks,
        vec![
            Block::Heading {
                level: 1,
                content: vec![t("T")]
            },
            para(vec![t("a "), Inline::Emph(vec![t("b")])]),
            para(vec![t("c")]),
        ]
    );
}

#[test]
fn not_well_formed_names_the_line() {
    let r = read_limited("<html>\n<body>\n<p>a</b></body></html>", Limits::DEFAULT);
    match r {
        Err(XhtmlError::Xml(e)) => {
            assert!(matches!(e.kind, xml::ErrorKind::NotWellFormed(_)));
            assert_eq!(e.line, 3);
        }
        other => panic!("{other:?}"),
    }
    let deep = format!("{}{}", "<div>".repeat(300), "</div>".repeat(300));
    match read_limited(&deep, Limits::DEFAULT) {
        Err(XhtmlError::Xml(e)) => assert_eq!(e.kind, xml::ErrorKind::TooDeep),
        other => panic!("{other:?}"),
    }
    // An HTML name is known; an unknown name is not.
    assert!(matches!(
        read_limited("<p>&nosuchname;</p>", Limits::DEFAULT),
        Err(XhtmlError::Xml(_))
    ));
}

#[test]
fn styles_loss_always() {
    let (d, losses) = read(&page(""));
    assert!(d.blocks.is_empty());
    assert_eq!(losses, BTreeSet::from([Loss::Styles]));
    let (_, losses) = read(&page("<p style=\"color: red\">a</p>"));
    assert_eq!(losses, BTreeSet::from([Loss::Styles]));
    assert_eq!(
        Loss::Styles.message(),
        "book styles (fonts, colours, layout) not carried over"
    );
    assert_eq!(Loss::MergedCells.message(), "merged table cells split");
}

#[test]
fn input_budget_is_checked_first() {
    let s = page("<p>abc</p>");
    let fits = Limits {
        input_bytes: s.len(),
        ..Limits::DEFAULT
    };
    assert!(read_limited(&s, fits).is_ok());
    let short = Limits {
        input_bytes: s.len() - 1,
        ..Limits::DEFAULT
    };
    assert_eq!(
        read_limited(&s, short),
        Err(XhtmlError::Limit(LimitError::Exceeded("input")))
    );
    // Before the XML: a broken text over the limit is "too large", not
    // "not well-formed".
    assert_eq!(
        read_limited(
            "<<<<",
            Limits {
                input_bytes: 3,
                ..Limits::DEFAULT
            }
        ),
        Err(XhtmlError::Limit(LimitError::Exceeded("input")))
    );
}

/// The smallest node budget under which `s` reads.
fn min_nodes(s: &str) -> usize {
    (0..10_000)
        .find(|&n| {
            read_limited(
                s,
                Limits {
                    nodes: n,
                    ..Limits::DEFAULT
                },
            )
            .is_ok()
        })
        .unwrap()
}

#[test]
fn nodes_budget_names_the_limit() {
    let s = page(&"<p>a <em>b</em></p>".repeat(20));
    let n = min_nodes(&s);
    assert!(n > 20, "{n}");
    let r = read_limited(
        &s,
        Limits {
            nodes: n - 1,
            ..Limits::DEFAULT
        },
    );
    assert_eq!(r, Err(XhtmlError::Limit(LimitError::Exceeded("nodes"))));
    // Every element is a node, kept or not: a chapter of empty spans
    // spends the budget too.
    let s = page(&"<span/>".repeat(100));
    let r = read_limited(
        &s,
        Limits {
            nodes: 100,
            ..Limits::DEFAULT
        },
    );
    assert_eq!(r, Err(XhtmlError::Limit(LimitError::Exceeded("nodes"))));
    assert!(
        read_limited(
            &s,
            Limits {
                nodes: 200,
                ..Limits::DEFAULT
            }
        )
        .is_ok()
    );
}

#[test]
fn text_budget_names_the_limit() {
    let s = page(&format!("<p>{}</p>", "x".repeat(100)));
    let ok = |n| {
        read_limited(
            &s,
            Limits {
                text_bytes: n,
                ..Limits::DEFAULT
            },
        )
        .is_ok()
    };
    assert!(ok(100));
    assert_eq!(
        read_limited(
            &s,
            Limits {
                text_bytes: 99,
                ..Limits::DEFAULT
            }
        ),
        Err(XhtmlError::Limit(LimitError::Exceeded("owned text")))
    );
    // A run of white space costs at most the one space it collapses to
    // (here the one after `a`, written before the paragraph's end drops
    // it); attribute text kept is charged too: 2 + 10 + 3.
    let s = page(r#"<p>   a    </p><p><img src="0123456789" alt="abc"/></p>"#);
    assert!(
        read_limited(
            &s,
            Limits {
                text_bytes: 15,
                ..Limits::DEFAULT
            }
        )
        .is_ok()
    );
    assert_eq!(
        read_limited(
            &s,
            Limits {
                text_bytes: 14,
                ..Limits::DEFAULT
            }
        ),
        Err(XhtmlError::Limit(LimitError::Exceeded("owned text")))
    );
}

/// Work per input byte the reader may do on top of the XML reader's own
/// (`xml::STEPS_PER_BYTE`): every text byte is scanned twice (measured,
/// then copied), each element and frame costs a few steps, and trimming
/// the space at the end of a paragraph walks down the last inline of each
/// level, at most once per element that built it. 8 leaves room for the
/// constants; a quadratic path at 4 MiB exceeds it by orders of magnitude.
const STEPS_PER_BYTE: u64 = 8;

fn assert_linear(s: &str) -> Result<(Document, BTreeSet<Loss>), XhtmlError> {
    let (r, steps) = read_steps(s, Limits::DEFAULT);
    assert!(
        steps <= STEPS_PER_BYTE * s.len() as u64,
        "{steps} steps for {} bytes",
        s.len()
    );
    r
}

#[test]
fn span_heavy_chapter_is_linear() {
    // Every word in its own styled span, as converters write: 4 MiB fits
    // the default budgets and reads in steps linear in its length.
    let mut body = String::from("<p>");
    let mut words = 0u64;
    while body.len() < 4 << 20 {
        body.push_str(&format!(
            r#"<span class="c{}">word{words}</span> "#,
            words % 7
        ));
        words += 1;
        if words.is_multiple_of(100) {
            body.push_str("</p>\n<p>");
        }
    }
    body.push_str("</p>");
    let s = page(&body);
    let (d, _) = assert_linear(&s).expect("a 4 MiB span-heavy chapter fits");
    assert!(has_text(&d));
    let out = txt::write(&d).0;
    assert!(out.contains(&format!("word{}", words - 1)));
    // Denser: two-byte words in bare spans pass the node budget, and the
    // failure names it.
    let dense = page(&"<span>a</span>".repeat(550_000));
    assert_eq!(
        assert_linear(&dense),
        Err(XhtmlError::Limit(LimitError::Exceeded("nodes")))
    );
    // A deep chain of emphasis closed before each break: the trailing
    // space is trimmed through the whole chain each time.
    let chain = format!("{}x {}<br/>", "<em>".repeat(200), "</em>".repeat(200));
    let s = page(&format!("<p>{}</p>", chain.repeat(2_000)));
    assert_linear(&s).unwrap();
    // Thousands of nested anchors, lists and table cells.
    let s = page(&format!(
        "{}{}",
        r#"<ul><li><a href="x"><table><tr><td>a</td></tr></table></a></li></ul>"#.repeat(20_000),
        "<p>a<br/></p>".repeat(20_000)
    ));
    assert_linear(&s).unwrap();
}

#[test]
fn nested_rp_is_linear() {
    // Only the outermost rp collects its text and decides once: nested rp
    // elements do not copy it again per level.
    let s = page(&format!(
        "<p>{}{}{}</p>",
        "<rp>".repeat(250),
        "word ".repeat(24_000),
        "</rp>".repeat(250)
    ));
    let (d, _) = assert_linear(&s).unwrap();
    assert!(txt::write(&d).0.starts_with("word word"));
    // A fuzz input that broke the bound by copying per level.
    let crash = include_str!("data/nested-rp.xhtml");
    let (r, steps) = read_steps(crash, Limits::DEFAULT);
    assert!(
        steps <= STEPS_PER_BYTE * crash.len() as u64,
        "{steps} steps for {} bytes",
        crash.len()
    );
    assert_eq!(
        txt::write(&r.unwrap().0).0,
        format!("{}\n", "x".repeat(200))
    );
}

#[test]
fn has_text_ignores_alt_text_and_white_space() {
    let doc = |b: &str| read(&page(b)).0;
    assert!(!has_text(&doc("")));
    assert!(!has_text(&doc(
        r#"<div><img src="c.jpg" alt="Cover"/></div>"#
    )));
    assert!(!has_text(&doc("<p>\u{20}<br/></p><hr/>")));
    assert!(!has_text(&doc(
        r#"<svg xmlns="http://www.w3.org/2000/svg"><text>t</text></svg>"#
    )));
    assert!(has_text(&doc("<p>a</p>")));
    assert!(has_text(&doc("<pre>x</pre>")));
    assert!(has_text(&doc("<p><code>x</code></p>")));
    assert!(has_text(&doc(r#"<p><a href="x">y</a></p>"#)));
    assert!(has_text(&doc("<table><tr><td>c</td></tr></table>")));
    assert!(has_text(&doc(
        "<ul><li><blockquote>q</blockquote></li></ul>"
    )));
    assert!(has_text(&Document {
        blocks: vec![Block::Heading {
            level: 1,
            content: vec![Inline::Emph(vec![t("h")])]
        }]
    }));
    assert!(!has_text(&Document {
        blocks: vec![Block::CodeBlock {
            info: String::new(),
            text: " \n".to_string()
        }]
    }));
}

/// A tiny deterministic generator (xorshift64), so the property test needs
/// no dependency and every failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

struct Gen {
    rng: Rng,
    out: String,
    words: Vec<String>,
    /// Words written inside an SVG: the reader drops them and must say so.
    svg_words: Vec<String>,
    /// Words that are text (not an image's alt text).
    text_words: usize,
}

impl Gen {
    fn word(&mut self, in_svg: bool) -> String {
        let w = format!("q{}z", self.words.len() + self.svg_words.len());
        if in_svg {
            self.svg_words.push(w.clone());
        } else {
            self.words.push(w.clone());
            self.text_words += 1;
        }
        w
    }

    fn children(&mut self, depth: u32) {
        for _ in 0..self.rng.below(4) {
            self.node(depth + 1);
        }
    }

    fn node(&mut self, depth: u32) {
        // Words glued by markup (`<b>x</b>y`) are one word to the plain
        // text writer, which may cut a word longer than a table column:
        // a space before every node keeps each word whole.
        self.out.push(' ');
        const WRAPS: [&str; 30] = [
            "p",
            "h2",
            "div",
            "section",
            "span",
            "em",
            "strong",
            "b",
            "i",
            "del",
            "s",
            "code",
            "kbd",
            "blockquote",
            "ul",
            "ol",
            "li",
            "pre",
            "figure",
            "figcaption",
            "aside",
            "foo",
            "dl",
            "dt",
            "dd",
            "sup",
            "small",
            "caption",
            "td",
            "tr",
        ];
        let k = if depth > 6 { 0 } else { self.rng.below(48) };
        match k {
            0..=9 => {
                let w = self.word(false);
                let pad = [" ", "", "\n  ", "\t"][self.rng.below(4) as usize];
                self.out.push_str(&format!("{pad}{w}{pad}"));
            }
            10 => {
                let w = self.word(false);
                self.text_words -= 1;
                self.out
                    .push_str(&format!(r#"<img src="i{w}.png" alt="{w}"/>"#));
            }
            11 => self.out.push_str("<br/>"),
            12 => self.out.push_str("<hr/>"),
            13 => {
                // SVG text is dropped (an image loss).
                let w = self.word(true);
                self.out.push_str(&format!(
                    r#"<svg xmlns="http://www.w3.org/2000/svg"><text>{w}</text></svg>"#
                ));
            }
            14 => {
                let w = self.word(false);
                self.out.push_str(&format!(
                    r#"<math xmlns="http://www.w3.org/1998/Math/MathML"><mi>{w}</mi></math>"#
                ));
            }
            15 => {
                let (b, r) = (self.word(false), self.word(false));
                // An rp that holds more than a bracket keeps its text.
                let p = self.word(false);
                self.out.push_str(&format!(
                    "<ruby>{b}<rp>(</rp><rt>{r}</rt><rp>) {p}</rp></ruby>"
                ));
            }
            16 => {
                self.out.push_str(r#"<a href="http://x/">"#);
                self.children(depth);
                self.out.push_str("</a>");
            }
            17..=19 => self.table(depth),
            _ => {
                let tag = WRAPS[self.rng.below(WRAPS.len() as u64) as usize];
                self.out.push_str(&format!("<{tag}>"));
                self.children(depth);
                self.out.push_str(&format!("</{tag}>"));
            }
        }
    }

    fn table(&mut self, depth: u32) {
        self.out.push_str("<table>");
        let section = self.rng.below(3) == 0;
        if section {
            self.out.push_str("<thead>");
        }
        let rows = 1 + self.rng.below(3);
        for r in 0..rows {
            if r == 1 && section {
                self.out.push_str("</thead><tbody>");
            }
            self.out.push_str("<tr>");
            for _ in 0..1 + self.rng.below(3) {
                let tag = if self.rng.below(2) == 0 { "td" } else { "th" };
                let span = match self.rng.below(4) {
                    0 => r#" colspan="2""#,
                    1 => r#" rowspan="2""#,
                    _ => "",
                };
                self.out.push_str(&format!("<{tag}{span}>"));
                self.children(depth + 1);
                self.out.push_str(&format!("</{tag}>"));
            }
            self.out.push_str("</tr>");
        }
        if section {
            // Close whichever section is open.
            self.out
                .push_str(if rows == 1 { "</thead>" } else { "</tbody>" });
        }
        self.out.push_str("</table>");
    }
}

#[test]
fn no_text_is_lost_silently() {
    for seed in 1..=3000u64 {
        let mut g = Gen {
            rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
            out: String::new(),
            words: Vec::new(),
            svg_words: Vec::new(),
            text_words: 0,
        };
        for _ in 0..1 + g.rng.below(5) {
            g.node(0);
        }
        let s = page(&g.out);
        let (d, losses) = read(&s);
        let out = txt::write(&d).0;
        for w in &g.words {
            assert!(
                out.contains(w.as_str()),
                "seed {seed}: {w} lost\n{s}\n{out}"
            );
        }
        if !g.svg_words.is_empty() {
            assert!(losses.contains(&Loss::Images), "seed {seed}: {s}");
        }
        // Text means text: an alt text alone is none, unless `pre` or
        // inline code flattened it into their text.
        let flat = ["<pre>", "<code>", "<kbd>"].iter().any(|t| s.contains(t));
        assert!(
            has_text(&d) == (g.text_words > 0) || (flat && g.text_words == 0),
            "seed {seed}: {s}"
        );
    }
}

/// The reader's own steps for a document (the XML reader's are not in it).
fn steps(s: &str) -> u64 {
    let (r, steps) = read_steps(s, Limits::DEFAULT);
    r.unwrap_or_else(|e| panic!("{e:?} for {s}"));
    steps
}

#[test]
fn step_counts_per_construct() {
    // The linearity tests bound the steps only from above, so a counter
    // that stops counting would pass them: each count here is worked out
    // from the builder. Per event 1; a block start 1 (closing implicit
    // frames) + 1 (its loop) + 1 per frame pushed; text in a run 2 per
    // byte; a closed frame 1, a paragraph's trim 1 per level walked, an
    // attached block or inline 1 per frame searched.
    //
    // p: start 1+1+1+1; "abc" 1+6; end 1, close 1, trim 1, attach 1.
    assert_eq!(steps("<p>abc</p>"), 15);
    // div in a run: start 1+1+1 (a break); end 1. "bc" after the break:
    // 1, 2 (white space test), trim 1 before the break, 4.
    assert_eq!(steps("<p>a<div></div>bc</p>"), 4 + 3 + 3 + 1 + 8 + 4);
    // code: start 1, ensure_inline 1, frame 1; "abc" flat 1+6; end 1,
    // close 1, attach 1; p's end 1, close 1, trim 1 (code ends it), attach 1.
    assert_eq!(steps("<p><code>abc</code></p>"), 4 + 3 + 7 + 3 + 4);
    // img: start 1, ensure_inline 1, attach 1; end 1; " " 1+2; p's trim
    // pops the emptied space and goes on to the image: 2.
    assert_eq!(steps(r#"<p>a<img src="i"/> </p>"#), 4 + 3 + 3 + 1 + 3 + 5);
    // A table: start 4; tr 4; a td 4 + its text 3 + its end 3 = 10.
    // Row 1 (a, b, c over two rows): per cell a column added 1 and its
    // span set 1, then 3 columns counted down: 9; its end 1 + close 1.
    // Row 2 (d): d's span 1, column 1 free (an empty cell 1), column 2
    // covered (a merged cell 1), 3 counted down: 6; end 1 + close 1.
    // Table end 1 + close 1, the empty header padded to 3 columns 3,
    // attach 1.
    assert_eq!(
        steps(
            r#"<table><tr><td>a</td><td>b</td><td rowspan="2">c</td></tr><tr><td>d</td></tr></table>"#
        ),
        4 + (4 + 30 + 2 + 9) + (4 + 10 + 2 + 6) + (2 + 3 + 1)
    );
}

#[test]
fn error_display_is_the_cause() {
    let limit = LimitError::Exceeded("nodes");
    assert_eq!(XhtmlError::Limit(limit).to_string(), limit.to_string());
    let e = read_limited("<p>", Limits::DEFAULT).unwrap_err();
    let XhtmlError::Xml(x) = &e else {
        panic!("an unclosed element is not well-formed: {e:?}");
    };
    let shown = e.to_string();
    assert!(!shown.is_empty());
    assert_eq!(shown, x.to_string());
}

#[test]
fn svg_lower_headings_and_quiet_table_elements() {
    // SVG in the XHTML namespace (written without its own) is dropped
    // with its text, an image loss.
    let (d, losses) = read(&page("<svg><desc>x</desc></svg>"));
    assert_eq!(d.blocks, vec![]);
    assert!(losses.contains(&Loss::Images), "{losses:?}");
    let heading = |level, s: &str| Block::Heading {
        level,
        content: vec![t(s)],
    };
    assert_eq!(
        blocks("<h4>a</h4><h5>b</h5><h6>c</h6>"),
        vec![heading(4, "a"), heading(5, "b"), heading(6, "c")]
    );
    // A row group ends the row spans above it; `col` between rows holds
    // nothing (no row of its own that a span would cover).
    let table = |rows: Vec<Vec<Vec<Inline>>>| Block::Table {
        align: vec![Align::None; rows[0].len()],
        head: vec![vec![]; rows[0].len()],
        rows,
    };
    assert_eq!(
        blocks(
            r#"<table><tr><td rowspan="2">a</td></tr><tbody><tr><td>b</td></tr></tbody></table>"#
        ),
        vec![table(vec![vec![vec![t("a")]], vec![vec![t("b")]]])]
    );
    assert_eq!(
        blocks(r#"<table><tr><td rowspan="2">a</td></tr><col/><tr><td>b</td></tr></table>"#),
        vec![table(vec![
            vec![vec![t("a")], vec![]],
            vec![vec![], vec![t("b")]]
        ])]
    );
}

#[test]
fn link_in_pre_is_a_links_loss_only_with_href() {
    let (d, losses) = read(&page(r#"<pre><a href="x">t</a></pre>"#));
    assert_eq!(
        d.blocks,
        vec![Block::CodeBlock {
            info: String::new(),
            text: "t\n".into()
        }]
    );
    assert_eq!(losses, BTreeSet::from([Loss::Links, Loss::Styles]));
    let (_, losses) = read(&page(r#"<pre><a id="x">t</a></pre>"#));
    assert_eq!(losses, BTreeSet::from([Loss::Styles]));
}

#[test]
fn block_in_pre_after_a_space() {
    let code = |s: &str| {
        vec![Block::CodeBlock {
            info: String::new(),
            text: s.into(),
        }]
    };
    // A trailing space is not a line end: the block still starts a line.
    assert_eq!(blocks("<pre>a <div>b</div></pre>"), code("a \nb\n"));
    // A cell after a space adds no second space.
    assert_eq!(blocks("<pre>a <td>b</td></pre>"), code("a b\n"));
}

#[test]
fn empty_implicit_cell_is_dropped() {
    // Inline markup in a row opens a cell for its content; holding
    // nothing, it is no cell.
    assert_eq!(
        blocks("<table><tr><span/><td>a</td></tr></table>"),
        vec![Block::Table {
            align: vec![Align::None],
            head: vec![vec![]],
            rows: vec![vec![vec![t("a")]]],
        }]
    );
}

#[test]
fn inline_code_text_is_charged() {
    let s = page("<p><code>abcd</code></p>");
    let limits = |text_bytes| Limits {
        text_bytes,
        ..Limits::DEFAULT
    };
    assert!(read_limited(&s, limits(4)).is_ok());
    assert_eq!(
        read_limited(&s, limits(3)),
        Err(XhtmlError::Limit(LimitError::Exceeded("owned text")))
    );
}

#[test]
fn row_span_at_the_end_of_a_row_and_empty_rows() {
    // The covered column after the row's last cell is a merged cell.
    let (d, losses) = read(&page(
        r#"<table><tr><td>a</td><td rowspan="2">b</td></tr><tr><td>c</td></tr></table>"#,
    ));
    assert_eq!(
        d.blocks,
        vec![Block::Table {
            align: vec![Align::None; 2],
            head: vec![vec![], vec![]],
            rows: vec![vec![vec![t("a")], vec![t("b")]], vec![vec![t("c")], vec![]]],
        }]
    );
    assert_eq!(losses, BTreeSet::from([Loss::MergedCells, Loss::Styles]));
    // A row with no cells, nothing spanning into it, is no row.
    assert_eq!(
        blocks("<table><tr><td>a</td><td>b</td></tr><tr></tr></table>"),
        vec![Block::Table {
            align: vec![Align::None; 2],
            head: vec![vec![], vec![]],
            rows: vec![vec![vec![t("a")], vec![t("b")]]],
        }]
    );
}

#[test]
fn trailing_trim_edges() {
    // A second break: the first is not trimmed away before it.
    assert_eq!(
        blocks("<p>a<br/><br/>b</p>"),
        vec![para(vec![
            t("a"),
            Inline::HardBreak,
            Inline::HardBreak,
            t("b")
        ])]
    );
    // Emphasis emptied by the trim before a break goes at the end.
    assert_eq!(blocks("<p>x<b> </b><br/></p>"), vec![para(vec![t("x")])]);
    // The trailing space inside emphasis goes.
    assert_eq!(
        blocks("<p><em>a </em></p>"),
        vec![para(vec![Inline::Emph(vec![t("a")])])]
    );
}

fn read_ids(body: &str) -> (Vec<Block>, Ids) {
    let s = page(body);
    let (d, _, ids) = read_with_ids(&s, Limits::DEFAULT).unwrap_or_else(|e| panic!("{e:?}"));
    // The ids never change the tree: it is the one `read_limited` builds.
    assert_eq!(d, read_limited(&s, Limits::DEFAULT).unwrap().0);
    (d.blocks, ids)
}

/// The text of every heading in document order (pre-order: a block, then
/// the blocks inside it), the order the heading ordinals count in.
fn heading_texts(blocks: &[Block]) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack: Vec<&Block> = blocks.iter().rev().collect();
    while let Some(b) = stack.pop() {
        match b {
            Block::Heading { content, .. } => out.push(match content.as_slice() {
                [Inline::Text(t)] => t.clone(),
                other => format!("{other:?}"),
            }),
            Block::BlockQuote(inner) => stack.extend(inner.iter().rev()),
            Block::List { items, .. } => {
                stack.extend(items.iter().rev().flat_map(|i| i.blocks.iter().rev()));
            }
            _ => {}
        }
    }
    out
}

/// The heading text each id lands on.
fn heading_of(blocks: &[Block], ids: &Ids) -> Vec<(String, String)> {
    let texts = heading_texts(blocks);
    ids.headings
        .iter()
        .map(|(n, id)| (id.clone(), texts[*n].clone()))
        .collect()
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

#[test]
fn heading_ids_kept_in_order() {
    // Ordinals count the headings in document order, nested ones (a quote,
    // a list item) included; a heading without an id is counted too.
    let (b, ids) = read_ids(concat!(
        r#"<h1 id="top">Top</h1><p>x</p><h2>No id</h2>"#,
        r#"<blockquote><h3 id="q">In quote</h3></blockquote>"#,
        r#"<ul><li><h4 id="l">In item</h4></li></ul><h2 id="end">End</h2>"#,
    ));
    assert_eq!(
        ids.headings,
        vec![
            (0, "top".to_string()),
            (2, "q".to_string()),
            (3, "l".to_string()),
            (4, "end".to_string())
        ]
    );
    assert_eq!(
        heading_of(&b, &ids),
        pairs(&[
            ("top", "Top"),
            ("q", "In quote"),
            ("l", "In item"),
            ("end", "End")
        ])
    );
    assert!(ids.others.is_empty());

    // A container whose first block is a heading: its id lands on that
    // heading too, outer ids first, through transparent wrappers and
    // white space; a list item's id likewise.
    let (b, ids) = read_ids(concat!(
        r#"<section id="c1"> <div id="w"><h2 id="t">Title</h2></div><p>p</p></section>"#,
        r#"<ol><li id="i"><h3>Item</h3></li></ol>"#,
        r#"<blockquote id="bq"><h3>Quoted</h3></blockquote>"#,
        r#"<body id="never"/>"#,
    ));
    assert_eq!(
        heading_of(&b, &ids),
        pairs(&[
            ("c1", "Title"),
            ("w", "Title"),
            ("t", "Title"),
            ("i", "Item"),
            ("bq", "Quoted")
        ])
    );
    assert_eq!(ids.others, vec!["never".to_string()]);

    // Something before the heading: the container's id is not the
    // heading's (a link to it lands on the paragraph, not a title).
    for body in [
        r#"<section id="s"><p>first</p><h2>H</h2></section>"#,
        r#"<section id="s"><hr/><h2>H</h2></section>"#,
        r#"<section id="s">text<h2>H</h2></section>"#,
        r#"<section id="s"><blockquote><h2>H</h2></blockquote></section>"#,
        r#"<section id="s"><table><tr><td>c</td></tr></table><h2>H</h2></section>"#,
        r#"<section id="s"></section><h2>H</h2>"#,
        r#"<section id="s"><p><h2>H</h2></p></section>"#,
    ] {
        let (_, ids) = read_ids(body);
        assert!(ids.headings.is_empty(), "{body}: {ids:?}");
        assert_eq!(ids.others, vec!["s".to_string()], "{body}");
    }
    // An empty element before the heading leaves nothing: the id is still
    // the heading's.
    let (b, ids) = read_ids(r#"<section id="s"><span/><p></p><h2>H</h2></section>"#);
    assert_eq!(heading_of(&b, &ids), pairs(&[("s", "H")]));

    // An empty heading is no heading: its ids are others, and it takes no
    // ordinal.
    let (b, ids) = read_ids(r#"<div id="d"><h2 id="e"> </h2></div><h2 id="f">F</h2>"#);
    assert_eq!(heading_of(&b, &ids), pairs(&[("f", "F")]));
    assert_eq!(ids.headings, vec![(0, "f".to_string())]);
    assert_eq!(ids.others, vec!["d".to_string(), "e".to_string()]);
}

#[test]
fn ids_on_non_heading_elements_listed() {
    // Footnote anchors, paragraphs, spans, a heading in a cell (inline
    // there), elements in `pre` and in code: all others, in document order.
    // An empty id is no id; an element dropped with its content keeps none.
    let (_, ids) = read_ids(concat!(
        r#"<p id="p1">a<a id="n1"/><span id="s1">b</span><a id="r1" href="x.xhtml#n1">c</a></p>"#,
        r#"<table id="t"><tr id="tr"><td id="td"><h2 id="h-in-cell">x</h2></td></tr></table>"#,
        r#"<pre id="pre">a<span id="in-pre">b</span></pre><p><code id="c">x</code><img id="img" src="i.png"/></p>"#,
        r#"<p id="">empty</p><svg xmlns="http://www.w3.org/2000/svg" id="svg"><g id="g"/></svg>"#,
        r#"<script id="sc"/><ul id="u"><li>i</li></ul><hr id="hr"/>"#,
    ));
    assert!(ids.headings.is_empty());
    assert_eq!(
        ids.others,
        [
            "p1",
            "n1",
            "s1",
            "r1",
            "t",
            "tr",
            "td",
            "h-in-cell",
            "pre",
            "in-pre",
            "c",
            "img",
            "u",
            "hr"
        ]
        .map(String::from)
        .to_vec()
    );
    // The same id twice is listed twice: the reader keeps what the book says.
    let (_, ids) = read_ids(r#"<p id="x">a</p><p id="x">b</p>"#);
    assert_eq!(ids.others, vec!["x".to_string(), "x".to_string()]);
}

#[test]
fn id_text_charged_to_the_budget() {
    let s = page(&format!(r#"<p id="{}">abc</p>"#, "i".repeat(100)));
    let with = |n| {
        read_with_ids(
            &s,
            Limits {
                text_bytes: n,
                ..Limits::DEFAULT
            },
        )
        .map(|_| ())
    };
    // The text (3) and the id (100).
    assert_eq!(with(103), Ok(()));
    assert_eq!(
        with(102),
        Err(XhtmlError::Limit(LimitError::Exceeded("owned text")))
    );
    // Without ids the chapter costs what it always did.
    assert!(
        read_limited(
            &s,
            Limits {
                text_bytes: 3,
                ..Limits::DEFAULT
            }
        )
        .is_ok()
    );
    // A heading's id and a container's id waiting for a heading are
    // charged too: 1 + 2 + 3.
    let s = page(r#"<div id="ab"><h1 id="cde">x</h1></div>"#);
    let ok = |n| {
        read_with_ids(
            &s,
            Limits {
                text_bytes: n,
                ..Limits::DEFAULT
            },
        )
        .is_ok()
    };
    assert!(ok(6));
    assert!(!ok(5));
}

#[test]
fn ids_are_linear() {
    // Deeply nested containers with ids, each waiting for a heading that
    // comes only at the end, then many headings with ids: every id is
    // resolved once.
    let depth = 200;
    let mut body = String::new();
    for i in 0..depth {
        body.push_str(&format!(r#"<div id="d{i}">"#));
    }
    body.push_str(r#"<h1 id="h">x</h1>"#);
    for _ in 0..depth {
        body.push_str("</div>");
    }
    let mut n = 0;
    while body.len() < 2 << 20 {
        body.push_str(&format!(
            r#"<section id="s{n}"><h2 id="h{n}">t</h2><p id="p{n}">p</p></section>"#
        ));
        n += 1;
    }
    let s = page(&body);
    let (r, steps) = read_with_ids_steps(&s, Limits::DEFAULT);
    assert!(
        steps <= STEPS_PER_BYTE * s.len() as u64,
        "{steps} steps for {} bytes",
        s.len()
    );
    let (_, _, ids) = r.unwrap();
    assert_eq!(ids.headings.len(), depth + 1 + 2 * n);
    assert_eq!(ids.others.len(), n);
    // Without ids the steps are the reader's own, unchanged.
    assert!(read_steps(&s, Limits::DEFAULT).1 <= steps);
}

#[test]
fn inline_between_table_rows_opens_no_row() {
    // An empty inline element between rows (an anchor) opened an implicit
    // row that took the row span of the cell above: `d` landed in the
    // spanned column.
    let (d, _) = read(&page(
        r#"<table><tr><td rowspan="2">a</td><td>b</td></tr><a id="x"/><tr><td>d</td></tr></table>"#,
    ));
    assert_eq!(
        d.blocks,
        vec![Block::Table {
            align: vec![Align::None; 2],
            head: vec![vec![], vec![]],
            rows: vec![vec![vec![t("a")], vec![t("b")]], vec![vec![], vec![t("d")]]],
        }]
    );
    // White space, an empty span or emphasis there neither.
    assert_eq!(
        blocks(
            r#"<table><tr><td rowspan="2">a</td></tr> <span> </span><b/><br/><tr><td>d</td></tr></table>"#
        ),
        vec![Block::Table {
            align: vec![Align::None; 2],
            head: vec![vec![], vec![]],
            rows: vec![vec![vec![t("a")], vec![]], vec![vec![], vec![t("d")]]],
        }]
    );
    // Text between rows is still a row of its own: no text is lost.
    assert_eq!(
        blocks(r#"<table><tr><td>a</td></tr><span>x</span><tr><td>d</td></tr></table>"#),
        vec![Block::Table {
            align: vec![Align::None],
            head: vec![vec![]],
            rows: vec![vec![vec![t("a")]], vec![vec![t("x")]], vec![vec![t("d")]]],
        }]
    );
}
