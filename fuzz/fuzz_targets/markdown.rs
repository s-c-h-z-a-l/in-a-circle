#![no_main]
//! The document readers on arbitrary bytes, then every writer on the tree
//! they built: the two real conversions (`inacircle-text`: txt->md and
//! md->txt) plus `html::write`, so the tree walks run too.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(d) = doc::md::read(data) {
        let _ = doc::html::write(&d);
        let _ = doc::txt::write(&d);
        let _ = doc::md::write(&d);
    }
    if let Ok((d, _losses)) = doc::txt::read(data) {
        let _ = doc::html::write(&d);
        let _ = doc::md::write(&d);
    }
    // The shipped, budgeted path under budgets small enough to be hit by
    // short inputs: it must stop with an error, never panic or hang. The
    // budgets are data-derived so every budget gets its turn.
    let n = data.len() % 64;
    let tight = doc::Limits {
        input_bytes: usize::MAX,
        nodes: n,
        text_bytes: n * 4,
        output_bytes: n * 8,
    };
    if let Ok(d) = doc::md::read_limited(data, tight) {
        let _ = doc::txt::write_limited(&d, tight);
        let _ = doc::md::write_limited(&d, tight);
    }
    if let Ok((d, _)) = doc::txt::read_limited(data, tight) {
        let _ = doc::md::write_limited(&d, tight);
    }
});
