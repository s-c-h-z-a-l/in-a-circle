#![no_main]
//! `probe::probe` and, for a PNG that probes, `probe::check_png_data`, on
//! arbitrary bytes: whatever an image entry of a CBZ holds. `probe::sniff`
//! reads the same bytes as a stream and must agree with the probe wherever
//! the probe accepts.

use libfuzzer_sys::fuzz_target;
use probe::{Format, Image, ProbeError};

/// 12 million pixels, not the shipped page limit (180 million): the data
/// check inflates up to `height * (1 + width * 3)` bytes, and under ASan a
/// 180-million-pixel RGB bomb (0.5 MB of input, 540 MB expanded) took 8.3 s,
/// a 169 MB gray one 2.6 s. 12 million pixels expand to at most 36 MB, which
/// keeps the worst input near 0.6 s, 3x under the 2 s timeout in
/// scripts/fuzz.sh (the same trade as the zip_reader target's limits). The
/// 180 million boundary itself is covered by crates/probe/tests. Memory is
/// flat whatever the header says: the probe sizes nothing from it, the
/// data check inflates into a counter and the sniff allocates nothing (its
/// one buffer is 4 KiB on the stack). Time and memory are watched by the
/// limits in scripts/fuzz.sh, the same as for the probe.
const MAX_PIXELS: u64 = 12_000_000;

fuzz_target!(|data: &[u8]| {
    let mut stream = data;
    let sniffed = probe::sniff(&mut stream, MAX_PIXELS);
    // Both entry points recognize the same signatures.
    let probed = probe::probe(data, MAX_PIXELS);
    assert_eq!(
        matches!(sniffed, Err(ProbeError::NotImage)),
        matches!(probed, Err(ProbeError::NotImage))
    );
    if let Ok(s) = sniffed {
        assert!(s.width > 0 && s.height > 0);
        assert!(u64::from(s.width) * u64::from(s.height) <= MAX_PIXELS);
    }
    let Ok(probed) = probed else {
        return;
    };
    // The sniff is the looser check: whatever the probe accepts, it accepts
    // with the same format and size.
    let want = match &probed.image {
        Image::Jpeg(j) => (Format::Jpeg, j.width, j.height),
        Image::Png(p) => (Format::Png, p.width, p.height),
    };
    match sniffed {
        Ok(got) => assert_eq!((got.format, got.width, got.height), want),
        // The one exception: the probe has no marker limit, the sniff stops
        // after MAX_STEPS markers and fill bytes. Each step takes at least
        // one byte, so only a longer input can reach it.
        Err(ProbeError::Damaged) if data.len() > probe::sniff::MAX_STEPS as usize => {}
        Err(e) => panic!("the probe accepted, the sniff said {e:?}"),
    }
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
