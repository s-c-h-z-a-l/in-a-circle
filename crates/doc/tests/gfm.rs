//! GFM spec 0.29-gfm `extension` examples: with the GFM dialect and the
//! tagfilter, `html::write_with(md::read_with(markdown, Gfm))` must equal
//! the expected HTML after normalization. Also pins that the CommonMark
//! dialect keeps reading GFM-looking input the old way.
//!
//! The fixture is the unmodified `test/spec.txt` of cmark-gfm (source, tag
//! and sha256 in `data/LICENSE-gfm`); the examples are extracted here at run
//! time, so there is no conversion step to trust. Extension examples are
//! those with an info string after `example` (`table`, `disabled`,
//! `strikethrough`, `autolink`, `tagfilter`); all sit in a section whose
//! heading ends in "(extension)". Numbers are the spec's: the 1-based
//! position among all `example` blocks.
//!
//! `GFM_VERBOSE=1` prints each failing example with both outputs.

use doc::html::{self, Options};
use doc::{Dialect, md};

/// Examples known to fail, with the reason. Each entry needs one. These
/// are the extensions the reader does not parse yet; an entry goes when its
/// extension lands, and the harness fails an entry that passes.
const SKIP: &[(u32, &str)] = &[
    (198, "extension not implemented yet: table"),
    (199, "extension not implemented yet: table"),
    (200, "extension not implemented yet: table"),
    (201, "extension not implemented yet: table"),
    (202, "extension not implemented yet: table"),
    (204, "extension not implemented yet: table"),
    (205, "extension not implemented yet: table"),
    (279, "extension not implemented yet: task list"),
    (280, "extension not implemented yet: task list"),
    (491, "extension not implemented yet: strikethrough"),
    (621, "extension not implemented yet: autolink"),
    (622, "extension not implemented yet: autolink"),
    (623, "extension not implemented yet: autolink"),
    (624, "extension not implemented yet: autolink"),
    (625, "extension not implemented yet: autolink"),
    (626, "extension not implemented yet: autolink"),
    (627, "extension not implemented yet: autolink"),
    (628, "extension not implemented yet: autolink"),
    (629, "extension not implemented yet: autolink"),
    (630, "extension not implemented yet: autolink"),
    (631, "extension not implemented yet: autolink"),
];

const SPEC: &str = include_str!("data/gfm-spec-0.29.0.gfm.13.txt");

/// Extension examples in `spec.txt`: 8 table, 2 task list (`disabled`: the
/// reference runner skips them, we do not), 2 strikethrough, 11 autolink,
/// 1 tagfilter.
const EXTENSION_EXAMPLES: usize = 24;

/// A fence of 32 backticks opens and closes an example in `spec.txt`.
const FENCE: &str = "````````````````````````````````";

struct Example {
    number: u32,
    section: String,
    kind: String,
    markdown: String,
    html: String,
}

/// All examples of the spec text, extension or not.
fn parse_spec(spec: &str) -> Vec<Example> {
    let mut out = Vec::new();
    let mut section = String::new();
    let mut number = 0;
    let mut lines = spec.lines();
    while let Some(line) = lines.next() {
        if let Some(info) = line
            .strip_prefix(FENCE)
            .and_then(|r| r.strip_prefix(" example"))
        {
            number += 1;
            let (mut markdown, mut html, mut in_html) = (String::new(), String::new(), false);
            for l in lines.by_ref() {
                if l == FENCE {
                    break;
                }
                if l == "." && !in_html {
                    in_html = true;
                    continue;
                }
                let dst = if in_html { &mut html } else { &mut markdown };
                dst.push_str(l);
                dst.push('\n');
            }
            // The spec writes a tab as an arrow.
            out.push(Example {
                number,
                section: section.clone(),
                kind: info.trim().to_string(),
                markdown: markdown.replace('\u{2192}', "\t"),
                html: html.replace('\u{2192}', "\t"),
            });
        } else if line.starts_with('#') {
            section = line.trim_start_matches('#').trim().to_string();
        }
    }
    out
}

/// The whitespace part of the cmark test runner's normalization (same as
/// `commonmark.rs`): whitespace between tags is dropped, and so is
/// leading/trailing whitespace. Applied to both sides.
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
fn extension_examples_are_the_tagged_ones() {
    let all = parse_spec(SPEC);
    assert_eq!(all.len(), 672, "the spec holds 672 examples");
    let ext: Vec<_> = all.iter().filter(|e| !e.kind.is_empty()).collect();
    assert_eq!(ext.len(), EXTENSION_EXAMPLES);
    assert!(ext.iter().all(|e| e.section.ends_with("(extension)")));
    assert!(
        all.iter()
            .filter(|e| e.kind.is_empty())
            .all(|e| !e.section.ends_with("(extension)")),
        "an untagged example sits in an extension section"
    );
}

#[test]
fn spec_extension_examples() {
    for (n, why) in SKIP {
        assert!(!why.is_empty(), "SKIP entry {n} needs a reason");
    }
    let (mut passed, mut skipped) = (0, 0);
    let mut failed: Vec<(u32, String)> = Vec::new();
    for e in parse_spec(SPEC).iter().filter(|e| !e.kind.is_empty()) {
        let doc = md::read_with(e.markdown.as_bytes(), Dialect::Gfm).unwrap();
        let actual = html::write_with(&doc, Options { tagfilter: true });
        let ok = normalize(&actual) == normalize(&e.html);
        if SKIP.iter().any(|&(n, _)| n == e.number) {
            assert!(!ok, "example {} passes now: remove it from SKIP", e.number);
            skipped += 1;
        } else if ok {
            passed += 1;
        } else {
            if std::env::var_os("GFM_VERBOSE").is_some() {
                eprintln!(
                    "--- example {} ({})\n{:?}\nexpected {:?}\nactual   {:?}",
                    e.number, e.section, e.markdown, e.html, actual
                );
            }
            failed.push((e.number, e.section.clone()));
        }
    }
    eprintln!(
        "gfm: {passed} passed, {skipped} skipped, {} failed: {failed:?}",
        failed.len()
    );
    assert!(failed.is_empty(), "{} spec examples failed", failed.len());
}

/// GFM-looking input in CommonMark mode: the old reading, byte for byte.
#[test]
fn commonmark_mode_ignores_gfm_syntax() {
    let cases = [
        ("| a |\n", "<p>| a |</p>\n"),
        ("| a |\n| - |\n| b |\n", "<p>| a |\n| - |\n| b |</p>\n"),
        ("~~x~~\n", "<p>~~x~~</p>\n"),
        ("www.x.y\n", "<p>www.x.y</p>\n"),
        ("http://x.y\n", "<p>http://x.y</p>\n"),
        ("[ ] item\n", "<p>[ ] item</p>\n"),
        ("- [ ] item\n", "<ul>\n<li>[ ] item</li>\n</ul>\n"),
        ("- [x] item\n", "<ul>\n<li>[x] item</li>\n</ul>\n"),
        ("<title>x</title>\n", "<title>x</title>\n"),
    ];
    for (input, expected) in cases {
        let by_dialect = md::read_with(input.as_bytes(), Dialect::CommonMark).unwrap();
        assert_eq!(by_dialect, md::read(input.as_bytes()).unwrap(), "{input:?}");
        assert_eq!(html::write(&by_dialect), expected, "{input:?}");
        // The tagfilter is a writer switch, not a reader one: off, the
        // raw HTML above stays as written.
        assert_eq!(
            html::write_with(&by_dialect, Options::default()),
            expected,
            "{input:?}"
        );
    }
}
