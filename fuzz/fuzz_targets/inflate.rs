#![no_main]
//! `flate::inflate` and `flate::zlib_inflate` on arbitrary bytes: whatever
//! a ZIP entry or a PNG data stream holds.

use libfuzzer_sys::fuzz_target;

/// 64 KiB = 2 x the 32 KiB window: enough output for the window to wrap
/// once or twice, for matches at the maximum distance and for the limit to
/// fire mid-stream (a 64 KiB input, the max_len in scripts/fuzz.sh, can
/// expand to ~64 MiB, so a bomb stops here after a fraction of a
/// millisecond). It is deliberately far below the page reader's 64 MiB: at
/// 64 MiB every bomb input cost tens of ms and the run managed ~25 exec/s
/// with flat coverage (256 KiB still managed only ~440 exec/s per worker). The limit logic is the same at any value; the exact
/// boundary is covered by `output_limit_stops_at_the_limit` and the
/// million-block case by `many_empty_fixed_blocks` in
/// crates/flate/tests/inflate.rs. The sink keeps nothing, so memory stays
/// flat.
const LIMIT: u64 = 64 << 10;

fuzz_target!(|data: &[u8]| {
    // Same bytes through both entry points: a raw stream is a zlib stream
    // minus the framing, and most inputs are invalid as one of the two.
    let _ = flate::inflate(data, LIMIT, &mut |_| Ok(()));
    let _ = flate::zlib_inflate(data, LIMIT, &mut |_| Ok(()));
});
