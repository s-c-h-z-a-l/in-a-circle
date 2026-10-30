#![no_main]
//! `probe::probe` and, for a PNG that probes, `probe::check_png_data`, on
//! arbitrary bytes: whatever an image entry of a CBZ holds.

use libfuzzer_sys::fuzz_target;
use probe::{Image, ProbeError};

/// 12 million pixels, not the shipped page limit (180 million): the data
/// check inflates up to `height * (1 + width * 3)` bytes, and under ASan a
/// 180-million-pixel RGB bomb (0.5 MB of input, 540 MB expanded) took 8.3 s,
/// a 169 MB gray one 2.6 s. 12 million pixels expand to at most 36 MB, which
/// keeps the worst input near 0.6 s, 3x under the 2 s timeout in
/// scripts/fuzz.sh (the same trade as the zip_reader target's limits). The
/// 180 million boundary itself is covered by crates/probe/tests. Memory is
/// flat whatever the header says: the probe sizes nothing from it and the
/// data check inflates into a counter.
const MAX_PIXELS: u64 = 12_000_000;

fuzz_target!(|data: &[u8]| {
    let Ok(probed) = probe::probe(data, MAX_PIXELS) else {
        return;
    };
    match probed.image {
        Image::Jpeg(j) => {
            assert!(u64::from(j.width) * u64::from(j.height) <= MAX_PIXELS);
            assert!(matches!(j.components, 1 | 3 | 4));
        }
        Image::Png(p) => {
            assert!(u64::from(p.width) * u64::from(p.height) <= MAX_PIXELS);
            for r in &p.idat {
                assert!(r.end <= data.len() && r.start <= r.end);
            }
            // Any outcome but a panic is fine here; Ok and Damaged are both
            // expected, TooLarge cannot happen under the limit above.
            let r = probe::check_png_data(data, &p);
            assert!(!matches!(r, Err(ProbeError::TooLarge)));
        }
    }
});
