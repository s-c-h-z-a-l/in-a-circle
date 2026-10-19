//! Wheel rasterizer: analytic coverage, computed for one octant only.
//!
//! The 8 segments are 45 degrees each, and their boundaries lie on the
//! pixel grid's mirror axes (horizontal, vertical, diagonal). So every
//! pixel's coverage equals that of its mirror image in octant 0, and
//! octant k is exactly segment k. We compute octant 0 once (no atan2: the
//! two boundaries are the lines v = 0 and u = v), mirror it into a dense
//! quadrant table, and paint row by row into the 4 quadrants, so writes
//! stay contiguous. A hover change rewrites one half-quadrant per segment.

use std::f32::consts::{FRAC_1_SQRT_2, TAU};

/// Side of the wheel's square buffer, in pixels. Even, so the center is
/// on a pixel corner and the mirrors map pixels onto pixels.
pub const BOX: u32 = 480;
pub const SEGMENTS: u32 = 8;
const R_OUT: f32 = 220.0;
const R_IN: f32 = 80.0;
const R_HUB: f32 = 64.0;
const GAP: f32 = 2.0;
const C: u32 = BOX / 2;

/// Premultiplied RGBA.
const SEG: [f32; 4] = premul([0.15, 0.15, 0.18], 0.85);
const HOVER: [f32; 4] = premul([0.95, 0.75, 0.20], 0.95);
const HUB_C: [f32; 4] = premul([0.08, 0.08, 0.10], 0.90);

const fn premul(c: [f32; 3], a: f32) -> [f32; 4] {
    [c[0] * a, c[1] * a, c[2] * a, a]
}

/// Hover value for "no segment".
pub const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Segment of quadrant `q` pixels, by side of the diagonal: [a >= b, a < b].
/// Quadrants: 0 = right-down, 1 = left-down, 2 = left-up, 3 = right-up
/// (y grows down, angle grows clockwise from the right).
const SEG_OF: [[u32; 2]; 4] = [[0, 1], [3, 2], [4, 5], [7, 6]];

/// Pixel index of quadrant offset (a, b) in quadrant `q`.
#[inline(always)]
fn qpos(q: usize, a: u32, b: u32) -> usize {
    let x = if q == 0 || q == 3 { C + a } else { C - 1 - a };
    let y = if q < 2 { C + b } else { C - 1 - b };
    (y * BOX + x) as usize
}

/// Coverage of octant pixel (a, b): (hub?, 0..=1).
#[inline(always)]
fn coverage(a: u32, b: u32) -> (bool, f32) {
    let (u, v) = (a as f32 + 0.5, b as f32 + 0.5);
    let r2 = u * u + v * v;
    // Away from the anti-aliased edges the radial terms are 0 or 1: skip
    // the sqrt there (most of the ring).
    const fn sq(x: f32) -> f32 {
        x * x
    }
    if r2 < sq(R_IN - 0.5) {
        if r2 <= sq(R_HUB - 0.5) {
            return (true, 1.0);
        }
        if r2 >= sq(R_HUB + 0.5) {
            return (true, 0.0);
        }
        return (true, (R_HUB - r2.sqrt() + 0.5).clamp(0.0, 1.0));
    }
    let ring = if r2 >= sq(R_IN + 0.5) && r2 <= sq(R_OUT - 0.5) {
        1.0
    } else {
        let r = r2.sqrt();
        (R_OUT - r + 0.5).clamp(0.0, 1.0) * (r - R_IN + 0.5).clamp(0.0, 1.0)
    };
    // Distance to the nearer segment boundary: the line v = 0 or u = v.
    let d = v.min((u - v) * FRAC_1_SQRT_2);
    (false, ring * (d - GAP + 0.5).clamp(0.0, 1.0))
}

/// atan2 via a minimax polynomial on [0, 1]; max error ~1e-5 rad.
#[inline(always)]
fn fast_atan2(y: f32, x: f32) -> f32 {
    let (ax, ay) = (x.abs(), y.abs());
    let (mn, mx) = if ax < ay { (ax, ay) } else { (ay, ax) };
    let z = mn / mx.max(f32::MIN_POSITIVE);
    let z2 = z * z;
    let mut a = z
        * (0.999_977_26
            + z2 * (-0.332_623_47
                + z2 * (0.193_543_46
                    + z2 * (-0.116_432_87 + z2 * (0.052_653_32 + z2 * -0.011_721_2)))));
    if ay > ax {
        a = std::f32::consts::FRAC_PI_2 - a;
    }
    if x < 0.0 {
        a = std::f32::consts::PI - a;
    }
    if y < 0.0 { -a } else { a }
}

/// Segment under a point given in wheel-box coordinates, or NONE (hub or
/// outside the ring). Angle 0 points right, increasing clockwise (y down).
pub fn segment_at(x: f64, y: f64) -> u32 {
    let (px, py) = (x as f32 - C as f32, y as f32 - C as f32);
    let r = (px * px + py * py).sqrt();
    if !(R_IN..=R_OUT).contains(&r) {
        return NONE;
    }
    let mut a = fast_atan2(py, px);
    if a < 0.0 {
        a += TAU;
    }
    ((a / (TAU / SEGMENTS as f32)) as u32).min(SEGMENTS - 1)
}

/// coverage -> premultiplied ARGB8888 (little-endian BGRA) for one color.
fn lut(c: [f32; 4]) -> [u32; 256] {
    let mut t = [0u32; 256];
    for (k, v) in t.iter_mut().enumerate() {
        let q = |x: f32| (x * k as f32 + 0.5) as u32;
        *v = q(c[2]) | q(c[1]) << 8 | q(c[0]) << 16 | q(c[3]) << 24;
    }
    t
}

pub struct Wheel {
    /// Coverage of one quadrant, `cov[b * C + a]` for offsets (a, b) from
    /// the center; computed for one octant and mirrored across the diagonal.
    cov: Vec<u8>,
    /// Per quadrant row b: hub pixels are a < hub_end[b], ring pixels
    /// hub_end[b] <= a < row_end[b].
    hub_end: Vec<u16>,
    row_end: Vec<u16>,
    bboxes: [Rect; SEGMENTS as usize],
    lut_seg: [u32; 256],
    lut_hov: [u32; 256],
    lut_hub: [u32; 256],
}

impl Wheel {
    pub fn new() -> Wheel {
        let reach = ((R_OUT + 1.0).ceil() as u32).min(C);
        let mut cov = vec![0u8; (C * C) as usize];
        let mut hub_end = vec![0u16; C as usize];
        let mut row_end = vec![0u16; C as usize];
        let (mut a0, mut a1, mut b1) = (u32::MAX, 0, 0);
        for a in 0..reach {
            for b in 0..=a {
                let (is_hub, c) = coverage(a, b);
                let q = (c * 255.0 + 0.5) as u8;
                if !is_hub && q != 0 {
                    (a0, a1, b1) = (a0.min(a), a1.max(a), b1.max(b));
                }
                cov[(b * C + a) as usize] = q;
                cov[(a * C + b) as usize] = q;
            }
        }
        // Row extents, from the same radius tests the coverage uses. Both
        // only shrink as b grows, so walk them down: O(C) in total.
        let (hub, out) = ((R_IN - 0.5) * (R_IN - 0.5), (R_OUT + 0.5) * (R_OUT + 0.5));
        let (mut h, mut e) = (C, C);
        for b in 0..C {
            let v = b as f32 + 0.5;
            let r2 = |a: u32| (a as f32 - 0.5) * (a as f32 - 0.5) + v * v; // of a - 1
            while h > 0 && r2(h) >= hub {
                h -= 1;
            }
            while e > 0 && r2(e) >= out {
                e -= 1;
            }
            (hub_end[b as usize], row_end[b as usize]) = (h as u16, e as u16);
        }
        // Quadrant extents of each side's ring: side 0 is a in [a0, a1],
        // b in [0, b1]; side 1 is its mirror across the diagonal.
        let bboxes = std::array::from_fn(|s| {
            let (q, side) = (0..4)
                .flat_map(|q| (0..2).map(move |d| (q, d)))
                .find(|&(q, d)| SEG_OF[q][d] == s as u32)
                .unwrap();
            let ((ax0, ax1), (by0, by1)) = if side == 0 {
                ((a0, a1), (0, b1))
            } else {
                ((0, b1), (a0, a1))
            };
            let (i0, i1) = (qpos(q, ax0, by0) as u32, qpos(q, ax1, by1) as u32);
            let (xa, ya, xb, yb) = (i0 % BOX, i0 / BOX, i1 % BOX, i1 / BOX);
            Rect {
                x: xa.min(xb),
                y: ya.min(yb),
                w: xa.abs_diff(xb) + 1,
                h: ya.abs_diff(yb) + 1,
            }
        });
        Wheel {
            cov,
            hub_end,
            row_end,
            bboxes,
            lut_seg: lut(SEG),
            lut_hov: lut(HOVER),
            lut_hub: lut(HUB_C),
        }
    }

    /// Write quadrant `q`, row `b`, offsets `a0..a1` with one color table.
    /// Coverage 0 maps to 0, which the zeroed buffer already holds, so the
    /// loop writes unconditionally and stays branch-free.
    #[inline(always)]
    fn span(&self, px: &mut [[u8; 4]], q: usize, b: u32, a0: u32, a1: u32, lut: &[u32; 256]) {
        if a0 >= a1 {
            return;
        }
        let src = &self.cov[(b * C + a0) as usize..(b * C + a1) as usize];
        let y = if q < 2 { C + b } else { C - 1 - b };
        let line = &mut px[(y * BOX) as usize..][..BOX as usize];
        if q == 0 || q == 3 {
            let dst = &mut line[(C + a0) as usize..(C + a1) as usize];
            for (d, &c) in dst.iter_mut().zip(src) {
                *d = lut[c as usize].to_le_bytes();
            }
        } else {
            let dst = &mut line[(C - a1) as usize..(C - a0) as usize];
            for (d, &c) in dst.iter_mut().rev().zip(src) {
                *d = lut[c as usize].to_le_bytes();
            }
        }
    }

    fn seg_lut(&self, seg: u32, hover: u32) -> &[u32; 256] {
        if seg == hover {
            &self.lut_hov
        } else {
            &self.lut_seg
        }
    }

    /// Paint the whole wheel into a zeroed BOX x BOX canvas (pixels with
    /// zero coverage are never written). Returns the damage.
    pub fn paint_full(&self, canvas: &mut [u8], hover: u32) -> Rect {
        let px = canvas.as_chunks_mut::<4>().0;
        for (q, segs) in SEG_OF.iter().enumerate() {
            let (lo, hi) = (self.seg_lut(segs[0], hover), self.seg_lut(segs[1], hover));
            for b in 0..C {
                let (h, e) = (
                    self.hub_end[b as usize] as u32,
                    self.row_end[b as usize] as u32,
                );
                self.span(px, q, b, 0, h, &self.lut_hub);
                // a < b is side 1, a >= b side 0.
                self.span(px, q, b, h, b.clamp(h, e), hi);
                self.span(px, q, b, b.clamp(h, e), e, lo);
            }
        }
        Rect {
            x: 0,
            y: 0,
            w: BOX,
            h: BOX,
        }
    }

    fn paint_segment(&self, px: &mut [[u8; 4]], seg: u32, hover: u32) {
        let (q, side) = (0..4)
            .flat_map(|q| (0..2).map(move |d| (q, d)))
            .find(|&(q, d)| SEG_OF[q][d] == seg)
            .unwrap();
        let lut = self.seg_lut(seg, hover);
        for b in 0..C {
            let (h, e) = (
                self.hub_end[b as usize] as u32,
                self.row_end[b as usize] as u32,
            );
            let m = b.clamp(h, e);
            let (a0, a1) = if side == 0 { (m, e) } else { (h, m) };
            self.span(px, q, b, a0, a1, lut);
        }
    }

    /// Repaint a canvas showing `old` to show `new`; only the two segments'
    /// pixels change.
    ///
    /// This is not the damage to report: with more than one buffer, the
    /// screen shows another buffer's state. Damage what differs from the
    /// screen, i.e. `bbox(shown)` and `bbox(new)`.
    pub fn paint_hover(&self, canvas: &mut [u8], old: u32, new: u32) {
        let px = canvas.as_chunks_mut::<4>().0;
        for s in [old, new] {
            if s < SEGMENTS {
                self.paint_segment(px, s, new);
            }
        }
    }

    /// Bounding box of segment `s`, or None for NONE.
    pub fn bbox(&self, s: u32) -> Option<Rect> {
        self.bboxes.get(s as usize).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: usize = (BOX * BOX * 4) as usize;

    fn full(w: &Wheel, hover: u32) -> Vec<u8> {
        let mut c = vec![0u8; SIZE];
        w.paint_full(&mut c, hover);
        c
    }

    /// The previous per-pixel rasterizer (atan2, arc-length gap), as the
    /// reference: segment index and coverage of pixel (x, y).
    fn reference_pixel(x: u32, y: u32) -> (Option<u32>, u8) {
        let c = BOX as f32 / 2.0;
        let (px, py) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
        let r = (px * px + py * py).sqrt();
        let q = |k: f32| (k * 255.0 + 0.5) as u8;
        if r < R_IN - 0.5 {
            return (None, q((R_HUB - r + 0.5).clamp(0.0, 1.0)));
        }
        let ring = (R_OUT - r + 0.5).clamp(0.0, 1.0) * (r - R_IN + 0.5).clamp(0.0, 1.0);
        let mut a = fast_atan2(py, px);
        if a < 0.0 {
            a += TAU;
        }
        let seg = TAU / SEGMENTS as f32;
        let idx = ((a / seg) as u32).min(SEGMENTS - 1);
        let local = a - idx as f32 * seg;
        let gap = (local.min(seg - local) * r - GAP + 0.5).clamp(0.0, 1.0);
        (Some(idx), q(ring * gap))
    }

    #[test]
    fn matches_per_pixel_reference() {
        let w = Wheel::new();
        let (lseg, lhov, lhub) = (lut(SEG), lut(HOVER), lut(HUB_C));
        for hover in [NONE, 0, 3, 7] {
            let got = full(&w, hover);
            let (mut max, mut sum, mut n) = (0u8, 0u64, 0u64);
            for y in 0..BOX {
                for x in 0..BOX {
                    let (idx, cov) = reference_pixel(x, y);
                    let l = match idx {
                        None => &lhub,
                        Some(i) if i == hover => &lhov,
                        Some(_) => &lseg,
                    };
                    let want = l[cov as usize].to_le_bytes();
                    let i = ((y * BOX + x) * 4) as usize;
                    for ch in 0..4 {
                        let d = got[i + ch].abs_diff(want[ch]);
                        max = max.max(d);
                        sum += d as u64;
                        n += 1;
                    }
                }
            }
            let mean = sum as f64 / n as f64;
            assert!(max <= 2, "hover {hover}: max channel diff {max}");
            assert!(mean < 0.01, "hover {hover}: mean diff {mean}");
        }
    }

    /// Every highlighted pixel belongs to the segment hit-testing reports,
    /// and the segment's bbox contains it.
    #[test]
    fn highlight_matches_hit_test_and_bbox() {
        let w = Wheel::new();
        let base = full(&w, NONE);
        for s in 0..SEGMENTS {
            let lit = full(&w, s);
            let r = w.bbox(s).unwrap();
            for i in (0..SIZE).step_by(4) {
                if lit[i..i + 4] == base[i..i + 4] {
                    continue;
                }
                let (x, y) = ((i / 4) as u32 % BOX, (i / 4) as u32 / BOX);
                assert!(x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h);
                let at = segment_at(x as f64 + 0.5, y as f64 + 0.5);
                assert!(at == s || at == NONE, "pixel {x},{y}: seg {s}, hit {at}");
            }
        }
    }

    fn blit(screen: &mut [u8], buf: &[u8], r: Rect) {
        for y in r.y..r.y + r.h {
            let a = ((y * BOX + r.x) * 4) as usize;
            let b = a + (r.w * 4) as usize;
            screen[a..b].copy_from_slice(&buf[a..b]);
        }
    }

    /// A compositor only updates damaged regions. With two buffers in
    /// turn, the screen must still match a full paint after every hover.
    #[test]
    fn screen_matches_full_paint_with_two_buffers() {
        let w = Wheel::new();
        let mut bufs = [full(&w, NONE), full(&w, NONE)];
        let mut painted = [NONE; 2];
        let mut screen = bufs[0].clone();
        let mut shown = NONE;
        let path = [6, 5, 4, NONE, 3, 3, 7, NONE, NONE, 0, 1, 0];
        for (step, &new) in path.iter().enumerate() {
            let i = step % 2;
            w.paint_hover(&mut bufs[i], painted[i], new);
            painted[i] = new;
            for s in [shown, new] {
                if let Some(r) = w.bbox(s) {
                    blit(&mut screen, &bufs[i], r);
                }
            }
            shown = new;
            assert!(
                screen == full(&w, new),
                "step {step}: screen differs (hover {new})"
            );
        }
    }
}
