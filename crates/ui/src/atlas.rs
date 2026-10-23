//! The baked glyph atlas, embedded in the binary and read in place.

/// One glyph: bitmap size, placement and advance, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyph<'a> {
    pub w: u32,
    pub h: u32,
    /// Left edge of the bitmap relative to the pen.
    pub xoff: i32,
    /// Top edge of the bitmap relative to the baseline, y down.
    pub yoff: i32,
    pub advance: u32,
    /// `w * h` coverage bytes, row-major.
    pub alpha: &'a [u8],
}

pub struct Atlas {
    data: &'static [u8],
}

static ATLAS: Atlas = Atlas {
    data: include_bytes!("atlas.bin"),
};

pub fn get() -> &'static Atlas {
    &ATLAS
}

// Layout is documented in `tools/bake-atlas`. The data is trusted (embedded at
// build time and checked by tests) but every read is still bounds-checked, so
// a corrupt atlas yields missing glyphs, never a panic or UB.
const HEADER: usize = 40;
const ENTRY: usize = 20;

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

impl Atlas {
    fn count(&self) -> usize {
        match self.data.get(..4) {
            Some(b"IACA") => u32_at(self.data, 16).unwrap_or(0) as usize,
            _ => 0,
        }
    }

    /// The glyph for `c`, or `None` if the atlas lacks it.
    pub fn glyph(&self, c: char) -> Option<Glyph<'_>> {
        let (mut lo, mut hi) = (0, self.count());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let o = HEADER + mid * ENTRY;
            let cp = u32_at(self.data, o)?;
            match cp.cmp(&(c as u32)) {
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
        let alpha_base = HEADER + self.count() * ENTRY;
        let off = alpha_base + u32_at(d, o + 16)? as usize;
        Some(Glyph {
            w,
            h,
            xoff: i32::from(u16_at(d, o + 8)? as i16),
            yoff: i32::from(u16_at(d, o + 10)? as i16),
            advance: u32::from(u16_at(d, o + 12)?),
            alpha: d.get(off..off + (w * h) as usize)?,
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
        v.extend("áéíóöőúüűÁÉÍÓÖŐÚÜŰ…".chars());
        v
    }

    #[test]
    fn atlas_parses_and_has_the_charset() {
        let a = get();
        let cs = charset();
        assert_eq!(cs.len(), 114); // 95 ASCII + 18 accented + the ellipsis
        for c in cs {
            let g = a.glyph(c).unwrap_or_else(|| panic!("missing {c:?}"));
            assert_eq!(g.alpha.len(), (g.w * g.h) as usize, "{c:?}");
        }
        let space = a.glyph(' ').unwrap();
        assert_eq!((space.w * space.h, space.advance > 0), (0, true));
        let l = a.glyph('l').unwrap();
        assert!(l.h >= 8 && l.w >= 1 && l.advance >= l.w, "{l:?}");
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
}
