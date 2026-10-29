//! Structure budgets (`doc::Limits`): every dimension has a max / max+1 pair
//! on a tiny fixture, nothing is built past a budget, and a budget never
//! changes what a conversion that fits produces.

use doc::{LimitError, Limits, ReadError, md, txt};

const ALL: Limits = Limits::UNLIMITED;

fn lim(f: impl FnOnce(&mut Limits)) -> Limits {
    let mut l = ALL;
    f(&mut l);
    l
}

fn exceeded(r: Result<impl std::fmt::Debug, ReadError>, what: &'static str) {
    match r {
        Err(ReadError::Limit(LimitError::Exceeded(w))) if w == what => {}
        other => panic!("expected Exceeded({what:?}), got {other:?}"),
    }
}

/// Smallest value of the budget under which `ok` holds, found by search
/// (`ok` must be monotonic: more budget never turns a pass into a failure).
fn min_ok(mut hi: usize, ok: impl Fn(usize) -> bool) -> usize {
    assert!(ok(hi), "the upper bound itself must pass");
    let mut lo = 0;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if ok(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

// ---- input bytes ----

#[test]
fn input_bytes_max_and_max_plus_one() {
    let text = b"hello\n";
    assert!(txt::read_limited(text, lim(|l| l.input_bytes = 6)).is_ok());
    exceeded(txt::read_limited(text, lim(|l| l.input_bytes = 5)), "input");
    assert!(md::read_limited(text, lim(|l| l.input_bytes = 6)).is_ok());
    exceeded(md::read_limited(text, lim(|l| l.input_bytes = 5)), "input");
}

#[test]
fn input_bytes_are_checked_before_the_content() {
    // Over the budget AND not text: the size is the answer, nothing was read.
    let bad = b"\xff\xff\xff\xff";
    exceeded(txt::read_limited(bad, lim(|l| l.input_bytes = 3)), "input");
    exceeded(md::read_limited(bad, lim(|l| l.input_bytes = 3)), "input");
    assert_eq!(
        txt::read_limited(bad, lim(|l| l.input_bytes = 4)).unwrap_err(),
        ReadError::NotText
    );
}

// ---- nodes ----

/// `n` lines in one paragraph: 1 paragraph + n texts + (n - 1) breaks.
#[test]
fn txt_nodes_max_and_max_plus_one() {
    let n = 50;
    let input = "a\n".repeat(n);
    let need = 1 + n + (n - 1);
    assert!(txt::read_limited(input.as_bytes(), lim(|l| l.nodes = need)).is_ok());
    exceeded(
        txt::read_limited(input.as_bytes(), lim(|l| l.nodes = need - 1)),
        "nodes",
    );
}

/// Many short blocks: each paragraph is a block node plus its text node.
#[test]
fn txt_many_short_paragraphs_spend_the_node_budget() {
    let n = 40;
    let input = "a\n\n".repeat(n);
    assert!(txt::read_limited(input.as_bytes(), lim(|l| l.nodes = 2 * n)).is_ok());
    exceeded(
        txt::read_limited(input.as_bytes(), lim(|l| l.nodes = 2 * n - 1)),
        "nodes",
    );
}

#[test]
fn md_nodes_max_and_max_plus_one() {
    // (markdown, nodes it takes): blocks and inline arena nodes.
    for (src, need) in [
        ("a\n", 2),           // paragraph, text
        ("> a\n", 3),         // quote, paragraph, text
        ("- a\n", 4),         // list, item, paragraph, text
        ("---\n", 1),         // thematic break
        ("```\nx\n```\n", 1), // fenced code
        ("*a*\n", 4),         // paragraph, "*", "a", "*" text; emphasis wraps them (+1)
    ] {
        let need = if src == "*a*\n" { 5 } else { need };
        assert!(
            md::read_limited(src.as_bytes(), lim(|l| l.nodes = need)).is_ok(),
            "{src:?} fits in {need}"
        );
        exceeded(
            md::read_limited(src.as_bytes(), lim(|l| l.nodes = need - 1)),
            "nodes",
        );
    }
}

#[test]
fn md_many_short_lines_blocks_inlines_and_refs_spend_nodes() {
    // Short lines: 1 block + 1 text per line pair of a paragraph.
    let lines = "a\n".repeat(100);
    let n = min_ok(10_000, |b| {
        md::read_limited(lines.as_bytes(), lim(|l| l.nodes = b)).is_ok()
    });
    assert!(n >= 100, "{n}");
    exceeded(
        md::read_limited(lines.as_bytes(), lim(|l| l.nodes = n - 1)),
        "nodes",
    );
    // Many blocks.
    let blocks = "---\n".repeat(100);
    let n = min_ok(10_000, |b| {
        md::read_limited(blocks.as_bytes(), lim(|l| l.nodes = b)).is_ok()
    });
    assert_eq!(n, 100);
    // Many inlines.
    let inl = "*a* ".repeat(100);
    let n = min_ok(10_000, |b| {
        md::read_limited(inl.as_bytes(), lim(|l| l.nodes = b)).is_ok()
    });
    assert!(n >= 300, "{n}");
    // Many link reference definitions: each kept definition is a node.
    let refs: String = (0..100).map(|i| format!("[r{i}]: /u\n")).collect();
    let n = min_ok(10_000, |b| {
        md::read_limited(refs.as_bytes(), lim(|l| l.nodes = b)).is_ok()
    });
    assert!(n >= 100, "{n}");
    exceeded(
        md::read_limited(refs.as_bytes(), lim(|l| l.nodes = 99)),
        "nodes",
    );
}

// ---- owned text ----

#[test]
fn txt_owned_text_max_and_max_plus_one() {
    let input = "abc\ndefg\n"; // 3 + 4 line bytes
    assert!(txt::read_limited(input.as_bytes(), lim(|l| l.text_bytes = 7)).is_ok());
    exceeded(
        txt::read_limited(input.as_bytes(), lim(|l| l.text_bytes = 6)),
        "owned text",
    );
}

#[test]
fn md_owned_text_max_and_max_plus_one() {
    let input = "abc\n";
    // block content "abc\n" (4) + inline text "abc" (3)
    assert!(md::read_limited(input.as_bytes(), lim(|l| l.text_bytes = 7)).is_ok());
    exceeded(
        md::read_limited(input.as_bytes(), lim(|l| l.text_bytes = 6)),
        "owned text",
    );
}

/// Reference expansion: every use copies the destination, so a small
/// document grows past the text budget; the budget stops it.
#[test]
fn md_reference_expansion_stops_at_the_text_budget() {
    let dest = "d".repeat(1000);
    let uses = 50;
    let input = format!("[x]: {dest}\n\n{}", "[x] ".repeat(uses));
    let ok = |b: usize| md::read_limited(input.as_bytes(), lim(|l| l.text_bytes = b)).is_ok();
    let m = min_ok(1 << 20, ok);
    // The 50 copies of the destination are in the charge.
    assert!(m >= uses * dest.len(), "{m}");
    assert!(m < uses * dest.len() + 4 * dest.len(), "{m}");
    assert!(ok(m) && !ok(m - 1));
    exceeded(
        md::read_limited(input.as_bytes(), lim(|l| l.text_bytes = 10 * dest.len())),
        "owned text",
    );
}

/// `&nGt;` is 5 bytes and decodes to 6 (U+226B U+20D2): the budget is charged
/// the decoded length, so the entity form needs exactly one byte more per
/// reference and copy than five plain bytes, at the boundary.
#[test]
fn md_entity_decoding_is_charged_by_decoded_bytes() {
    let k = 40;
    let ent = "&nGt;".repeat(k);
    let plain = "xxxxx".repeat(k);
    let need = |input: &str| {
        let ok = |b: usize| md::read_limited(input.as_bytes(), lim(|l| l.text_bytes = b)).is_ok();
        let m = min_ok(1 << 20, ok);
        assert!(ok(m) && !ok(m - 1));
        m
    };
    // Autolink: the destination and the link text are two copies.
    let a = need(&format!("<http://a/{ent}>\n")) - need(&format!("<http://a/{plain}>\n"));
    assert_eq!(a, 2 * k, "autolink");
    // Inline target: one copy.
    let i = need(&format!("[a](/u{ent})\n")) - need(&format!("[a](/u{plain})\n"));
    assert_eq!(i, k, "inline target");
    // Link reference definition: one copy kept.
    let r = need(&format!("[x]: /u{ent}\n")) - need(&format!("[x]: /u{plain}\n"));
    assert_eq!(r, k, "reference definition");
}

/// NUL becomes U+FFFD: 1 byte to 3, charged as owned text before the copy.
#[test]
fn md_nul_replacement_is_charged() {
    let input = b"\0\0\0\0";
    // copy 4 * 3 = 12, block content 12 + 1 ('\n' added per line), inline 12
    let m = min_ok(1000, |b| {
        md::read_limited(input, lim(|l| l.text_bytes = b)).is_ok()
    });
    // Exactly the three charges above: 12 + 13 + 12.
    assert_eq!(m, 37);
    exceeded(
        md::read_limited(input, lim(|l| l.text_bytes = 11)),
        "owned text",
    );
    // Without NULs the same length costs less.
    assert!(md::read_limited(b"aaaa", lim(|l| l.text_bytes = 11)).is_ok());
}

/// A code span's copy is charged by its length (the block's line content
/// and the copy: 2 bytes per code byte) on top of the run index, not more,
/// not less.
#[test]
fn md_code_span_is_charged_per_byte() {
    let need = |input: &str| {
        let ok = |b: usize| md::read_limited(input.as_bytes(), lim(|l| l.text_bytes = b)).is_ok();
        let m = min_ok(1 << 20, ok);
        assert!(ok(m) && !ok(m - 1));
        m
    };
    // "`abc`": the line "`abc`\n" (6), the index of its two runs (2 x 8,
    // see the next test), the code copy (3).
    assert_eq!(need("`abc`"), 6 + 16 + 3);
    assert_eq!(need("`abcd`"), 7 + 16 + 4);
    assert_eq!(need("`abcdef`") - need("`abc`"), 6);
}

/// The index of backtick runs a code span search builds is charged 8 bytes
/// a run, before it is built, and only once: three unmatched runs cost 24
/// over the same text without backticks.
#[test]
fn md_backtick_run_index_is_charged_per_run() {
    let need = |input: &str| {
        let ok = |b: usize| md::read_limited(input.as_bytes(), lim(|l| l.text_bytes = b)).is_ok();
        let m = min_ok(1 << 20, ok);
        assert!(ok(m) && !ok(m - 1));
        m
    };
    assert_eq!(need("`a ``b ```c") - need("xa xxb xxxc"), 3 * 8);
    // Two more runs, two more charges (no span matches in either).
    assert_eq!(
        need("`a ``b ```c ````d `````e") - need("xa xxb xxxc xxxxd xxxxxe"),
        5 * 8
    );
}

// ---- output ----

#[test]
fn txt_write_output_max_and_max_plus_one() {
    let (d, _) = txt::read(b"a\nb").unwrap();
    // "a\nb\n" = 4 bytes (md writer: "a\\\nb\n" = 5)
    let (out, _) = txt::write_limited(&d, lim(|l| l.output_bytes = 4)).unwrap();
    assert_eq!(out, "a\nb\n");
    assert_eq!(
        txt::write_limited(&d, lim(|l| l.output_bytes = 3)).unwrap_err(),
        LimitError::Exceeded("output")
    );
}

#[test]
fn md_write_output_max_and_max_plus_one() {
    let (d, _) = txt::read(b"a\nb").unwrap();
    let (out, _) = md::write_limited(&d, lim(|l| l.output_bytes = 5)).unwrap();
    assert_eq!(out, "a\\\nb\n");
    assert_eq!(
        md::write_limited(&d, lim(|l| l.output_bytes = 4)).unwrap_err(),
        LimitError::Exceeded("output")
    );
}

/// Escaping grows the output (`*` -> `\*`): the writer counts what it
/// produces, not what it was given.
#[test]
fn md_write_escape_growth_is_counted() {
    let input = "*".repeat(100);
    let (d, _) = txt::read(input.as_bytes()).unwrap();
    let (full, _) = md::write(&d);
    assert_eq!(full.len(), 201);
    assert!(md::write_limited(&d, lim(|l| l.output_bytes = 201)).is_ok());
    assert_eq!(
        md::write_limited(&d, lim(|l| l.output_bytes = 200)).unwrap_err(),
        LimitError::Exceeded("output")
    );
}

/// Nested containers copy their body into the parent: each piece is held
/// to the budget, so a deep quote of a big paragraph stops early.
#[test]
fn nested_blocks_never_exceed_the_output_budget() {
    let src = format!("{}{}\n", "> ".repeat(100), "word ".repeat(100));
    let d = md::read(src.as_bytes()).unwrap();
    let (full, _) = md::write(&d);
    let (txt_full, _) = txt::write(&d);
    assert!(md::write_limited(&d, lim(|l| l.output_bytes = full.len())).is_ok());
    assert!(md::write_limited(&d, lim(|l| l.output_bytes = full.len() - 1)).is_err());
    assert!(txt::write_limited(&d, lim(|l| l.output_bytes = txt_full.len())).is_ok());
    assert!(txt::write_limited(&d, lim(|l| l.output_bytes = txt_full.len() - 1)).is_err());
    // Far below: fails without producing the rest.
    assert!(md::write_limited(&d, lim(|l| l.output_bytes = 10)).is_err());
}

// ---- no behaviour change, no panic at any budget ----

const SPEC: &str = include_str!("data/commonmark-0.31.2.json");

/// Every CommonMark example: under every budget below its need the parse
/// fails cleanly (never panics, never returns a partial tree), at the need
/// it equals the unlimited parse.
#[test]
fn spec_examples_fail_cleanly_below_their_budget_and_agree_at_it() {
    let examples: serde_json::Value = serde_json::from_str(SPEC).unwrap();
    for e in examples.as_array().unwrap() {
        let src = e["markdown"].as_str().unwrap().as_bytes();
        let want = md::read(src).unwrap();
        for dim in 0..2 {
            let with = |b: usize| {
                md::read_limited(
                    src,
                    lim(|l| {
                        if dim == 0 {
                            l.nodes = b
                        } else {
                            l.text_bytes = b
                        }
                    }),
                )
            };
            let need = min_ok(1 << 16, |b| with(b).is_ok());
            for b in 0..need {
                match with(b) {
                    Err(ReadError::Limit(LimitError::Exceeded(_))) => {}
                    other => panic!("example {} budget {b}: {other:?}", e["example"]),
                }
            }
            assert_eq!(with(need).unwrap(), want, "example {}", e["example"]);
        }
        // The writers: the output budget is the exact output length.
        let w = |b: usize| md::write_limited(&want, lim(|l| l.output_bytes = b));
        let full = md::write(&want);
        let need = full.0.len();
        for b in 0..need {
            assert_eq!(w(b).unwrap_err(), LimitError::Exceeded("output"));
        }
        assert_eq!(w(need).unwrap(), full, "example {}", e["example"]);
        let w = |b: usize| txt::write_limited(&want, lim(|l| l.output_bytes = b));
        let full = txt::write(&want);
        let need = full.0.len();
        for b in 0..need {
            assert_eq!(w(b).unwrap_err(), LimitError::Exceeded("output"));
        }
        assert_eq!(w(need).unwrap(), full, "txt example {}", e["example"]);
    }
}

/// A conversion that fits is the old conversion, byte for byte.
#[test]
fn limited_equals_unlimited_when_it_fits() {
    let src = b"# T\n\n- a *b*\n- [l](http://x \"t\")\n\n```\ncode\n```\n";
    let a = md::read(src).unwrap();
    assert_eq!(md::read_limited(src, Limits::DEFAULT).unwrap(), a);
    assert_eq!(
        md::write_limited(&a, Limits::DEFAULT).unwrap(),
        md::write(&a)
    );
    assert_eq!(
        txt::write_limited(&a, Limits::DEFAULT).unwrap(),
        txt::write(&a)
    );
}

#[test]
fn display_names_the_budget() {
    assert_eq!(
        ReadError::Limit(LimitError::Exceeded("nodes")).to_string(),
        "document nodes limit exceeded"
    );
}

/// The shipped values are the G0-P rows.
#[test]
fn default_limits_are_the_g0p_rows() {
    let d = Limits::DEFAULT;
    assert_eq!(d.input_bytes, 8_388_608);
    assert_eq!(d.nodes, 524_288);
    assert_eq!(d.text_bytes, 33_554_432);
    assert_eq!(d.output_bytes, 33_554_432);
}

/// Real limits, hostile shapes, bounded: the budget stops each long before
/// the input is spent (a 4 MiB input of single characters per line would
/// need ~800 MB unbounded).
#[test]
fn hostile_shapes_stop_at_the_default_budget() {
    let lines = "a\n".repeat(1 << 20);
    exceeded(
        txt::read_limited(lines.as_bytes(), Limits::DEFAULT),
        "nodes",
    );
    let emph = "*a* ".repeat(1 << 20);
    exceeded(md::read_limited(emph.as_bytes(), Limits::DEFAULT), "nodes");
    let quotes = "> a\n".repeat(1 << 20);
    exceeded(
        md::read_limited(quotes.as_bytes(), Limits::DEFAULT),
        "nodes",
    );
}

/// A fenced code block in the output is charged what it writes: the budget
/// that equals the output passes, one byte less does not (the up-front
/// room check never refuses what fits). Info string, empty body, a longer
/// fence and a tilde fence included.
#[test]
fn md_fenced_code_output_budget_is_exact() {
    for input in [
        "```\nabc\n```",
        "```rs\nabc\n```",
        "````\na ``` b\n````",
        "```\n```",
        "~~~\na\n~~~",
    ] {
        let (d, _) = md::read(input.as_bytes()).map(|d| (d, ())).unwrap();
        let (text, _) = md::write_limited(&d, ALL).unwrap();
        let n = text.len();
        assert!(
            md::write_limited(&d, lim(|l| l.output_bytes = n)).is_ok(),
            "{input:?}: {n} must fit"
        );
        assert!(
            matches!(
                md::write_limited(&d, lim(|l| l.output_bytes = n - 1)),
                Err(LimitError::Exceeded("output"))
            ),
            "{input:?}: {} must not",
            n - 1
        );
    }
}
