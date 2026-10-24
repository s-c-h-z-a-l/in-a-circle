#![no_main]
//! `manifest::parse` and `parse_line` on arbitrary text: no panic, no hang.
//! The input is whatever a plugin prints for `--manifest`, or the content of
//! a cache or history file on the user's disk.

use libfuzzer_sys::fuzz_target;

/// The magics the engine really passes (`catalog.rs`, `journal.rs`), plus one
/// that never matches, so the mismatch path runs too.
const MAGICS: [&str; 4] = [
    "inacircle-manifest",
    "inacircle-catalog",
    "inacircle-history",
    "other",
];

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    for magic in MAGICS {
        let _ = manifest::parse(text, magic);
    }
    // Callers also hand single lines to `parse_line`.
    for line in text.lines() {
        if let Ok(l) = manifest::parse_line(line) {
            // The writer must accept whatever the parser produced.
            let _ = manifest::write_line(&l);
        }
    }
});
