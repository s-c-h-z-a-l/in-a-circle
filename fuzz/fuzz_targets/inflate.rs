#![no_main]
//! `flate::inflate` and `flate::zlib_inflate` on arbitrary bytes: whatever
//! a ZIP entry or a PNG data stream holds.

use libfuzzer_sys::fuzz_target;

/// 64 MiB: the output limit the page reader gives one page (an upper
/// estimate of a decoded page), so the fuzzer exercises the same limit the
/// shipped code runs with. A bomb stops here instead of exhausting memory;
/// the sink below keeps nothing, so memory stays flat either way.
const LIMIT: u64 = 64 << 20;

fuzz_target!(|data: &[u8]| {
    // Same bytes through both entry points: a raw stream is a zlib stream
    // minus the framing, and most inputs are invalid as one of the two.
    let _ = flate::inflate(data, LIMIT, &mut |_| Ok(()));
    let _ = flate::zlib_inflate(data, LIMIT, &mut |_| Ok(()));
});
