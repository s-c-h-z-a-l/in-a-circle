//! Our decoder against miniz_oxide (pinned dev-dependency) on generated
//! streams, and on corrupted ones.

use flate::{inflate, zlib_inflate};
use miniz_oxide::deflate::{compress_to_vec, compress_to_vec_zlib};
use miniz_oxide::inflate::{decompress_to_vec_with_limit, decompress_to_vec_zlib_with_limit};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Input generator: size 0..256 KiB (log-uniform, so small cases dominate),
/// one of four shapes.
fn generate(rng: &mut Rng) -> Vec<u8> {
    let bits = rng.below(19) as u32; // 0..=18 -> up to 256 KiB
    let len = if bits == 0 {
        0
    } else {
        rng.below(1 << bits) as usize
    };
    let mut v = Vec::with_capacity(len);
    match rng.below(4) {
        0 => v.extend((0..len).map(|_| rng.next() as u8)),
        1 => v.resize(len, rng.next() as u8),
        2 => {
            let words: [&[u8]; 5] = [b"alpha ", b"beta ", b"gamma\n", b"de", b"lta "];
            while v.len() < len {
                v.extend_from_slice(words[rng.below(5) as usize]);
            }
            v.truncate(len);
        }
        _ => {
            // Short random seed, then copies of earlier stretches.
            let seed = (rng.below(64) + 1) as usize;
            v.extend((0..seed.min(len)).map(|_| rng.next() as u8));
            while v.len() < len {
                let from = rng.below(v.len() as u64) as usize;
                let n = (rng.below(300) + 1) as usize;
                for i in 0..n {
                    let b = v[from + i % (v.len() - from)];
                    v.push(b);
                }
            }
            v.truncate(len);
        }
    }
    v
}

const LIMIT: u64 = 64 << 20;

fn ours_raw(s: &[u8]) -> Result<Vec<u8>, flate::InflateError> {
    let mut out = Vec::new();
    inflate(s, LIMIT, &mut |c| {
        out.extend_from_slice(c);
        Ok(())
    })?;
    Ok(out)
}

fn ours_zlib(s: &[u8]) -> Result<Vec<u8>, flate::InflateError> {
    let mut out = Vec::new();
    zlib_inflate(s, LIMIT, &mut |c| {
        out.extend_from_slice(c);
        Ok(())
    })?;
    Ok(out)
}

#[test]
fn matches_miniz_on_corpus() {
    let mut rng = Rng(0xC0FF_EE00_1234_5678);
    for case in 0..2000 {
        let data = generate(&mut rng);
        let level = rng.below(10) as u8;
        let zlib = rng.below(2) == 1;
        let (got, theirs) = if zlib {
            let s = compress_to_vec_zlib(&data, level);
            (
                ours_zlib(&s),
                decompress_to_vec_zlib_with_limit(&s, LIMIT as usize),
            )
        } else {
            let s = compress_to_vec(&data, level);
            (
                ours_raw(&s),
                decompress_to_vec_with_limit(&s, LIMIT as usize),
            )
        };
        let got = got.unwrap_or_else(|e| panic!("case {case} (level {level}, zlib {zlib}): {e}"));
        let theirs = theirs.expect("miniz decodes its own stream");
        assert!(got == theirs, "case {case}: output differs from miniz");
        assert!(got == data, "case {case}: output differs from the input");
    }
}

#[test]
fn matches_miniz_on_errors() {
    let mut rng = Rng(0xBAD5_EED5_0000_0001);
    let mut errors = 0;
    for case in 0..2000 {
        // Small inputs: a corrupted stream may inflate, and the output
        // stays far below the limit.
        let len = rng.below(4096) as usize;
        let data: Vec<u8> = (0..len)
            .map(|i| {
                if rng.below(3) == 0 {
                    rng.next() as u8
                } else {
                    (i / 7) as u8
                }
            })
            .collect();
        let level = rng.below(10) as u8;
        let zlib = rng.below(2) == 1;
        let mut s = if zlib {
            compress_to_vec_zlib(&data, level)
        } else {
            compress_to_vec(&data, level)
        };
        match rng.below(4) {
            0 if !s.is_empty() => {
                let n = rng.below(s.len() as u64) as usize;
                s.truncate(n);
            }
            _ if !s.is_empty() => {
                for _ in 0..=rng.below(3) {
                    let i = rng.below(s.len() as u64) as usize;
                    s[i] ^= 1 << rng.below(8);
                }
            }
            _ => {}
        }
        let (ours, theirs) = if zlib {
            (
                ours_zlib(&s),
                decompress_to_vec_zlib_with_limit(&s, LIMIT as usize),
            )
        } else {
            (
                ours_raw(&s),
                decompress_to_vec_with_limit(&s, LIMIT as usize),
            )
        };
        errors += usize::from(ours.is_err());
        assert_eq!(
            ours.is_err(),
            theirs.is_err(),
            "case {case} (zlib {zlib}): ours {:?}, miniz {:?}",
            ours.as_ref().map(|_| ()),
            theirs.as_ref().map(|_| ())
        );
        if let (Ok(a), Ok(b)) = (&ours, &theirs) {
            assert!(a == b, "case {case}: accepted outputs differ");
        }
    }
    // The control: the corruption really produces errors.
    assert!(
        errors > 500,
        "only {errors} of 2000 corrupted streams failed"
    );
}
