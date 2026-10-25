#![no_main]
//! The catalog cache parser on arbitrary bytes: no panic, no hang. The file
//! is our own write, but it lives on the user's disk and the wheel reads it
//! on every launch.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    engine::catalog::fuzz_read_cache(data);
});
