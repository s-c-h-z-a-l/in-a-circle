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
});
