//! Differential test against libxml2's `xmllint`: the well-formedness
//! verdict (including Namespaces in XML) agrees on the hand-written inputs
//! below and on every seed of the fuzz corpus (`fuzz/corpus/xml`).
//! `xmllint --noout --nonet` never fetches anything and loads no DTD; a
//! document is well-formed for it when it exits 0 and reports no error
//! (namespace errors are reported but exit 0).
//!
//! The stated divergences, where this reader is stricter by design:
//! - a reference to an entity a DTD declares or may declare
//!   (`EntityDeclarations`): libxml2 expands or tolerates it;
//! - a declared encoding other than UTF-8 or UTF-16: libxml2 converts it;
//! - a namespace name holding a reference or a line break: refused so
//!   that namespace names always borrow from the input;
//! - an XML version other than 1.x: libxml2 warns and reads on;
//! - a declared encoding that contradicts the bytes (a fatal error in
//!   XML 1.0 section 4.3.3): libxml2 trusts the bytes.
//!
//! And where libxml2 is stricter, outside well-formedness as this reader
//! defines it:
//! - DTD grammar: a markup declaration in the internal subset that breaks
//!   the declaration grammar. This reader skips the internal subset
//!   lexically by design (it never interprets declarations); libxml2
//!   parses each one. Counted only when the input has an internal subset
//!   and every error libxml2 reports is one of its markup-declaration
//!   messages (`dtd_declaration_error`).
//! - namespace-name URI syntax: Namespaces in XML 1.0 does not make the
//!   syntax of a namespace name a namespace constraint; libxml2 reports a
//!   name that is not a URI as a (non-fatal) namespace error. Counted only
//!   when that is every error it reports and it exits 0.
//!
//! A missing `xmllint` skips the test, unless
//! `INACIRCLE_REQUIRE_PDF_TOOLS=1` (the switch for every local test-time
//! tool) makes it a failure.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use xml::{ErrorKind, Options, Reader};

/// A tool on `PATH`; a missing one skips, unless
/// `INACIRCLE_REQUIRE_PDF_TOOLS=1` makes it a failure.
fn have(tool: &str) -> bool {
    let found = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
        .unwrap_or(false);
    if !found {
        if std::env::var_os("INACIRCLE_REQUIRE_PDF_TOOLS").is_some_and(|v| v == "1") {
            panic!("{tool} is required (INACIRCLE_REQUIRE_PDF_TOOLS=1) but not installed");
        }
        eprintln!("skipping: {tool} is not installed");
    }
    found
}

/// Hand-written inputs, each aimed at one rule; the corpus adds the rest.
const HAND: &[&str] = &[
    "<a/>",
    "<a><b/>text<![CDATA[<x>]]><!-- c --><?pi x?></a>",
    "<a x='1' x='2'/>",
    "<a xmlns:a='urn:u' xmlns:b='urn:u' a:x='1' b:x='2'/>",
    "<a xmlns:a='urn:1' xmlns:b='urn:2' a:x='1' b:x='2'/>",
    "<p:a/>",
    "<a xmlns:p=''/>",
    "<a xmlns:xml='urn:x'/>",
    "<a xmlns='http://www.w3.org/XML/1998/namespace'/>",
    "<a:b:c/>",
    "<a>&#0;</a>",
    "<a>&#xD800;</a>",
    "<a>&nbsp;</a>",
    "<a>]]></a>",
    "<a><!-- -- --></a>",
    "<a/><b/>",
    "<a></b>",
    "<!DOCTYPE a [ <!ATTLIST a v CDATA \"]>\"> <!-- ]> --> ]><a/>",
    "<!DOCTYPE a SYSTEM \"file:///etc/passwd\"><a/>",
    "<!DOCTYPE a [ <!ENTITY e \"x\"> ]><a>&e;</a>",
    "\u{FEFF}<a/>",
    "\u{FEFF}\u{FEFF}<a/>",
    "<a\r\nb='1'\r\nb='2'/>",
    // libxml2 stricter: DTD grammar, namespace-name URI syntax.
    "<!DOCTYPE a [ <!ELEMENT a EMPTEMPTY> ]><a/>",
    "<!DOCTYPE a [ <!ATTLIST a b CDATA> ]><a/>",
    "<a xmlns:p=\"hpns/]\"/>",
];

/// libxml2's refusal: whether it still exited 0 (namespace errors do),
/// and every error line it reported (the first one is its verdict; the
/// rest are follow-ups).
#[derive(Debug)]
struct Refusal {
    exit_ok: bool,
    errors: Vec<String>,
}

/// libxml2's verdict: `Ok` if well-formed, else its refusal.
fn xmllint(input: &[u8], dir: &Path) -> Result<(), Refusal> {
    let mut child = Command::new("xmllint")
        .args(["--noout", "--nonet", "--nsclean", "-"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    let errors: Vec<String> = err
        .lines()
        .filter(|l| l.contains(" error : "))
        .map(str::to_string)
        .collect();
    let exit_ok = out.status.success();
    match (exit_ok, errors.is_empty()) {
        (true, true) => Ok(()),
        (_, false) => Err(Refusal { exit_ok, errors }),
        (false, true) => Err(Refusal {
            exit_ok,
            errors: vec![format!("{} {}", out.status, err.trim())],
        }),
    }
}

/// The message of one `xmllint` error line of the given kind
/// (`-:5: parser error : <message>`), if it is of that kind.
fn message<'a>(line: &'a str, kind: &str) -> Option<&'a str> {
    line.split_once(&format!(" {kind} error : "))
        .map(|(_, m)| m)
}

/// Whether a libxml2 parser message is one of its markup-declaration
/// grammar errors (`xmlParseElementDecl`, `xmlParseAttributeListDecl`,
/// `xmlParseEntityDecl`, `xmlParseNotationDecl` and the content-model
/// parsers in libxml2's `parser.c`). Listed form by form, so that any
/// other error, a limit such as "depth too deep" included, stays a
/// disagreement.
fn dtd_declaration_error(m: &str) -> bool {
    const EXACT: &[&str] = &[
        "Space required after 'ELEMENT'",
        "Space required after '<!ATTLIST'",
        "Space required after '<!ENTITY'",
        "Space required after '<!NOTATION'",
        "Space required after the element name",
        "Space required after the attribute name",
        "Space required after the attribute type",
        "Space required after the attribute default value",
        "Space required after the entity name",
        "Space required after the NOTATION name'",
        "xmlParseElementDecl: 'EMPTY', 'ANY' or '(' expected",
        "xmlParseElementDecl: no name for Element",
        "xmlParseEntityDecl: no name",
        "Entity value required",
        "ContentDecl : Name or '(' expected",
        "ContentDecl : ',' '|' or ')' expected",
        "MixedContentDecl : '|' or ')*' expected",
        "MixedContentDecl : '#PCDATA' expected",
        "xmlParseElementMixedContentDecl : Name expected",
    ];
    // Parameterised forms: `'%c' expected`, `%s '(' expected`,
    // `entity %s not terminated`.
    let child = m
        .strip_prefix("xmlParseElementChildrenContentDecl : '")
        .is_some_and(|r| r.len() == "x' expected".len() && r.ends_with("' expected"));
    let content = m.starts_with("xmlParseElementContentDecl : ") && m.ends_with(" '(' expected");
    let entity = m.starts_with("xmlParseEntityDecl: entity ") && m.ends_with(" not terminated");
    EXACT.contains(&m) || child || content || entity
}

/// Whether the input has a DOCTYPE with an internal subset: a `[` after
/// `<!DOCTYPE` and before the declaration's `>`, outside quoted literals.
fn has_internal_subset(input: &[u8]) -> bool {
    let Ok(text) = xml::decode(input) else {
        return false;
    };
    let Some(at) = text.find("<!DOCTYPE") else {
        return false;
    };
    let mut quote = None;
    for c in text[at..].chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '[') => return true,
            (None, '>') => return false,
            _ => {}
        }
    }
    false
}

/// A divergence where libxml2 is stricter than this reader by design;
/// `None` when the disagreement is not one of them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stricter {
    /// A markup declaration in the internal subset breaks the grammar.
    DtdGrammar,
    /// A namespace name is not a syntactically valid URI.
    NamespaceUri,
}

/// Which stated class (if any) a "we accept, libxml2 refuses" verdict
/// falls in. Every reported error must belong to the class, so that a
/// later, real error is never hidden behind an excused first one.
fn stricter(input: &[u8], refusal: &Refusal) -> Option<Stricter> {
    let errors = &refusal.errors;
    let first = errors.first()?;
    if has_internal_subset(input)
        && message(first, "parser").is_some_and(dtd_declaration_error)
        && errors.iter().all(|l| {
            message(l, "parser").is_some_and(|m| {
                dtd_declaration_error(m) || m == "Content error in the internal subset"
            })
        })
    {
        return Some(Stricter::DtdGrammar);
    }
    let uri = |l: &String| {
        message(l, "namespace")
            .is_some_and(|m| m.starts_with("xmlns") && m.ends_with(" not a valid URI"))
    };
    if refusal.exit_ok && errors.iter().all(uri) {
        return Some(Stricter::NamespaceUri);
    }
    None
}

/// Ours, with the HTML names off (libxml2 has none in XML).
fn ours(input: &[u8]) -> Result<(), xml::Error> {
    let text = xml::decode(input)?;
    let mut r = Reader::new(&text, Options::default());
    while r.next()?.is_some() {}
    Ok(())
}

/// Whether a disagreement is one of the stated divergences.
fn stated(e: &xml::Error) -> bool {
    match &e.kind {
        ErrorKind::EntityDeclarations | ErrorKind::UnsupportedEncoding(_) => true,
        ErrorKind::NotWellFormed(m) => matches!(
            *m,
            "a reference or line break in a namespace name"
                | "an XML version other than 1.x"
                | "the declared encoding does not match the bytes"
        ),
        ErrorKind::TooDeep => false,
    }
}

#[test]
fn verdicts_agree_with_xmllint() {
    if !have("xmllint") {
        return;
    }
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/xml");
    let mut inputs: Vec<(String, Vec<u8>)> = HAND
        .iter()
        .map(|s| (format!("hand {s:?}"), s.as_bytes().to_vec()))
        .collect();
    let mut files: Vec<_> = std::fs::read_dir(&corpus)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    for f in files {
        inputs.push((format!("seed {}", f.display()), std::fs::read(&f).unwrap()));
    }
    let (mut agree, mut diverge, mut grammar, mut uri) = (0, 0, 0, 0);
    let mut wrong = Vec::new();
    for (name, input) in &inputs {
        let theirs = xmllint(input, dir);
        let mine = ours(input);
        match (&mine, &theirs) {
            (Ok(()), Ok(())) | (Err(_), Err(_)) => agree += 1,
            (Err(e), Ok(())) if stated(e) => diverge += 1,
            (Ok(()), Err(refusal)) => match stricter(input, refusal) {
                Some(Stricter::DtdGrammar) => grammar += 1,
                Some(Stricter::NamespaceUri) => uri += 1,
                None => wrong.push(format!("{name}: ours {mine:?}, xmllint {theirs:?}")),
            },
            _ => wrong.push(format!("{name}: ours {mine:?}, xmllint {theirs:?}")),
        }
    }
    eprintln!(
        "xmllint: {} inputs, {agree} agree, {diverge} stated divergences; \
         libxml2 stricter: {grammar} DTD grammar, {uri} namespace-name URI syntax",
        inputs.len()
    );
    assert!(inputs.len() >= 200, "{}", inputs.len());
    // The hand inputs exercise both classes.
    assert!(grammar >= 2 && uri >= 1, "{grammar} {uri}");
    assert!(
        wrong.is_empty(),
        "{} disagree:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}
