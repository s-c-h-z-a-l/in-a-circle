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
];

/// libxml2's verdict: `Ok` if well-formed, else its first message.
fn xmllint(input: &[u8], dir: &Path) -> Result<(), String> {
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
    let first = err.lines().find(|l| l.contains(" error : "));
    match (out.status.success(), first) {
        (true, None) => Ok(()),
        (_, Some(l)) => Err(l.to_string()),
        (false, None) => Err(format!("{} {}", out.status, err.trim())),
    }
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
    let (mut agree, mut diverge) = (0, 0);
    let mut wrong = Vec::new();
    for (name, input) in &inputs {
        let theirs = xmllint(input, dir);
        let mine = ours(input);
        match (&mine, &theirs) {
            (Ok(()), Ok(())) | (Err(_), Err(_)) => agree += 1,
            (Err(e), Ok(())) if stated(e) => diverge += 1,
            _ => wrong.push(format!("{name}: ours {mine:?}, xmllint {theirs:?}")),
        }
    }
    eprintln!(
        "xmllint: {} inputs, {agree} agree, {diverge} stated divergences",
        inputs.len()
    );
    assert!(inputs.len() >= 200, "{}", inputs.len());
    assert!(
        wrong.is_empty(),
        "{} disagree:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}
