use super::*;

fn opts() -> Options {
    Options::default()
}

fn html() -> Options {
    Options {
        html_entities: true,
        ..Options::default()
    }
}

/// Every event of `s`, or the first error.
fn events(s: &str, o: Options) -> Result<Vec<Event<'_>>, Error> {
    let mut r = Reader::new(s, o);
    let mut v = Vec::new();
    while let Some(e) = r.next()? {
        v.push(e);
    }
    Ok(v)
}

fn err(s: &str, o: Options) -> Error {
    events(s, o).expect_err("expected an error")
}

fn not_wf(s: &str) -> u32 {
    match err(s, opts()) {
        Error {
            kind: ErrorKind::NotWellFormed(_),
            line,
        } => line,
        e => panic!("expected not well-formed, got {e:?}"),
    }
}

fn q<'a>(ns: Option<&'a str>, local: &'a str) -> QName<'a> {
    QName { ns, local }
}

/// The text of all `Text` events, concatenated.
fn text(s: &str, o: Options) -> String {
    events(s, o)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e {
            Event::Text(t) => Some(t.into_owned()),
            _ => None,
        })
        .collect()
}

/// Steps to read `s` to the end (errors included).
fn steps(s: &str, o: Options) -> u64 {
    let mut r = Reader::new(s, o);
    while let Ok(Some(_)) = r.next() {}
    r.steps()
}

#[test]
fn elements_text_and_attributes() {
    let s = "<?xml version='1.0'?>\n<!-- c --><a x=\"1\" y='two'>hi<b/>there<?pi data?></a>\n";
    let ev = events(s, opts()).unwrap();
    let Event::Start(a) = &ev[0] else {
        panic!("{ev:?}")
    };
    assert_eq!(a.name, q(None, "a"));
    assert_eq!(a.attr(None, "x"), Some("1"));
    assert_eq!(a.attr(None, "y"), Some("two"));
    assert_eq!(a.attr(None, "z"), None);
    let names: Vec<_> = a.attrs().map(|(n, v)| (n.local, v.to_string())).collect();
    assert_eq!(names, [("x", "1".into()), ("y", "two".into())]);
    assert_eq!(ev[1], Event::Text(Cow::Borrowed("hi")));
    assert!(matches!(&ev[2], Event::Start(b) if b.name == q(None, "b")));
    assert_eq!(ev[3], Event::End(q(None, "b")));
    // The PI between text and the end tag is skipped.
    assert_eq!(ev[4], Event::Text(Cow::Borrowed("there")));
    assert_eq!(ev[5], Event::End(q(None, "a")));
    assert_eq!(ev.len(), 6);
    // CDATA is text, merged with its neighbours; markup inside is literal.
    assert_eq!(text("<a>x<![CDATA[<b>&amp;]]>y</a>", opts()), "x<b>&amp;y");
    // Line ends normalize to \n in text, CDATA and attributes; whitespace
    // in attribute values becomes one space each.
    assert_eq!(
        text("<a>1\r\n2\r3<![CDATA[\r\n]]></a>", opts()),
        "1\n2\n3\n"
    );
    let ev = events("<a v=\"a\tb\r\nc\nd&#10;\"/>", opts()).unwrap();
    let Event::Start(a) = &ev[0] else { panic!() };
    assert_eq!(a.attr(None, "v"), Some("a b c d\n"));
    // An unchanged value borrows from the input.
    let ev = events("<a v='plain'>t</a>", opts()).unwrap();
    let Event::Start(a) = &ev[0] else { panic!() };
    assert!(matches!(
        a.attrs().next(),
        Some((_, Cow::Borrowed("plain")))
    ));
}

#[test]
fn syntax_errors_are_not_well_formed() {
    for s in [
        "",
        "   ",
        "text",
        "<a>",
        "<a></b>",
        "<a x=1/>",
        "<a x='1'y='2'/>",
        "<a x='<'/>",
        "<a>]]></a>",
        "<a><!-- -- --></a>",
        "<a><!-- --->",
        "<a>&</a>",
        "<a>&amp</a>",
        "<a>&#;</a>",
        "<a>&#x;</a>",
        "<1a/>",
        "<a/><?xml version='1.0'?>",
        " <?xml version='1.0'?><a/>",
        "<?xml?><a/>",
        "<?xml version='2.0'?><a/>",
        "<?xml version='1.0' standalone='maybe'?><a/>",
        "<?xml encoding='UTF-8'?><a/>",
        "<a><?xml x?></a>",
        "<a><?pi?x?></a>",
        "<a><!DOCTYPE a></a>",
        "<!DOCTYPE a><!DOCTYPE a><a/>",
        "<a/><!DOCTYPE a>",
        "<a>\u{1}</a>",
        "<a>\u{FFFE}</a>",
        "<a><![CDATA[x</a>",
        "<a x='1' x='2'/>",
        "<a:b:c/>",
        "<:a/>",
        "<a:/>",
    ] {
        let e = events(s, opts());
        assert!(
            matches!(
                e,
                Err(Error {
                    kind: ErrorKind::NotWellFormed(_),
                    ..
                })
            ),
            "{s:?}: {e:?}"
        );
    }
}

#[test]
fn errors_are_sticky() {
    let mut r = Reader::new("<a></b><c/>", opts());
    assert!(matches!(r.next(), Ok(Some(Event::Start(_)))));
    let e = r.next().unwrap_err();
    assert_eq!(r.next().unwrap_err(), e);
    assert_eq!(r.next().unwrap_err(), e);
}

#[test]
fn namespaces_resolve_prefixes_and_default() {
    let s = r#"<r xmlns="urn:d" xmlns:p="urn:p" p:a="1" b="2" xml:lang="hu">
        <p:c xmlns:p="urn:inner"><d/></p:c><p:e/><f xmlns=""/></r>"#;
    let ev = events(s, opts()).unwrap();
    let starts: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            Event::Start(el) => Some(el.clone()),
            _ => None,
        })
        .collect();
    let r = &starts[0];
    assert_eq!(r.name, q(Some("urn:d"), "r"));
    assert_eq!(r.attr(Some("urn:p"), "a"), Some("1"));
    // Unprefixed attributes have no namespace, whatever the default is.
    assert_eq!(r.attr(None, "b"), Some("2"));
    assert_eq!(r.attr(Some("urn:d"), "b"), None);
    assert_eq!(r.attr(Some(XML_NS), "lang"), Some("hu"));
    // Declarations are applied, not listed.
    assert_eq!(r.attrs().count(), 3);
    assert_eq!(starts[1].name, q(Some("urn:inner"), "c"));
    assert_eq!(starts[2].name, q(Some("urn:d"), "d"));
    // The inner binding ended with its element.
    assert_eq!(starts[3].name, q(Some("urn:p"), "e"));
    // xmlns="" undeclares the default.
    assert_eq!(starts[4].name, q(None, "f"));
    assert!(ev.contains(&Event::End(q(Some("urn:inner"), "c"))));
    // The xml prefix may be declared, to its own namespace only.
    assert!(events(&format!("<a xmlns:xml='{XML_NS}'/>"), opts()).is_ok());
    for s in [
        "<a xmlns:xml='urn:x'/>".to_string(),
        format!("<a xmlns:p='{XML_NS}'/>"),
        format!("<a xmlns='{XML_NS}'/>"),
        format!("<a xmlns:p='{XMLNS_NS}'/>"),
        format!("<a xmlns='{XMLNS_NS}'/>"),
        "<a xmlns:xmlns='urn:x'/>".to_string(),
        "<a xmlns:p=''/>".to_string(),
        "<a xmlns:='urn:x'/>".to_string(),
        "<xmlns:a/>".to_string(),
        "<a xmlns:p='urn:a&amp;b'/>".to_string(),
        "<a xmlns:p='urn:a\nb'/>".to_string(),
    ] {
        assert!(
            matches!(
                events(&s, opts()),
                Err(Error {
                    kind: ErrorKind::NotWellFormed(_),
                    ..
                })
            ),
            "{s}"
        );
    }
}

#[test]
fn unbound_prefix_is_not_well_formed() {
    assert_eq!(not_wf("<p:a/>"), 1);
    assert_eq!(not_wf("<a\n p:x='1'/>"), 2);
    // A binding ends with its element.
    assert_eq!(not_wf("<a><b xmlns:p='urn:p'/>\n<p:c/></a>"), 2);
}

#[test]
fn duplicate_attribute_is_not_well_formed() {
    not_wf("<a x='1' x='2'/>");
    not_wf("<a xmlns:p='u' xmlns:p='u'/>");
    not_wf("<a xmlns='u' xmlns='u'/>");
    // Two prefixes, one namespace: the same expanded name.
    not_wf("<a xmlns:a='urn:u' xmlns:b='urn:u' a:x='1' b:x='2'/>");
    // Different namespaces: distinct.
    assert!(
        events(
            "<a xmlns:a='urn:1' xmlns:b='urn:2' a:x='1' b:x='2' x='3'/>",
            opts()
        )
        .is_ok()
    );
    // The same above the pairwise threshold (the hash-set path).
    let many: String = (0..20).map(|i| format!(" v{i}='{i}'")).collect();
    assert!(events(&format!("<a{many}/>"), opts()).is_ok());
    not_wf(&format!("<a{many} v7='x'/>"));
    not_wf(&format!(
        "<a xmlns:a='urn:u' xmlns:b='urn:u'{many} a:x='1' b:x='2'/>"
    ));
}

#[test]
fn mismatched_end_tag_reports_line() {
    let e = err("<a>\n<b>\n</c>\n</a>", opts());
    assert!(matches!(e.kind, ErrorKind::NotWellFormed(_)));
    assert_eq!(e.line, 3);
    // \r\n and a lone \r are line ends too.
    assert_eq!(not_wf("<a>\r\n\r<b></c></a>"), 3);
    assert!(e.to_string().starts_with("not well-formed XML (line 3)"));
    // The reader reports the line of the last event.
    let mut r = Reader::new("<a>\n\n<b/></a>", opts());
    r.next().unwrap();
    assert_eq!(r.line(), 1);
    r.next().unwrap();
    r.next().unwrap();
    assert_eq!(r.line(), 3);
}

#[test]
fn content_after_root_is_rejected() {
    assert!(events("<a/> <!-- ok --> <?pi ok?>\n", opts()).is_ok());
    assert_eq!(not_wf("<a/>\n<b/>"), 2);
    not_wf("<a/>text");
    not_wf("<a/>&amp;");
    not_wf("<a/></a>");
    not_wf("text<a/>");
}

#[test]
fn predefined_and_numeric_entities() {
    assert_eq!(
        text(
            "<a>&lt;&gt;&amp;&apos;&quot;&#65;&#x42;&#x0043;&#0000068;&#x1F600;</a>",
            opts()
        ),
        "<>&'\"ABCD\u{1F600}"
    );
    // A character reference to \r is kept, not normalized.
    assert_eq!(text("<a>&#13;</a>", opts()), "\r");
    let ev = events("<a v='&lt;&#9;x'/>", opts()).unwrap();
    let Event::Start(a) = &ev[0] else { panic!() };
    assert_eq!(a.attr(None, "v"), Some("<\tx"));
    // HTML names are undefined without HTML mode.
    not_wf("<a>&eacute;</a>");
}

#[test]
fn numeric_reference_to_forbidden_char_is_rejected() {
    for s in [
        "<a>&#0;</a>",
        "<a>&#xD800;</a>",
        "<a>&#xDFFF;</a>",
        "<a>&#1;</a>",
        "<a>&#xFFFE;</a>",
        "<a>&#xFFFF;</a>",
        "<a>&#x110000;</a>",
        "<a>&#99999999999999999999;</a>",
        "<a v='&#0;'/>",
    ] {
        let e = err(s, opts());
        assert!(matches!(e.kind, ErrorKind::NotWellFormed(_)), "{s}: {e:?}");
    }
    assert_eq!(
        text("<a>&#x10FFFF;&#xFFFD;&#xE000;</a>", opts()),
        "\u{10FFFF}\u{FFFD}\u{E000}"
    );
}

#[test]
fn html_entities_only_in_html_mode() {
    assert_eq!(text("<a>&nbsp;&eacute;&amp;</a>", html()), "\u{A0}é&");
    assert_eq!(entity("nbsp"), Some("\u{A0}"));
    assert_eq!(entity("nosuchname"), None);
    let e = err("<a>&nbsp;</a>", opts());
    assert!(matches!(e.kind, ErrorKind::NotWellFormed(_)));
    // An unknown name is undefined in HTML mode too.
    let e = err("<a>&nosuchname;</a>", html());
    assert!(matches!(e.kind, ErrorKind::NotWellFormed(_)));
    // In attributes too; whitespace a name stands for becomes a space.
    let ev = events("<a v='&eacute;&Tab;&NewLine;'/>", html()).unwrap();
    let Event::Start(a) = &ev[0] else { panic!() };
    assert_eq!(a.attr(None, "v"), Some("é  "));
}

#[test]
fn entity_table_is_sorted_and_semicolon_only() {
    let t = entities::ENTITIES;
    assert!(t.len() > 2000);
    assert!(t.windows(2).all(|w| w[0].0 < w[1].0));
    // 32: the longest name the doc crate's Markdown reader scans for
    // (its MAX_ENTITY_NAME); the longest WHATWG name has 31 characters.
    assert!(t.iter().all(|(n, v)| {
        n.bytes().all(|c| c.is_ascii_alphanumeric()) && n.len() <= 32 && !v.is_empty()
    }));
    assert_eq!(ENTITY_LOOKUP_STEPS, 12);
    for (n, v) in t {
        assert_eq!(entity(n), Some(*v));
    }
}

#[test]
fn internal_subset_is_skipped_lexically() {
    let s = r#"<!DOCTYPE a [
        <!ELEMENT a (#PCDATA)>
        <!ATTLIST a v CDATA "]>">
        <!-- ]> in a comment -->
        <?pi ]> in a PI?>
        <!NOTATION n SYSTEM ']>'>
        <!ENTITY % pe "]>">
    ]>
    <a>ok</a>"#;
    assert_eq!(text(s, opts()), "ok");
    // A parameter entity reference is skipped.
    assert_eq!(text("<!DOCTYPE a [ %pe; ]><a>ok</a>", opts()), "ok");
    for s in [
        "<!DOCTYPE a [ <!ELEMENT a (#PCDATA)> <a/>",
        "<!DOCTYPE a [ <!ATTLIST a v CDATA \"]>\" ]><a/>",
        "<!DOCTYPE a [ <!-- ]> --><a/>",
        "<!DOCTYPE a [ <!BOGUS a> ]><a/>",
        "<!DOCTYPE a [ <![INCLUDE[ ]]> ]><a/>",
        "<!DOCTYPE a [ <!ELEMENT a <!-- x --> > ]><a/>",
        "<!DOCTYPE a [ text ]><a/>",
    ] {
        let e = err(s, opts());
        assert!(matches!(e.kind, ErrorKind::NotWellFormed(_)), "{s}: {e:?}");
    }
}

#[test]
fn declared_entity_reference_is_rejected() {
    let s = "<!DOCTYPE a [ <!ENTITY e \"x\"> ]>\n<a>&e;</a>";
    let e = err(s, opts());
    assert_eq!(e.kind, ErrorKind::EntityDeclarations);
    assert_eq!(e.line, 2);
    assert_eq!(e.to_string(), "entity declarations are not supported");
    // Also in HTML mode, also when the name is an HTML one: the
    // declaration says something else and is not honoured.
    let s = "<!DOCTYPE a [ <!ENTITY nbsp \"x\"> ]><a v='&nbsp;'/>";
    assert_eq!(err(s, html()).kind, ErrorKind::EntityDeclarations);
    // A declared but unreferenced entity is harmless.
    assert!(events("<!DOCTYPE a [ <!ENTITY e \"x\"> ]><a/>", opts()).is_ok());
    // An undeclared name stays not well-formed when every declaration
    // was seen.
    let e = err("<!DOCTYPE a [ <!ENTITY e \"x\"> ]><a>&f;</a>", opts());
    assert!(matches!(e.kind, ErrorKind::NotWellFormed(_)));
    // A name that an external subset or a parameter entity may declare.
    for s in [
        "<!DOCTYPE a SYSTEM 'a.dtd'><a>&f;</a>",
        "<!DOCTYPE a [ %pe; ]><a>&f;</a>",
    ] {
        assert_eq!(err(s, opts()).kind, ErrorKind::EntityDeclarations, "{s}");
    }
    // ... unless the document says it is standalone.
    let e = err(
        "<?xml version='1.0' standalone='yes'?><!DOCTYPE a SYSTEM 'a.dtd'><a>&f;</a>",
        opts(),
    );
    assert!(matches!(e.kind, ErrorKind::NotWellFormed(_)));

    // The billion laughs: refused at the first reference, nothing
    // expanded, the work proportional to the input.
    let mut s = String::from("<!DOCTYPE lolz [\n<!ENTITY lol \"lol\">\n");
    for i in 1..10 {
        let prev = if i == 1 {
            "lol".to_string()
        } else {
            format!("lol{}", i - 1)
        };
        s.push_str(&format!(
            "<!ENTITY lol{i} \"{}\">\n",
            format!("&{prev};").repeat(10)
        ));
    }
    s.push_str("]>\n<lolz>&lol9;</lolz>");
    let mut r = Reader::new(&s, opts());
    assert!(matches!(r.next(), Ok(Some(Event::Start(_)))));
    assert_eq!(r.next().unwrap_err().kind, ErrorKind::EntityDeclarations);
    assert!(
        r.steps() <= STEPS_PER_BYTE * s.len() as u64,
        "{}",
        r.steps()
    );
}

#[test]
fn external_doctype_is_never_loaded() {
    // The reader has no I/O at all: it gets text and returns events. A
    // system identifier naming a local file or a URL is only parsed.
    for s in [
        "<!DOCTYPE a SYSTEM \"file:///etc/passwd\"><a>x</a>",
        "<!DOCTYPE a PUBLIC \"-//W3C//DTD XHTML 1.1//EN\" \"http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd\"><a>x</a>",
        "<!DOCTYPE a SYSTEM 'http://127.0.0.1:1/x.dtd' [ <!ELEMENT a ANY> ]><a>x</a>",
    ] {
        assert_eq!(text(s, opts()), "x", "{s}");
    }
    // An EPUB 2 chapter: XHTML 1.1 DOCTYPE and HTML names in the text.
    let s = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.1//EN\" \"http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd\">\n\
             <html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>a&nbsp;caf&eacute;</p></body></html>";
    assert_eq!(text(s, html()), "a\u{A0}café");
    for s in [
        "<!DOCTYPE a SYSTEM><a/>",
        "<!DOCTYPE a SYSTEM'x'><a/>",
        "<!DOCTYPE a PUBLIC 'x'><a/>",
        "<!DOCTYPE a PUBLIC 'x{' 'y'><a/>",
        "<!DOCTYPE a PUBLIC 'x''y'><a/>",
        "<!DOCTYPE a SYSTEM 'x'",
    ] {
        not_wf(s);
    }
}

#[test]
fn depth_257_is_too_deep() {
    let nest = |n: usize, inner: &str| format!("{}{inner}{}", "<a>".repeat(n), "</a>".repeat(n));
    assert!(events(&nest(256, ""), opts()).is_ok());
    assert!(events(&nest(255, "<b/>"), opts()).is_ok());
    let e = err(&nest(257, ""), opts());
    assert_eq!(e.kind, ErrorKind::TooDeep);
    assert_eq!(err(&nest(256, "<b/>"), opts()).kind, ErrorKind::TooDeep);
    let e = err(&format!("{}\n<a>", "<a>".repeat(256)), opts());
    assert_eq!((e.kind, e.line), (ErrorKind::TooDeep, 2));
    // The limit is the option's.
    let o = Options {
        max_depth: 2,
        ..opts()
    };
    assert!(events(&nest(2, ""), o).is_ok());
    assert_eq!(err(&nest(3, ""), o).kind, ErrorKind::TooDeep);
}

#[test]
fn utf16_le_and_be_with_bom() {
    let doc = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><a>é\u{1F600}</a>";
    let le: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain(doc.encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    let be: Vec<u8> = [0xFE, 0xFF]
        .into_iter()
        .chain(doc.encode_utf16().flat_map(u16::to_be_bytes))
        .collect();
    for b in [&le, &be] {
        let t = decode(b).unwrap();
        assert!(matches!(t, Cow::Owned(_)));
        assert_eq!(t, doc);
        assert_eq!(text(&t, opts()), "é\u{1F600}");
    }
    // UTF-8, with and without a byte order mark, borrows.
    let t = decode(b"\xEF\xBB\xBF<a>x</a>").unwrap();
    assert!(matches!(t, Cow::Borrowed("<a>x</a>")));
    assert!(matches!(decode(b"<a/>").unwrap(), Cow::Borrowed(_)));
    // The reader skips a byte order mark it is given.
    assert_eq!(text("\u{FEFF}<a>x</a>", opts()), "x");
    // An unpaired surrogate, an odd length, no byte order mark, a
    // declaration that contradicts the bytes.
    let mut bad = le.clone();
    bad.extend([0x00, 0xD8]);
    let mut odd = le.clone();
    odd.push(0);
    let no_bom = le[2..].to_vec();
    let lying = b"<?xml version='1.0' encoding='UTF-16'?><a/>".to_vec();
    let lying16: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain(
            "<?xml version='1.0' encoding='UTF-8'?><a/>"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        )
        .collect();
    for b in [bad, odd, no_bom, lying, lying16] {
        let e = decode(&b).unwrap_err();
        assert!(matches!(e.kind, ErrorKind::NotWellFormed(_)), "{e:?}");
    }
}

#[test]
fn other_encoding_is_unsupported() {
    let e = decode(b"<?xml version=\"1.0\" encoding=\"windows-1250\"?><a/>").unwrap_err();
    assert_eq!(
        e.kind,
        ErrorKind::UnsupportedEncoding("windows-1250".into())
    );
    assert_eq!(e.to_string(), "unsupported encoding windows-1250");
    // UTF-8 is accepted in any case; no declaration means UTF-8.
    assert!(decode(b"<?xml version='1.0' encoding='utf-8'?><a/>").is_ok());
    assert!(decode(b"<?xml version='1.0'?><a/>").is_ok());
    // A malformed declaration is not well-formed, in decode already.
    let e = decode(b"<?xml version='1.0' encoding='8bit'?><a/>").unwrap_err();
    assert!(matches!(e.kind, ErrorKind::NotWellFormed(_)));
}

#[test]
fn invalid_utf8_rejected() {
    for b in [
        &b"<a>\xFF</a>"[..],
        b"<a>\xC3</a>",
        b"<a>\xED\xA0\x80</a>",
        b"\n\n<a>\xC0\xAF</a>",
    ] {
        let e = decode(b).unwrap_err();
        assert!(
            matches!(e.kind, ErrorKind::NotWellFormed("invalid UTF-8")),
            "{e:?}"
        );
    }
    assert_eq!(decode(b"\n\n<a>\xFF</a>").unwrap_err().line, 3);
}

/// The linearity tests: `steps` stays under `STEPS_PER_BYTE` per input
/// byte for inputs built to provoke super-linear work, at two sizes, so a
/// quadratic path cannot hide behind a small constant.
fn assert_linear(build: impl Fn(usize) -> String, o: Options) {
    for n in [1_000, 64_000] {
        let s = build(n);
        let st = steps(&s, o);
        assert!(
            st <= STEPS_PER_BYTE * s.len() as u64,
            "n={n}: {st} steps for {} bytes",
            s.len()
        );
    }
}

#[test]
fn long_attribute_list_is_linear() {
    // Distinct attributes (the hash-set path) and the same name repeated
    // (refused at the duplicate check, after the whole list).
    assert_linear(
        |n| {
            format!(
                "<a{}/>",
                (0..n).map(|i| format!(" a{i}=''")).collect::<String>()
            )
        },
        opts(),
    );
    assert_linear(|n| format!("<a{}/>", " a=''".repeat(n)), opts());
    // Many elements just below the pairwise threshold.
    let eight: String = (0..PAIRWISE_ATTRS)
        .map(|i| format!(" {}=''", (b'a' + i as u8) as char))
        .collect();
    assert_linear(
        |n| format!("<r>{}</r>", format!("<e{eight}/>").repeat(n)),
        opts(),
    );
    // Prefixed attributes, each resolved through the bindings.
    assert_linear(
        |n| {
            format!(
                "<a xmlns:p='urn:p'{}/>",
                (0..n)
                    .map(|i| format!(" p:a{i}='&amp;\t'"))
                    .collect::<String>()
            )
        },
        opts(),
    );
}

#[test]
fn many_namespace_declarations_are_linear() {
    // Many prefixes on one element, then many lookups of them.
    assert_linear(
        |n| {
            let decls: String = (0..n).map(|i| format!(" xmlns:p{i}='urn:{i}'")).collect();
            let uses: String = (0..n).map(|i| format!("<p{i}:e/>")).collect();
            format!("<r{decls}>{uses}</r>")
        },
        opts(),
    );
    // The same prefix rebound at every level, and closed again.
    assert_linear(
        |n| {
            let d = (n / 300).max(1);
            let one = format!(
                "{}{}",
                "<p:a xmlns:p='urn:x' xmlns='urn:y'>".repeat(200),
                "</p:a>".repeat(200)
            );
            format!("<r>{}</r>", one.repeat(d))
        },
        opts(),
    );
    // Many declarations on many sibling elements.
    assert_linear(
        |n| {
            let decls: String = (0..50).map(|i| format!(" xmlns:p{i}='urn:{i}'")).collect();
            format!("<r>{}</r>", format!("<e{decls}/>").repeat(n / 50 + 1))
        },
        opts(),
    );
}

#[test]
fn deep_nesting_is_linear() {
    // Up to the limit, again and again.
    assert_linear(
        |n| {
            let one = format!("{}{}", "<a>".repeat(256), "</a>".repeat(256));
            format!("<r>{}</r>", one.repeat(n / 256 + 1))
        },
        Options {
            max_depth: 257,
            ..opts()
        },
    );
    // Far past it when the option allows it: the stack is the reader's
    // own, no recursion.
    assert_linear(
        |n| format!("{}{}", "<a>".repeat(n), "</a>".repeat(n)),
        Options {
            max_depth: u32::MAX,
            ..opts()
        },
    );
    // Refused at the limit, with the rest unread.
    assert_linear(|n| "<a>".repeat(n), opts());
    let mut deep = String::new();
    for _ in 0..200_000 {
        deep.push_str("<a>");
    }
    for _ in 0..200_000 {
        deep.push_str("</a>");
    }
    let o = Options {
        max_depth: u32::MAX,
        ..opts()
    };
    assert_eq!(events(&deep, o).unwrap().len(), 400_000);
}

#[test]
fn many_entities_are_linear() {
    assert_linear(|n| format!("<a>{}</a>", "&amp;".repeat(n)), opts());
    assert_linear(|n| format!("<a>{}</a>", "&#x1F600;&#65;".repeat(n)), opts());
    assert_linear(|n| format!("<a v='{}'/>", "&lt;&#10;".repeat(n)), opts());
    // HTML names, each a table search.
    assert_linear(
        |n| format!("<a>{}</a>", "&nbsp;&a;".repeat(n).replace("&a;", "&Aring;")),
        html(),
    );
    // Leading zeros: digits are read once.
    assert_linear(|n| format!("<a>&#{}65;</a>", "0".repeat(n)), opts());
    // References to declared entities: refused at the first.
    assert_linear(
        |n| {
            let decls: String = (0..n).map(|i| format!("<!ENTITY e{i} 'x'>")).collect();
            format!("<!DOCTYPE a [{decls}]><a>{}</a>", "&e0;".repeat(n))
        },
        opts(),
    );
    // Text that alternates borrowed and owned pieces.
    assert_linear(
        |n| format!("<a>{}</a>", "x&amp;<!---->y\r".repeat(n)),
        opts(),
    );
}
