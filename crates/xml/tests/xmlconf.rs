//! The W3C XML Conformance Test Suite (xmlts20130923), its standalone
//! XML 1.0 part: every `valid` and `invalid` case (both well-formed) must
//! read to the end, every `not-wf` case must be refused. Source, version
//! and checksum: `tests/data/xmlconf/README`.
//!
//! In the committed subset (and so in scope) are the cases with
//! `ENTITIES="none"` (no external entity), `TYPE` valid, invalid or not-wf
//! (`error` cases leave the verdict to the processor), XML 1.0 (not 1.1,
//! not Namespaces 1.1), the fifth edition's rules (`EDITION` absent or
//! holding 5) and `NAMESPACE="yes"` (the reader always applies Namespaces
//! in XML). Within it, the skips below are categorized; each one is
//! checked to still be what its category says, so the lists cannot rot.

use std::path::{Path, PathBuf};
use xml::{ErrorKind, Options, Reader};

const CATALOGS: &[&str] = &[
    "xmltest/xmltest.xml",
    "sun/sun-valid.xml",
    "sun/sun-invalid.xml",
    "sun/sun-not-wf.xml",
    "oasis/oasis.xml",
    "ibm/ibm_oasis_valid.xml",
    "ibm/ibm_oasis_invalid.xml",
    "ibm/ibm_oasis_not-wf.xml",
    "eduni/errata-2e/errata2e.xml",
    "eduni/errata-3e/errata3e.xml",
    "eduni/errata-4e/errata4e.xml",
    "eduni/namespaces/1.0/rmt-ns10.xml",
    "eduni/namespaces/errata-1e/errata1e.xml",
    "eduni/misc/ht-bh.xml",
];

/// `not-wf` cases whose error is in the DTD (a malformed markup
/// declaration, a literal or reference inside one, a parameter entity
/// rule, an attribute type the DTD declares): the internal subset is
/// skipped lexically, never parsed, so the reader accepts them. None is a
/// risk: nothing a DTD says is used. Grouped by category in the file.
fn dtd_skips() -> Vec<&'static str> {
    include_str!("data/xmlconf-dtd-skips.txt")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

struct Case {
    id: String,
    kind: String,
    path: PathBuf,
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/xmlconf")
}

/// The value of `name="..."` (or `'...'`) in the attribute text of a tag.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag;
    while let Some(i) = rest.find(name) {
        let before = rest[..i].chars().next_back();
        let after = rest[i + name.len()..].trim_start();
        rest = &rest[i + name.len()..];
        if before.is_some_and(|c| !c.is_whitespace()) {
            continue;
        }
        let Some(v) = after.strip_prefix('=') else {
            continue;
        };
        let v = v.trim_start();
        let q = v.chars().next()?;
        let v = &v[1..];
        return Some(&v[..v.find(q)?]);
    }
    None
}

/// The in-scope cases of every catalog.
fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    for cat in CATALOGS {
        let path = root().join(cat);
        let text = std::fs::read_to_string(&path).unwrap();
        let dir = path.parent().unwrap();
        for (i, _) in text.match_indices("<TEST") {
            let chunk = &text[i + "<TEST".len()..];
            // `<TESTCASES` and `<TESTSUITE` are other elements.
            if !chunk.starts_with(char::is_whitespace) {
                continue;
            }
            let tag = &chunk[..chunk.find('>').unwrap()];
            let get = |n| attr(tag, n);
            let kind = get("TYPE").unwrap();
            let in_scope = matches!(kind, "valid" | "invalid" | "not-wf")
                && get("ENTITIES").unwrap_or("none") == "none"
                && !matches!(get("RECOMMENDATION"), Some("XML1.1" | "NS1.1"))
                && get("VERSION").is_none_or(|v| v.split_whitespace().any(|x| x == "1.0"))
                && get("EDITION").is_none_or(|v| v.split_whitespace().any(|x| x == "5"))
                && get("NAMESPACE").unwrap_or("yes") == "yes";
            if in_scope {
                out.push(Case {
                    id: get("ID").unwrap().to_string(),
                    kind: kind.to_string(),
                    path: dir.join(get("URI").unwrap()),
                });
            }
        }
    }
    out
}

/// The verdict on one file: `Ok` if it reads to the end.
fn read(path: &Path) -> Result<(), xml::Error> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let text = xml::decode(&bytes)?;
    let mut r = Reader::new(&text, Options::default());
    while r.next()?.is_some() {}
    Ok(())
}

#[test]
fn standalone_cases_agree_with_the_suite() {
    let cases = cases();
    let (mut pass, mut entity, mut encoding, mut grammar) = (0, 0, 0, 0);
    let mut wrong = Vec::new();
    let skips = dtd_skips();
    for c in &cases {
        let verdict = read(&c.path);
        let wf = c.kind != "not-wf";
        let listed = skips.contains(&c.id.as_str());
        match &verdict {
            // Entity declarations are never expanded: a well-formed case
            // that references one is refused by design.
            Err(e) if wf && e.kind == ErrorKind::EntityDeclarations => entity += 1,
            // Only UTF-8 and UTF-16 are read.
            Err(e) if wf && matches!(e.kind, ErrorKind::UnsupportedEncoding(_)) => encoding += 1,
            Ok(()) if !wf && listed => grammar += 1,
            Ok(()) if wf => pass += 1,
            Err(_) if !wf && !listed => pass += 1,
            _ => wrong.push(format!(
                "{} ({}{}): {verdict:?}",
                c.id,
                c.kind,
                if listed {
                    ", listed as DTD grammar"
                } else {
                    ""
                }
            )),
        }
    }
    eprintln!(
        "xmlconf: {} cases in scope, {pass} agree; skipped: {entity} entity \
         declarations, {encoding} other encodings, {grammar} DTD grammar",
        cases.len()
    );
    assert!(
        wrong.is_empty(),
        "{} disagree:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
    // The subset is the suite, not a handful: a broken catalog reader
    // would otherwise pass with nothing read.
    assert!(cases.len() > 1500, "{}", cases.len());
    // Every listed skip is a case in scope.
    for id in &skips {
        assert!(
            cases.iter().any(|c| c.id == *id),
            "{id} is not a case in scope"
        );
    }
}
