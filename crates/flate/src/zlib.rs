use crate::inflate::{Bits, InflateError, Inflated, run};
use std::io::{self, Read};

/// Adler-32 (RFC 1950 section 9). Start with `adler = 1`.
pub fn adler32(adler: u32, data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    // Largest n with 255*n*(n+1)/2 + (n+1)*(MOD-1) < 2^32 (zlib's NMAX):
    // the sums can be reduced once per chunk without overflowing a u32.
    const NMAX: usize = 5552;
    let mut a = adler & 0xFFFF;
    let mut b = adler >> 16;
    for chunk in data.chunks(NMAX) {
        for &x in chunk {
            a += u32::from(x);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// Inflate a zlib stream (RFC 1950): 2-byte header (CM=8, CINFO <= 7,
/// FCHECK valid, no preset dictionary), the deflate data, the Adler-32 of
/// the output. Same `limit` semantics as [`crate::inflate`]. The Adler-32
/// is verified after the last chunk was passed to `out`, so a caller must
/// not trust the output before `Ok` is returned.
pub fn zlib_inflate<R: Read>(
    input: R,
    limit: u64,
    out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<Inflated, InflateError> {
    let mut bits = Bits::new(input);
    let cmf = bits.byte()?;
    let flg = bits.byte()?;
    if cmf & 0x0F != 8
        || cmf >> 4 > 7
        || (u16::from(cmf) << 8 | u16::from(flg)) % 31 != 0
        || flg & 0x20 != 0
    {
        return Err(InflateError::BadZlibHeader);
    }
    let mut adler = 1u32;
    let out_bytes = run(&mut bits, limit, &mut |chunk| {
        adler = adler32(adler, chunk);
        out(chunk)
    })?;
    bits.align();
    let mut want = 0u32;
    for _ in 0..4 {
        want = want << 8 | u32::from(bits.byte()?);
    }
    if want != adler {
        return Err(InflateError::Adler32Mismatch);
    }
    Ok(Inflated {
        in_bytes: bits.consumed(),
        out_bytes,
    })
}
