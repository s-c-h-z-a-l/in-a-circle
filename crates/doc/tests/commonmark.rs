//! CommonMark 0.31.2 spec examples: `html::write(md::read(markdown))` must
//! equal the expected HTML after normalization.
//!
//! `COMMONMARK_SECTIONS` (comma-separated section names) restricts which
//! examples count; unset means all of them. Example:
//! `COMMONMARK_SECTIONS="ATX headings,Lists" cargo test -p doc --test commonmark -- --nocapture`
//! `COMMONMARK_VERBOSE=1` also prints each failing example with both outputs.

use doc::{Dialect, html, md};

/// Examples known to fail in CommonMark mode, with the reason. Each entry
/// needs one.
const SKIP: &[(u32, &str)] = &[];

/// Examples whose expected HTML changes in GFM mode, with the reason (the
/// extensions deliberately read some CommonMark input differently). Each
/// entry needs one. Empty until the extensions are parsed.
const SKIP_GFM: &[(u32, &str)] = &[];

const SPEC: &str = include_str!("data/commonmark-0.31.2.json");

/// The whitespace part of the cmark test runner's normalization: whitespace
/// between tags is dropped, and so is leading/trailing whitespace. Applied
/// to both sides, so it can only hide differences, never invent them.
fn normalize(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html.trim();
    while let Some(i) = rest.find('>') {
        out.push_str(&rest[..=i]);
        rest = &rest[i + 1..];
        let ws = rest.len() - rest.trim_start().len();
        if rest[ws..].starts_with('<') {
            rest = &rest[ws..];
        }
    }
    out.push_str(rest);
    out
}

#[test]
fn normalize_drops_whitespace_between_tags_only() {
    assert_eq!(
        normalize(" <p>a b</p>\n <p> c</p>\n"),
        "<p>a b</p><p> c</p>"
    );
    assert_eq!(normalize("a > b"), "a > b");
}

#[test]
fn spec_examples() {
    check_spec(Dialect::CommonMark, SKIP);
}

/// The same 652 examples with the GFM dialect: GFM is a strict superset
/// except for the entries of `SKIP_GFM`.
#[test]
fn spec_examples_gfm_dialect() {
    check_spec(Dialect::Gfm, SKIP_GFM);
}

fn check_spec(dialect: Dialect, skip_list: &[(u32, &str)]) {
    let examples: serde_json::Value = serde_json::from_str(SPEC).unwrap();
    let sections: Option<Vec<String>> = std::env::var("COMMONMARK_SECTIONS")
        .ok()
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect());
    if let Some(names) = &sections {
        for name in names {
            assert!(
                examples
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["section"].as_str() == Some(name.as_str())),
                "COMMONMARK_SECTIONS names an unknown section: {name:?}"
            );
        }
    }
    assert_eq!(
        examples.as_array().unwrap().len(),
        652,
        "the spec file must hold exactly 652 examples"
    );
    for (n, why) in skip_list {
        assert!(!why.is_empty(), "SKIP entry {n} needs a reason");
    }

    let (mut passed, mut skipped) = (0, 0);
    let mut failed: Vec<(u64, String)> = Vec::new();
    for e in examples.as_array().unwrap() {
        let section = e["section"].as_str().unwrap();
        if sections
            .as_ref()
            .is_some_and(|s| !s.iter().any(|x| x == section))
        {
            continue;
        }
        let number = e["example"].as_u64().unwrap();
        let skip = skip_list.iter().any(|&(n, _)| u64::from(n) == number);
        let markdown = e["markdown"].as_str().unwrap();
        let expected = e["html"].as_str().unwrap();
        let actual = html::write(&md::read_with(markdown.as_bytes(), dialect).unwrap());
        let ok = normalize(&actual) == normalize(expected);
        if skip {
            assert!(!ok, "example {number} passes now: remove it from SKIP");
            skipped += 1;
            continue;
        }
        if ok {
            passed += 1;
        } else {
            if std::env::var_os("COMMONMARK_VERBOSE").is_some() {
                eprintln!(
                    "--- example {number} ({section})\n{markdown:?}\nexpected {expected:?}\nactual   {actual:?}"
                );
            }
            failed.push((number, section.to_string()));
        }
    }

    eprintln!(
        "commonmark: {passed} passed, {skipped} skipped, {} failed",
        failed.len()
    );
    let mut by_section: Vec<(String, Vec<u64>)> = Vec::new();
    for (n, s) in &failed {
        match by_section.last_mut() {
            Some((last, ns)) if last == s => ns.push(*n),
            _ => by_section.push((s.clone(), vec![*n])),
        }
    }
    for (s, ns) in &by_section {
        eprintln!("  failed in {s}: {ns:?}");
    }
    assert!(failed.is_empty(), "{} spec examples failed", failed.len());
}
