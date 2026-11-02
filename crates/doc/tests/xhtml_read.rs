//! The XHTML chapter reader (`doc::xhtml_read`): the element subset it maps
//! to the document tree, what it drops (and reports), its budgets and its
//! linearity on hostile shapes.

use std::collections::BTreeSet;

use doc::xhtml_read::{XhtmlError, has_text, read_limited, read_steps};
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
                self.out
                    .push_str(&format!("<ruby>{b}<rp>(</rp><rt>{r}</rt><rp>)</rp></ruby>"));
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
