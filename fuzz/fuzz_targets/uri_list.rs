#![no_main]
//! `parse_uri_list` on arbitrary bytes: the `text/uri-list` another
//! application puts in a drag. The shipped source is included as is (it is
//! std-only), so the fuzzed code is the shipped code.

use libfuzzer_sys::fuzz_target;

#[path = "../../bins/inacircle/src/uri.rs"]
#[allow(dead_code)]
mod uri;

fuzz_target!(|data: &[u8]| {
    // The caller reads at most MAX_BYTES (1 MiB); keep -max_len at or below it.
    let _ = uri::parse_uri_list(data);
});
