//! Wheel rasterizer: analytic coverage, computed for one octant only.
//!
//! The 8 slices are 45 degrees each, and their boundaries lie on the
//! pixel grid's mirror axes (horizontal, vertical, diagonal). So every
//! pixel's coverage equals that of its mirror image in octant 0, and
//! octant k is exactly slice k. We compute octant 0 once (no atan2: the
//! two boundaries are the lines v = 0 and u = v), mirror it into a dense
//! quadrant table, and paint row by row into the 4 quadrants, so writes
//! stay contiguous. A hover change rewrites one half-quadrant per slice,
//! and the slice's label on top of it.
//!
//! Slice 0 points right and the index grows clockwise on screen (y down).
//! Everything here is in the coordinates of the wheel's square box.

use crate::atlas;
use crate::text;
use std::f32::consts::{FRAC_1_SQRT_2, TAU};

/// Side of the wheel's square buffer, in pixels. Even, so the center is
/// on a pixel corner and the mirrors map pixels onto pixels.
pub const BOX: u32 = 480;
const SEGMENTS: u32 = 8;
const R_OUT: f32 = 220.0;
const R_IN: f32 = 80.0;
const R_HUB: f32 = 64.0;
const GAP: f32 = 2.0;
const C: u32 = BOX / 2;
// Diagonal pixels (a == b) sit on a slice boundary and are painted with
// one side's color only. That is invisible only while the gap covers them.
const _: () = assert!(GAP >= 0.5);

/// Widest text the hub shows, in pixels: its corners stay inside the disc.
pub const HUB_TEXT_W: u32 = 108;
/// Label distance from the center.
const R_MID: f32 = (R_IN + R_OUT) / 2.0;
/// Room kept between a label's box and the ring edges and slice gaps.
const LABEL_MARGIN: f32 = 3.0;
/// Alpha factor of Dim and Empty slices and of Dim text.
const DIM_K: f32 = 0.45;

/// Premultiplied RGBA.
const SEG: [f32; 4] = premul([0.15, 0.15, 0.18], 0.85);
const HOVER: [f32; 4] = premul([0.95, 0.75, 0.20], 0.95);
const HUB_C: [f32; 4] = premul([0.08, 0.08, 0.10], 0.90);
const DIM: [f32; 4] = [
    SEG[0] * DIM_K,
    SEG[1] * DIM_K,
    SEG[2] * DIM_K,
    SEG[3] * DIM_K,
];

/// Premultiplied BGRA of full text coverage.
const TEXT: [u8; 4] = [245, 240, 240, 255];
const TEXT_HOVER: [u8; 4] = [25, 20, 20, 255];
const TEXT_DIM: [u8; 4] = [
    (245.0 * DIM_K) as u8,
    (240.0 * DIM_K) as u8,
    (240.0 * DIM_K) as u8,
    (255.0 * DIM_K) as u8,
];

const fn premul(c: [f32; 3], a: f32) -> [f32; 4] {
    [c[0] * a, c[1] * a, c[2] * a, a]
}

/// A pixel rectangle; `w == 0` or `h == 0` is empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const EMPTY: Rect = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Bounding box of both.
    pub fn union(self, o: Rect) -> Rect {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return self;
        }
        let (x, y) = (self.x.min(o.x), self.y.min(o.y));
        let (x1, y1) = (
            (self.x + self.w).max(o.x + o.w),
            (self.y + self.h).max(o.y + o.h),
        );
        Rect {
            x,
            y,
            w: x1 - x,
            h: y1 - y,
        }
    }

    /// True if `o` lies inside `self` (an empty `o` always does).
    pub fn contains(&self, o: Rect) -> bool {
        o.is_empty()
            || (o.x >= self.x
                && o.y >= self.y
                && o.x + o.w <= self.x + self.w
                && o.y + o.h <= self.y + self.h)
    }
}

/// How a slice is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceState {
    /// Label on the fill; the only state that highlights on hover.
    Normal,
    /// Label and fill at reduced alpha: the action does not apply.
    Dim,
    /// The reduced fill only, no label (stage 1: nothing offered yet).
    Empty,
}

pub struct Slice<'a> {
    pub label: &'a str,
    pub state: SliceState,
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
    // Distance to the nearer slice boundary: the line v = 0 or u = v.
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

/// coverage -> premultiplied ARGB8888 (little-endian BGRA) for one color.
fn lut(c: [f32; 4]) -> [u32; 256] {
    let mut t = [0u32; 256];
    for (k, v) in t.iter_mut().enumerate() {
        let q = |x: f32| (x * k as f32 + 0.5) as u32;
        *v = q(c[2]) | q(c[1]) << 8 | q(c[0]) << 16 | q(c[3]) << 24;
    }
    t
}

/// Text placed by its line box's top-left corner.
struct Label {
    text: String,
    x: i32,
    y: i32,
}

/// Whether a `w` x `h` box centred at (`cx`, `cy`) (relative to the wheel
/// center) lies in slice `k` with `LABEL_MARGIN` to spare: its corners and
/// edge midpoints are inside the ring and clear of both slice boundaries.
fn box_fits(k: u32, cx: f32, cy: f32, w: f32, h: f32) -> bool {
    let (hw, hh) = (w / 2.0, h / 2.0);
    let step = TAU / SEGMENTS as f32;
    let (s0, c0) = (k as f32 * step).sin_cos();
    let (s1, c1) = ((k + 1) as f32 * step).sin_cos();
    let clear = GAP + LABEL_MARGIN;
    let pts = [
        (-hw, -hh),
        (hw, -hh),
        (-hw, hh),
        (hw, hh),
        (0.0, -hh),
        (0.0, hh),
        (-hw, 0.0),
        (hw, 0.0),
    ];
    pts.iter().all(|&(dx, dy)| {
        let (px, py) = (cx + dx, cy + dy);
        let r = (px * px + py * py).sqrt();
        // Signed distances to the two boundary lines, positive inside.
        let d0 = c0 * py - s0 * px;
        let d1 = px * s1 - py * c1;
        (R_IN + LABEL_MARGIN..=R_OUT - LABEL_MARGIN).contains(&r) && d0 >= clear && d1 >= clear
    })
}

fn slice_label(k: u32, label: &str) -> Option<Label> {
    if label.is_empty() {
        return None;
    }
    let h = atlas::get().line_height();
    let mid = (k as f32 + 0.5) * (TAU / SEGMENTS as f32);
    let (cx, cy) = (R_MID * mid.cos(), R_MID * mid.sin());
    let max_w = (0..=R_OUT as u32)
        .rev()
        .find(|&w| box_fits(k, cx, cy, w as f32, h as f32))?;
    let text = text::fit(label, max_w);
    if text.is_empty() {
        return None;
    }
    let w = text::width(&text);
    Some(Label {
        x: (C as f32 + cx).round() as i32 - (w / 2) as i32,
        y: (C as f32 + cy).round() as i32 - (h / 2) as i32,
        text: text.into_owned(),
    })
}

fn hub_label(text: &str) -> Option<Label> {
    let text = text::fit(text, HUB_TEXT_W);
    if text.is_empty() {
        return None;
    }
    let (w, h) = (text::width(&text), atlas::get().line_height());
    Some(Label {
        x: C as i32 - (w / 2) as i32,
        y: C as i32 - (h / 2) as i32,
        text: text.into_owned(),
    })
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
    states: [SliceState; SEGMENTS as usize],
    labels: [Option<Label>; SEGMENTS as usize],
    hub: Option<Label>,
    lut_seg: [u32; 256],
    lut_hov: [u32; 256],
    lut_dim: [u32; 256],
    lut_hub: [u32; 256],
}

impl Wheel {
    /// `hub` is cut to `HUB_TEXT_W` with an ellipsis if it is wider; each
    /// label is cut to the room its slice has at the label radius.
    pub fn new(slices: [Slice; 8], hub: &str) -> Wheel {
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
        let (hub_r2, out) = ((R_IN - 0.5) * (R_IN - 0.5), (R_OUT + 0.5) * (R_OUT + 0.5));
        let (mut h, mut e) = (C, C);
        for b in 0..C {
            let v = b as f32 + 0.5;
            let r2 = |a: u32| (a as f32 - 0.5) * (a as f32 - 0.5) + v * v; // of a - 1
            while h > 0 && r2(h) >= hub_r2 {
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
            let (q, side) = quadrant_side(s as u32);
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
        let mut slices = slices.into_iter().enumerate();
        let labels: [Option<Label>; 8] = std::array::from_fn(|_| None);
        let mut wheel = Wheel {
            cov,
            hub_end,
            row_end,
            bboxes,
            states: [SliceState::Empty; 8],
            labels,
            hub: hub_label(hub),
            lut_seg: lut(SEG),
            lut_hov: lut(HOVER),
            lut_dim: lut(DIM),
            lut_hub: lut(HUB_C),
        };
        for (k, s) in slices.by_ref() {
            wheel.states[k] = s.state;
            wheel.labels[k] = match s.state {
                SliceState::Empty => None,
                _ => slice_label(k as u32, s.label),
            };
        }
        wheel
    }

    /// Slice under a point in box coordinates, or None in the hub, outside
    /// the ring, or for a non-finite point. Angle 0 points right and grows
    /// clockwise (y down). Gap pixels belong to their angular slice.
    pub fn segment_at(&self, x: f64, y: f64) -> Option<u8> {
        let (px, py) = (x as f32 - C as f32, y as f32 - C as f32);
        let r = (px * px + py * py).sqrt();
        if !(R_IN..=R_OUT).contains(&r) {
            return None;
        }
        let mut a = fast_atan2(py, px);
        if a < 0.0 {
            a += TAU;
        }
        Some(((a / (TAU / SEGMENTS as f32)) as u32).min(SEGMENTS - 1) as u8)
    }

    /// Bounding box of slice `s`, or None for an index outside 0..8.
    pub fn bbox(&self, s: u8) -> Option<Rect> {
        self.bboxes.get(s as usize).copied()
    }

    /// The slice that shows the highlight for a hover request: only Normal
    /// slices light up.
    fn lit(&self, hover: Option<u8>) -> Option<u8> {
        hover.filter(|&s| self.states.get(s as usize) == Some(&SliceState::Normal))
    }

    /// Write quadrant `q`, row `b`, offsets `a0..a1` with one color table.
    /// Coverage 0 maps to 0, so the loop writes unconditionally and stays
    /// branch-free; that is also what wipes a slice's old label.
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

    fn fill(&self, s: u32, lit: Option<u8>) -> &[u32; 256] {
        match self.states[s as usize] {
            SliceState::Normal if lit == Some(s as u8) => &self.lut_hov,
            SliceState::Normal => &self.lut_seg,
            _ => &self.lut_dim,
        }
    }

    fn draw_label(&self, canvas: &mut [u8], s: u32, lit: Option<u8>) {
        let Some(l) = &self.labels[s as usize] else {
            return;
        };
        let color = match self.states[s as usize] {
            SliceState::Normal if lit == Some(s as u8) => TEXT_HOVER,
            SliceState::Normal => TEXT,
            _ => TEXT_DIM,
        };
        text::draw(canvas, (BOX * 4) as usize, l.x, l.y, &l.text, color);
    }

    /// Paint the whole wheel into `canvas` (BOX x BOX BGRA, cleared first),
    /// slice `hover` highlighted. Returns the damage: the whole box.
    pub fn paint_full(&self, canvas: &mut [u8], hover: Option<u8>) -> Rect {
        let canvas = &mut canvas[..(BOX * BOX * 4) as usize];
        canvas.fill(0);
        let lit = self.lit(hover);
        let px = canvas.as_chunks_mut::<4>().0;
        for (q, segs) in SEG_OF.iter().enumerate() {
            let (lo, hi) = (self.fill(segs[0], lit), self.fill(segs[1], lit));
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
        for s in 0..SEGMENTS {
            self.draw_label(canvas, s, lit);
        }
        if let Some(l) = &self.hub {
            text::draw(canvas, (BOX * 4) as usize, l.x, l.y, &l.text, TEXT);
        }
        Rect {
            x: 0,
            y: 0,
            w: BOX,
            h: BOX,
        }
    }

    fn paint_segment(&self, canvas: &mut [u8], s: u32, lit: Option<u8>) {
        let (q, side) = quadrant_side(s);
        let lut = self.fill(s, lit);
        let px = canvas.as_chunks_mut::<4>().0;
        for b in 0..C {
            let (h, e) = (
                self.hub_end[b as usize] as u32,
                self.row_end[b as usize] as u32,
            );
            let m = b.clamp(h, e);
            let (a0, a1) = if side == 0 { (m, e) } else { (h, m) };
            self.span(px, q, b, a0, a1, lut);
        }
        self.draw_label(canvas, s, lit);
    }

    /// Repaint a canvas showing `old` to show `new`; only the two slices'
    /// pixels (and labels) change. Indexes outside 0..8 count as no slice.
    ///
    /// The returned rectangle is what changed in *this canvas*. With more
    /// than one buffer the screen shows another buffer's state, so feed it
    /// to `damage::Screen` for the damage to report. Empty if neither is a
    /// slice.
    pub fn paint_hover(&self, canvas: &mut [u8], old: Option<u8>, new: Option<u8>) -> Rect {
        let canvas = &mut canvas[..(BOX * BOX * 4) as usize];
        let lit = self.lit(new);
        let mut damage = Rect::EMPTY;
        let mut done = None;
        for s in [old, new].into_iter().flatten() {
            if u32::from(s) >= SEGMENTS || done == Some(s) {
                continue;
            }
            done = Some(s);
            self.paint_segment(canvas, u32::from(s), lit);
            damage = damage.union(self.bboxes[s as usize]);
        }
        damage
    }
}

/// Which quadrant and side of its diagonal slice `s` occupies.
fn quadrant_side(s: u32) -> (usize, usize) {
    (0..4)
        .flat_map(|q| (0..2).map(move |d| (q, d)))
        .find(|&(q, d)| SEG_OF[q][d] == s)
        .unwrap()
}

/// Top-left corner of the wheel's subsurface for a pointer at `cursor`
/// (output-local) on an `output` of the given size: the box centred on the
/// cursor, shifted to stay on the output. A pointer off the output is
/// pulled onto it; an output smaller than the box pins to the origin.
/// The hit test then takes `cursor - place(..)`.
pub fn place(cursor: (i32, i32), output: (u32, u32)) -> (i32, i32) {
    let axis = |c: i32, len: u32| {
        let max = (i64::from(len) - i64::from(BOX)).max(0);
        (i64::from(c) - i64::from(C)).clamp(0, max) as i32
    };
    (axis(cursor.0, output.0), axis(cursor.1, output.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use SliceState::*;

    const SIZE: usize = (BOX * BOX * 4) as usize;

    fn slices<'a>(labels: [&'a str; 8], states: [SliceState; 8]) -> [Slice<'a>; 8] {
        std::array::from_fn(|i| Slice {
            label: labels[i],
            state: states[i],
        })
    }

    /// Normal slices, no text at all: the geometry alone.
    fn bare() -> Wheel {
        Wheel::new(slices([""; 8], [Normal; 8]), "")
    }

    const LABELS: [&str; 8] = [
        "Copy path",
        "Open",
        "Move to…",
        "Compress",
        "Share",
        "A very long action label that cannot possibly fit",
        "Átnevezés",
        "Hash",
    ];
    const STATES: [SliceState; 8] = [Normal, Normal, Dim, Normal, Empty, Normal, Dim, Normal];

    fn sample() -> Wheel {
        Wheel::new(slices(LABELS, STATES), "report.pdf")
    }

    fn full(w: &Wheel, hover: Option<u8>) -> Vec<u8> {
        let mut c = vec![0u8; SIZE];
        w.paint_full(&mut c, hover);
        c
    }

    fn px(c: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * BOX + x) * 4) as usize;
        c[i..i + 4].try_into().unwrap()
    }

    fn pixels(c: &[u8]) -> impl Iterator<Item = (u32, u32, [u8; 4])> + '_ {
        c.chunks_exact(4).enumerate().map(|(i, p)| {
            let i = i as u32;
            (i % BOX, i / BOX, [p[0], p[1], p[2], p[3]])
        })
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
        let w = bare();
        let (lseg, lhov, lhub) = (lut(SEG), lut(HOVER), lut(HUB_C));
        for hover in [None, Some(0), Some(3), Some(7)] {
            let got = full(&w, hover);
            let (mut max, mut sum, mut n) = (0u8, 0u64, 0u64);
            for y in 0..BOX {
                for x in 0..BOX {
                    let (idx, cov) = reference_pixel(x, y);
                    let l = match idx {
                        None => &lhub,
                        Some(i) if Some(i as u8) == hover => &lhov,
                        Some(_) => &lseg,
                    };
                    let want = l[cov as usize].to_le_bytes();
                    let have = px(&got, x, y);
                    for ch in 0..4 {
                        let d = have[ch].abs_diff(want[ch]);
                        max = max.max(d);
                        sum += d as u64;
                        n += 1;
                    }
                }
            }
            let mean = sum as f64 / n as f64;
            assert!(max <= 2, "hover {hover:?}: max channel diff {max}");
            assert!(mean < 0.01, "hover {hover:?}: mean diff {mean}");
        }
    }

    /// Every highlighted pixel belongs to the slice hit-testing reports,
    /// and the slice's bbox contains it.
    #[test]
    fn highlight_matches_hit_test_and_bbox() {
        let w = sample();
        let base = full(&w, None);
        for s in (0..8u8).filter(|&s| STATES[s as usize] == Normal) {
            let lit = full(&w, Some(s));
            let r = w.bbox(s).unwrap();
            let mut changed = 0;
            for (x, y, p) in pixels(&lit) {
                if p == px(&base, x, y) {
                    continue;
                }
                changed += 1;
                assert!(x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h);
                let at = w.segment_at(x as f64 + 0.5, y as f64 + 0.5);
                assert!(
                    at == Some(s) || at.is_none(),
                    "pixel {x},{y}: {s} vs {at:?}"
                );
            }
            assert!(changed > 1000, "slice {s} barely highlights ({changed})");
        }
    }

    #[test]
    fn segment_at_follows_the_compass() {
        let w = bare();
        let c = BOX as f64 / 2.0;
        // 0 = right, growing clockwise on screen (y down).
        for (s, (dx, dy)) in [
            (0, (150.0, 20.0)),
            (1, (20.0, 150.0)),
            (2, (-20.0, 150.0)),
            (3, (-150.0, 20.0)),
            (4, (-150.0, -20.0)),
            (5, (-20.0, -150.0)),
            (6, (20.0, -150.0)),
            (7, (150.0, -20.0)),
        ] {
            assert_eq!(w.segment_at(c + dx, c + dy), Some(s), "{dx},{dy}");
        }
        // The hub, the centre, outside the ring and far away are no slice.
        assert_eq!(w.segment_at(c, c), None);
        assert_eq!(w.segment_at(c + 79.0, c), None);
        assert_eq!(w.segment_at(c + 221.0, c), None);
        assert_eq!(w.segment_at(-1e9, 1e9), None);
        assert_eq!(w.segment_at(f64::NAN, 0.0), None);
    }

    fn blit(screen: &mut [u8], buf: &[u8], r: Rect) {
        for y in r.y..r.y + r.h {
            let a = ((y * BOX + r.x) * 4) as usize;
            let b = a + (r.w * 4) as usize;
            screen[a..b].copy_from_slice(&buf[a..b]);
        }
    }

    #[test]
    fn hover_repaint_equals_full_paint() {
        let w = sample();
        let mut c = full(&w, None);
        let mut cur = None;
        for new in [
            Some(0),
            Some(2),
            Some(2),
            None,
            Some(5),
            Some(7),
            Some(4),
            None,
        ] {
            let r = w.paint_hover(&mut c, cur, new);
            assert!(c == full(&w, new), "{cur:?} -> {new:?}");
            for s in [cur, new].into_iter().flatten() {
                assert!(r.contains(w.bbox(s).unwrap()), "{cur:?} -> {new:?}");
            }
            cur = new;
        }
    }

    #[test]
    fn hover_repaint_touches_only_the_damage_rect() {
        let w = sample();
        let before = full(&w, Some(3));
        let mut after = before.clone();
        let r = w.paint_hover(&mut after, Some(3), Some(0));
        for (x, y, p) in pixels(&after) {
            let inside = x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
            assert!(inside || p == px(&before, x, y), "{x},{y} outside {r:?}");
        }
        assert!(r.w * r.h < BOX * BOX / 2, "not just two slices: {r:?}");
    }

    #[test]
    fn hover_without_slices_is_no_damage() {
        let w = sample();
        let mut c = full(&w, None);
        let before = c.clone();
        assert!(w.paint_hover(&mut c, None, None).is_empty());
        assert!(c == before);
        // Out-of-range indexes are no slice either.
        assert!(w.paint_hover(&mut c, Some(9), Some(200)).is_empty());
        assert!(c == before);
    }

    #[test]
    fn only_normal_slices_highlight() {
        let w = sample();
        let base = full(&w, None);
        for s in [2u8, 4, 6] {
            assert!(
                full(&w, Some(s)) == base,
                "slice {s} ({:?})",
                STATES[s as usize]
            );
        }
        assert!(full(&w, Some(0)) != base);
    }

    /// A compositor only updates damaged regions. With two buffers in
    /// turn, the screen must still match a full paint after every hover.
    #[test]
    fn screen_matches_full_paint_with_two_buffers() {
        use crate::damage::Screen;
        let w = sample();
        let mut bufs = [full(&w, None), full(&w, None)];
        let mut painted: [Option<u8>; 2] = [None; 2];
        let mut screen = bufs[0].clone();
        let mut state = Screen::new();
        let path = [
            Some(6),
            Some(5),
            Some(4),
            None,
            Some(3),
            Some(3),
            Some(7),
            None,
            None,
            Some(0),
            Some(1),
            Some(0),
            Some(7),
            Some(0),
        ];
        for (step, &new) in path.iter().enumerate() {
            let i = step % 2;
            let r = w.paint_hover(&mut bufs[i], painted[i], new);
            painted[i] = new;
            let d = state.commit(i, r);
            blit(&mut screen, &bufs[i], d);
            assert!(
                screen == full(&w, new),
                "step {step}: screen differs (hover {new:?})"
            );
        }
    }

    /// The spike's bug: damaging only what this buffer repainted leaves the
    /// other buffer's last change on screen.
    #[test]
    fn buffer_relative_damage_is_not_enough() {
        use crate::damage::Screen;
        let w = sample();
        let mut bufs = [full(&w, None), full(&w, None)];
        let mut screen = bufs[0].clone();
        let r0 = w.paint_hover(&mut bufs[0], None, Some(0));
        blit(&mut screen, &bufs[0], r0);
        let r1 = w.paint_hover(&mut bufs[1], None, Some(3));
        let mut naive = screen.clone();
        blit(&mut naive, &bufs[1], r1);
        assert!(naive != full(&w, Some(3)), "premise: naive damage is wrong");
        let mut st = Screen::new();
        st.commit(0, r0);
        let d = st.damage_for(1, r1);
        blit(&mut screen, &bufs[1], d);
        assert!(screen == full(&w, Some(3)));
    }

    #[test]
    fn labels_are_drawn_in_their_slice() {
        let with = sample();
        let with_text = full(&with, None);
        for k in 0..8u8 {
            let mut l = LABELS;
            l[k as usize] = "";
            let without = Wheel::new(slices(l, STATES), "report.pdf");
            let base = full(&without, None);
            let r = with.bbox(k).unwrap();
            let mut n = 0;
            for (x, y, p) in pixels(&with_text) {
                if p == px(&base, x, y) {
                    continue;
                }
                n += 1;
                assert!(x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h);
                assert_eq!(
                    with.segment_at(x as f64 + 0.5, y as f64 + 0.5),
                    Some(k),
                    "slice {k} label pixel {x},{y}"
                );
                // Text sits on the fill, never in the gap or outside the ring.
                assert!(px(&base, x, y)[3] >= 90, "label pixel {x},{y} off the fill");
            }
            if STATES[k as usize] == Empty {
                assert_eq!(n, 0, "Empty slice {k} draws a label");
            } else {
                assert!(n > 40, "slice {k} label has only {n} pixels");
            }
        }
    }

    #[test]
    fn labels_survive_hover_and_leave() {
        let w = sample();
        let mut c = full(&w, None);
        w.paint_hover(&mut c, None, Some(0));
        w.paint_hover(&mut c, Some(0), None);
        assert!(c == full(&w, None));
        // Hovered text differs from idle text: it is redrawn on the new fill.
        assert!(full(&w, Some(0)) != full(&w, None));
    }

    #[test]
    fn hub_text_is_drawn_inside_the_hub() {
        let a = Wheel::new(slices(LABELS, STATES), "report.pdf");
        let b = Wheel::new(slices(LABELS, STATES), "");
        let (ca, cb) = (full(&a, None), full(&b, None));
        let c = BOX as f64 / 2.0;
        let mut n = 0;
        for (x, y, p) in pixels(&ca) {
            if p == px(&cb, x, y) {
                continue;
            }
            n += 1;
            let (dx, dy) = (x as f64 + 0.5 - c, y as f64 + 0.5 - c);
            assert!((dx * dx + dy * dy).sqrt() < R_HUB as f64 - 1.0, "{x},{y}");
        }
        assert!(n > 40);
        // A hub text wider than the hub is cut, not drawn across the ring.
        let long = Wheel::new(slices(LABELS, STATES), &"W".repeat(60));
        let cl = full(&long, None);
        for (x, y, p) in pixels(&cl) {
            if p != px(&cb, x, y) {
                let (dx, dy) = (x as f64 + 0.5 - c, y as f64 + 0.5 - c);
                assert!((dx * dx + dy * dy).sqrt() < R_HUB as f64, "{x},{y}");
            }
        }
    }

    #[test]
    fn dim_slices_are_dimmer() {
        let w = Wheel::new(
            slices(
                ["A"; 8],
                [Normal, Dim, Empty, Normal, Normal, Normal, Normal, Normal],
            ),
            "",
        );
        let c = full(&w, None);
        let at = |s: u8| {
            // A ring pixel at mid radius, off the label, on the slice's axis.
            let a = (s as f64 + 0.5) * std::f64::consts::FRAC_PI_4;
            let (x, y) = (
                (BOX / 2) as f64 + 100.0 * a.cos(),
                (BOX / 2) as f64 + 100.0 * a.sin(),
            );
            px(&c, x as u32, y as u32)
        };
        let (n, d, e) = (at(0), at(1), at(2));
        assert!(d[3] < n[3] && d[3] > 0, "dim alpha {d:?} vs {n:?}");
        assert!(e[3] < n[3] && e[3] > 0, "empty alpha {e:?}");
        // Dim text is dimmer than normal text: the brightest red channel
        // within the slice itself (text is lighter than any fill).
        let peak = |states: [SliceState; 8]| {
            let w = Wheel::new(slices(["Open"; 8], states), "");
            let c = full(&w, None);
            pixels(&c)
                .filter(|&(x, y, _)| w.segment_at(x as f64 + 0.5, y as f64 + 0.5) == Some(1))
                .map(|(_, _, p)| p[2])
                .max()
                .unwrap()
        };
        let dim = peak([Normal, Dim, Normal, Normal, Normal, Normal, Normal, Normal]);
        let normal = peak([Normal; 8]);
        assert!(dim < normal, "dim text {dim} vs normal {normal}");
    }

    #[test]
    fn wheel_is_clamped_to_the_output() {
        let b = BOX as i32;
        let out = (1920u32, 1080u32);
        // Centred on the cursor when there is room.
        assert_eq!(place((960, 540), out), (960 - b / 2, 540 - b / 2));
        // Corners and edges: shifted so the box stays on the output.
        assert_eq!(place((0, 0), out), (0, 0));
        assert_eq!(place((1919, 1079), out), (1920 - b, 1080 - b));
        assert_eq!(place((0, 1079), out), (0, 1080 - b));
        assert_eq!(place((1919, 0), out), (1920 - b, 0));
        assert_eq!(place((100, 540), out), (0, 540 - b / 2));
        assert_eq!(place((1900, 540), out), (1920 - b, 540 - b / 2));
        // The second monitor of a pair: the output is its own width, and the
        // cursor is local to it, so its right edge clamps at its own width.
        assert_eq!(place((2559, 700), (2560, 1440)), (2560 - b, 700 - b / 2));
        // An output smaller than the box pins to the origin; the cursor
        // outside the output, or absurd, still lands on it.
        assert_eq!(place((50, 50), (400, 300)), (0, 0));
        assert_eq!(place((-5000, 99_999), out), (0, 1080 - b));
        assert_eq!(place((i32::MIN, i32::MAX), out), (0, 1080 - b));
        assert_eq!(place((i32::MAX, i32::MIN), (u32::MAX, u32::MAX)).1, 0);
        // The pointer maps back into the wheel: the cursor stays inside the
        // box and the hit test is in box coordinates.
        let (ox, oy) = place((30, 30), out);
        let w = bare();
        let (lx, ly) = (30.0 - ox as f64, 30.0 - oy as f64);
        assert!((0.0..BOX as f64).contains(&lx) && (0.0..BOX as f64).contains(&ly));
        assert_eq!(w.segment_at(lx, ly), None); // the corner is outside the ring
    }

    #[test]
    fn rect_union_and_contains() {
        let a = Rect {
            x: 10,
            y: 10,
            w: 5,
            h: 5,
        };
        let b = Rect {
            x: 20,
            y: 0,
            w: 5,
            h: 5,
        };
        let u = a.union(b);
        assert_eq!(
            u,
            Rect {
                x: 10,
                y: 0,
                w: 15,
                h: 15
            }
        );
        assert!(u.contains(a) && u.contains(b) && !a.contains(b));
        assert_eq!(Rect::EMPTY.union(a), a);
        assert_eq!(a.union(Rect::EMPTY), a);
        assert!(Rect::EMPTY.is_empty() && !a.is_empty());
        assert!(a.contains(Rect::EMPTY));
    }

    /// Release only: `cargo test -p ui --release -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing; run in release"]
    fn hover_repaint_cost() {
        let w = sample();
        let mut c = full(&w, None);
        let mut times = Vec::new();
        let mut cur = None;
        for i in 0..2000u32 {
            let new = if i % 5 == 4 {
                None
            } else {
                Some((i % 8) as u8)
            };
            let t = std::time::Instant::now();
            let r = w.paint_hover(&mut c, cur, new);
            std::hint::black_box(r);
            times.push(t.elapsed().as_nanos() as u64);
            cur = new;
        }
        times.sort_unstable();
        let (med, max) = (times[times.len() / 2], times[times.len() - 1]);
        println!(
            "hover repaint: median {} µs, max {} µs",
            med / 1000,
            max / 1000
        );
        let t = std::time::Instant::now();
        let w2 = sample();
        let new_t = t.elapsed();
        let t = std::time::Instant::now();
        std::hint::black_box(full(&w2, None));
        println!(
            "Wheel::new {} µs, paint_full (incl. 900 KB alloc) {} µs",
            new_t.as_micros(),
            t.elapsed().as_micros()
        );
        assert!(med <= 100_000, "median {med} ns over the 100 µs budget");
    }
}
