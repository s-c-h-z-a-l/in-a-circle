#![no_main]
//! The catalog cache parser on arbitrary bytes: no panic, no hang. The file
//! is our own write, but it lives on the user's disk and the wheel reads it
//! on every launch. What it accepts must write back as the same entries,
//! and every backoff (`fails`, `retry`) it accepts must obey the retry rule.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    engine::catalog::fuzz_read_cache(data);
});
