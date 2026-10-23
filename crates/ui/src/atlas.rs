//! The baked glyph atlas, embedded in the binary and read in place.

/// One glyph: bitmap size and placement in pixels, advance in 1/64 px.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyph<'a> {
    pub w: u32,
    pub h: u32,
    /// Left edge of the bitmap relative to the pen.
    pub xoff: i32,
    /// Top edge of the bitmap relative to the baseline, y down.
    pub yoff: i32,
    /// Pen advance, 26.6 fixed point (`SUBPIXEL` units per pixel).
    pub advance: u32,
    /// `w * h` coverage bytes, row-major.
    pub alpha: &'a [u8],
}

/// Glyph advances are 26.6 fixed point: this many units per pixel.
pub const SUBPIXEL: u32 = 64;

pub struct Atlas {
    /// Empty if the image failed validation.
    data: &'static [u8],
}

static ATLAS: Atlas = Atlas::from_bytes(include_bytes!("atlas.bin"));

pub fn get() -> &'static Atlas {
    &ATLAS
}

// Layout is documented in `tools/bake-atlas`. The image is trusted (embedded
// at build time and checked by tests) but still validated up front, and every
// read is bounds-checked, so a corrupt atlas yields no glyphs, never a panic.
const HEADER: usize = 40;
const ENTRY: usize = 20;
const VERSION: u16 = 2;

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(o..o.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(o..o.checked_add(4)?)?.try_into().ok()?,
    ))
}

impl Atlas {
    /// Wraps an atlas image; bad magic, an unknown version, a short header or
    /// a glyph table past the end of the data give an empty atlas.
    const fn from_bytes(data: &'static [u8]) -> Atlas {
        const EMPTY: &[u8] = &[];
        if data.len() < HEADER
            || !matches!(data, [b'I', b'A', b'C', b'A', ..])
            || u16::from_le_bytes([data[4], data[5]]) != VERSION
        {
            return Atlas { data: EMPTY };
        }
        let count = u32::from_le_bytes([data[16], data[17], data[18], data[19]]) as u64;
        if HEADER as u64 + count * ENTRY as u64 > data.len() as u64 {
            return Atlas { data: EMPTY };
        }
        Atlas { data }
    }

    fn count(&self) -> usize {
        u32_at(self.data, 16).unwrap_or(0) as usize
    }

    /// The glyph for `c`, or `None` if the atlas lacks it.
    pub fn glyph(&self, c: char) -> Option<Glyph<'_>> {
        let (mut lo, mut hi) = (0, self.count());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let o = HEADER + mid * ENTRY;
            match u32_at(self.data, o)?.cmp(&(c as u32)) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return self.entry(o),
            }
        }
        None
    }

    fn entry(&self, o: usize) -> Option<Glyph<'_>> {
        let d = self.data;
        let (w, h) = (u32::from(u16_at(d, o + 4)?), u32::from(u16_at(d, o + 6)?));
        let start = HEADER
            .checked_add(self.count() * ENTRY)?
            .checked_add(u32_at(d, o + 16)? as usize)?;
        Some(Glyph {
            w,
            h,
            xoff: i32::from(u16_at(d, o + 8)? as i16),
            yoff: i32::from(u16_at(d, o + 10)? as i16),
            advance: u32::from(u16_at(d, o + 12)?),
            alpha: d.get(start..start.checked_add((w * h) as usize)?)?,
        })
    }

    /// True if every character of `s` has a glyph.
    pub fn has_all(&self, s: &str) -> bool {
        s.chars().all(|c| self.glyph(c).is_some())
    }

    /// Pixels from the top of a line to its baseline.
    pub fn ascent(&self) -> i32 {
        u16_at(self.data, 8).map_or(0, |v| i32::from(v as i16))
    }

    /// Distance between baselines: ascent + descent + line gap.
    pub fn line_height(&self) -> u32 {
        let f = |o| u16_at(self.data, o).map_or(0, |v| i32::from(v as i16));
        (f(8) + f(10) + f(12)).max(0) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn charset() -> Vec<char> {
        let mut v: Vec<char> = (0x20u8..=0x7E).map(char::from).collect();
        v.extend("áéíóöőúüűÁÉÍÓÖŐÚÜŰ…·".chars());
        v
    }

    #[test]
    fn atlas_parses_and_has_the_charset() {
        let a = get();
        let cs = charset();
        assert_eq!(cs.len(), 115); // 95 ASCII + 18 accented + `…` + `·`
        for c in cs {
            let g = a.glyph(c).unwrap_or_else(|| panic!("missing {c:?}"));
            assert_eq!(g.alpha.len(), (g.w * g.h) as usize, "{c:?}");
        }
        let space = a.glyph(' ').unwrap();
        assert_eq!((space.w * space.h, space.advance > 0), (0, true));
        let l = a.glyph('l').unwrap();
        assert!(l.h >= 8 && l.w >= 1 && l.advance >= l.w * 64, "{l:?}");
        // Advances are 26.6 fixed point, not whole pixels.
        assert_ne!(a.glyph('e').unwrap().advance % 64, 0);
        // 'l' rises above the baseline and reaches it.
        assert!(l.yoff < 0 && l.yoff + l.h as i32 >= 0);
        assert!(l.alpha.iter().max().is_some_and(|&m| m >= 200));
        let lh = a.line_height();
        assert!((15..=25).contains(&lh), "line height {lh}");
    }

    #[test]
    fn missing_glyph_is_detected() {
        let a = get();
        assert!(a.has_all(""));
        assert!(a.has_all("Árvíztűrő tükörfúrógép … 0.1"));
        assert!(!a.has_all("漢字.txt"));
        assert!(!a.has_all("ok\u{1F600}"));
        assert!(a.glyph('漢').is_none());
        assert!(a.glyph('\n').is_none());
    }

    /// A v2 atlas image: header, `entries` as (cp, w, h, xoff, yoff, adv, off), alpha.
    fn image(
        version: u16,
        magic: &[u8; 4],
        count: u32,
        entries: &[[u32; 7]],
        alpha: &[u8],
    ) -> &'static [u8] {
        let mut v = Vec::new();
        v.extend(magic);
        v.extend(version.to_le_bytes());
        v.extend(15u16.to_le_bytes());
        v.extend(12i16.to_le_bytes());
        v.extend(3i16.to_le_bytes());
        v.extend(0i16.to_le_bytes());
        v.extend(0u16.to_le_bytes());
        v.extend(count.to_le_bytes());
        v.extend((alpha.len() as u32).to_le_bytes());
        v.extend([0u8; 16]);
        for e in entries {
            v.extend(e[0].to_le_bytes());
            v.extend((e[1] as u16).to_le_bytes());
            v.extend((e[2] as u16).to_le_bytes());
            v.extend((e[3] as i16).to_le_bytes());
            v.extend((e[4] as i16).to_le_bytes());
            v.extend((e[5] as u16).to_le_bytes());
            v.extend(0u16.to_le_bytes());
            v.extend(e[6].to_le_bytes());
        }
        v.extend(alpha);
        Box::leak(v.into_boxed_slice())
    }

    #[test]
    fn corrupt_atlas_yields_nothing_and_never_panics() {
        let ok = [[65, 2, 2, 0, 0xFFFF_FFFF, 640, 0]];
        let good = Atlas::from_bytes(image(2, b"IACA", 1, &ok, &[1, 2, 3, 4]));
        let g = good.glyph('A').unwrap();
        assert_eq!(
            (g.w, g.h, g.yoff, g.advance, g.alpha),
            (2, 2, -1, 640, &[1u8, 2, 3, 4][..])
        );
        assert_eq!((good.ascent(), good.line_height()), (12, 15));

        let empty = |a: &Atlas| {
            assert!(a.glyph('A').is_none() && !a.has_all("A"));
            assert_eq!((a.ascent(), a.line_height()), (0, 0));
        };
        // Truncated and short headers, empty data.
        let full = image(2, b"IACA", 1, &ok, &[1, 2, 3, 4]);
        for n in [0, 3, 4, 8, 39] {
            empty(&Atlas::from_bytes(&full[..n]));
        }
        // Bad magic, unknown versions (the old integer-advance format too).
        empty(&Atlas::from_bytes(image(2, b"XXXX", 1, &ok, &[1, 2, 3, 4])));
        empty(&Atlas::from_bytes(image(1, b"IACA", 1, &ok, &[1, 2, 3, 4])));
        empty(&Atlas::from_bytes(image(3, b"IACA", 1, &ok, &[1, 2, 3, 4])));
        // Glyph count past the data.
        for count in [2, 1000, u32::MAX] {
            empty(&Atlas::from_bytes(image(
                2,
                b"IACA",
                count,
                &ok,
                &[1, 2, 3, 4],
            )));
        }
        // Table cut short.
        empty(&Atlas::from_bytes(&full[..50]));
        // Alpha offset out of range, and w*h past the end: that glyph only.
        for bad in [
            [65, 2, 2, 0, 0, 640, 100],
            [65, 2, 2, 0, 0, 640, u32::MAX],
            [65, 65535, 65535, 0, 0, 640, 0],
            [65, 3, 2, 0, 0, 640, 0],
        ] {
            let a = Atlas::from_bytes(image(2, b"IACA", 1, &[bad], &[1, 2, 3, 4]));
            assert!(a.glyph('A').is_none(), "{bad:?}");
            assert!(a.glyph('B').is_none());
        }
    }
}
