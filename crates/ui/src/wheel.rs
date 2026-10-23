//! Wheel rasterizer: analytic coverage, computed for one octant only.
//!
//! The 8 slices are 45 degrees each, centred on the compass directions:
//! slice 0 points north (up) and the index grows clockwise on screen, so a
//! preset's compass direction is its slice. The borders lie at 22.5° +
//! k·45°. That set is symmetric about the pixel grid's mirror axes
//! (horizontal, vertical, diagonal), so every pixel's coverage equals that
//! of its mirror image in octant 0, which holds one border: the line at
//! 22.5° from the horizontal axis. The octant is computed once, at compile
//! time (no atan2), mirrored into a dense quadrant table, and painted row
//! by row into the 4 quadrants, so writes stay contiguous. Each quadrant
//! row splits at the two borders it crosses into three runs: a horizontal
//! slice (E or W), a diagonal one, a vertical one (N or S). A hover change
//! rewrites one slice's runs, and its label on top of them.
//!
//! Everything here is in the coordinates of the wheel's square box.
//!
//! The tables depend only on the constants below, so they are built by
//! `const` evaluation: `.rodata` holds `REACH * REACH` bytes of coverage
//! (48.8 KB), 4 KB of colour tables and about 2.5 KB of row extents and
//! splits, and `Wheel::new` computes nothing but the label layout. A test
//! recomputes them at run time and compares.

use crate::text;
use std::f32::consts::{FRAC_1_SQRT_2, FRAC_PI_2, FRAC_PI_8, TAU};

/// Side of the wheel's square buffer, in pixels. Even, so the center is
/// on a pixel corner and the mirrors map pixels onto pixels.
pub const BOX: u32 = 480;
const SEGMENTS: u32 = 8;
const R_OUT: f32 = 220.0;
const R_IN: f32 = 80.0;
const R_HUB: f32 = 64.0;
const GAP: f32 = 2.0;
const C: u32 = BOX / 2;
/// Pixels from the center the ring reaches (plus the anti-aliased edge):
/// the coverage table is cut to this.
const REACH: usize = R_OUT as usize + 1;
// Pixels whose center is near a border are painted with one side's color
// only (the side of their center). That is invisible only while the gap
// covers them.
const _: () = assert!(GAP >= 0.5);
/// sin and cos of 22.5°: the border in octant 0.
const S22: f32 = 0.382_683_43;
const C22: f32 = 0.923_879_5;
const _: () = assert!(R_OUT == (R_OUT as usize) as f32 && REACH <= C as usize);

/// Length in bytes of the canvas the wheel paints: BOX rows of BOX BGRA
/// pixels, rows packed (stride `BOX * 4`).
const CANVAS_LEN: usize = (BOX * BOX * 4) as usize;
const STRIDE: usize = (BOX * 4) as usize;

/// Widest text the hub shows, in pixels: its corners stay inside the disc.
pub const HUB_TEXT_W: u32 = 108;
/// Label distance from the center.
const R_MID: f32 = (R_IN + R_OUT) / 2.0;
/// Room kept between a label's box and the ring edges and slice gaps.
const LABEL_MARGIN: f32 = 3.0;
/// Alpha factor of Dim and Empty slices and of Dim text.
const DIM_K: f32 = 0.45;
/// Height of a label's box: the atlas's line height (a test keeps them equal).
const LABEL_H: u32 = 20;

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

    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Bounding box of both. Edges saturate at `u32::MAX`.
    pub fn union(self, o: Rect) -> Rect {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return self;
        }
        let (x, y) = (self.x.min(o.x), self.y.min(o.y));
        let x1 = self.x.saturating_add(self.w).max(o.x.saturating_add(o.w));
        let y1 = self.y.saturating_add(self.h).max(o.y.saturating_add(o.h));
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
                && o.x.saturating_add(o.w) <= self.x.saturating_add(self.w)
                && o.y.saturating_add(o.h) <= self.y.saturating_add(self.h))
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

/// What is under a point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// Inside slice `0..8`, clear of its borders.
    Slice(u8),
    /// Inside the wheel (`r <= R_OUT`) but not on a slice: the hub disc, the
    /// transparent annulus between hub and ring, the gaps along the slice
    /// borders. A drop target in stage 1, a no-op click in stage 2.
    Hub,
    /// Beyond the outer radius, or a non-finite point: what closes the wheel.
    Outside,
}

/// The slice of each run of quadrant `q`: [horizontal, diagonal,
/// vertical], i.e. quadrant offsets a > b·tan 67.5°, between, and
/// a < b·tan 22.5°. Quadrants: 0 = right-down, 1 = left-down, 2 = left-up,
/// 3 = right-up (y grows down). Slices: 0 = N, 1 = NE, ... 7 = NW.
const SEG_OF: [[u32; 3]; 4] = [[2, 3, 4], [6, 5, 4], [6, 7, 0], [2, 1, 0]];
/// The runs of a quadrant row, in `SEG_OF` order.
const RUNS: usize = 3;

/// Pixel index of quadrant offset (a, b) in quadrant `q`.
const fn qpos(q: usize, a: u32, b: u32) -> usize {
    let x = if q == 0 || q == 3 { C + a } else { C - 1 - a };
    let y = if q < 2 { C + b } else { C - 1 - b };
    (y * BOX + x) as usize
}

const fn clamp01(x: f32) -> f32 {
    if x < 0.0 {
        0.0
    } else if x > 1.0 {
        1.0
    } else {
        x
    }
}

/// Square root for `const` evaluation: Newton's method in f64 from above,
/// rounded to f32. The input is an exact f32 and f64 has more than twice its
/// precision, so this is the correctly rounded f32 square root (a test
/// compares every table entry with `f32::sqrt`).
const fn sqrt32(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let x = x as f64;
    let mut g = if x > 1.0 { (x + 1.0) * 0.5 } else { 1.0 };
    loop {
        let n = 0.5 * (g + x / g);
        if n >= g {
            return g as f32;
        }
        g = n;
    }
}

/// Coverage of octant pixel (a, b): (hub?, 0..=1).
const fn coverage(a: u32, b: u32) -> (bool, f32) {
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
        return (true, clamp01(R_HUB - sqrt32(r2) + 0.5));
    }
    let ring = if r2 >= sq(R_IN + 0.5) && r2 <= sq(R_OUT - 0.5) {
        1.0
    } else {
        let r = sqrt32(r2);
        clamp01(R_OUT - r + 0.5) * clamp01(r - R_IN + 0.5)
    };
    // Distance to the octant's slice border, the line at 22.5°.
    let d = u * S22 - v * C22;
    let d = if d < 0.0 { -d } else { d };
    (false, ring * clamp01(d - GAP + 0.5))
}

/// Everything about the geometry that does not change at run time.
struct Geo {
    /// Coverage of one quadrant, `cov[b * REACH + a]` for offsets (a, b)
    /// from the center; computed for one octant and mirrored across the
    /// diagonal.
    cov: [u8; REACH * REACH],
    /// Per quadrant row b: hub pixels are a < hub_end[b], ring pixels
    /// hub_end[b] <= a < row_end[b]; 0 beyond the ring.
    hub_end: [u16; C as usize],
    row_end: [u16; C as usize],
    /// Per quadrant row b: the vertical run is a < split[0][b], the diagonal
    /// one split[0][b] <= a < split[1][b], the horizontal one beyond (each
    /// clamped to the ring). By the pixel center's side of each border.
    split: [[u16; C as usize]; 2],
    /// Bounding box of each slice's ring pixels.
    bboxes: [Rect; SEGMENTS as usize],
}

/// Which run (`SEG_OF` order) quadrant pixel (a, b) is in.
const fn run_of(split: &[[u16; C as usize]; 2], a: u32, b: u32) -> usize {
    if a < split[0][b as usize] as u32 {
        2
    } else if a < split[1][b as usize] as u32 {
        1
    } else {
        0
    }
}

/// Bounding box of quadrant offsets `a0..=a1`, `b0..=b1` in quadrant `q`.
const fn quadrant_rect(q: usize, (a0, a1, b0, b1): (u32, u32, u32, u32)) -> Rect {
    let (i0, i1) = (qpos(q, a0, b0) as u32, qpos(q, a1, b1) as u32);
    let (xa, ya, xb, yb) = (i0 % BOX, i0 / BOX, i1 % BOX, i1 / BOX);
    Rect {
        x: if xa < xb { xa } else { xb },
        y: if ya < yb { ya } else { yb },
        w: xa.abs_diff(xb) + 1,
        h: ya.abs_diff(yb) + 1,
    }
}

const fn union_const(r: Rect, o: Rect) -> Rect {
    if r.is_empty() {
        return o;
    }
    if o.is_empty() {
        return r;
    }
    let (x, y) = (
        if r.x < o.x { r.x } else { o.x },
        if r.y < o.y { r.y } else { o.y },
    );
    let (rx, ox) = (r.x + r.w, o.x + o.w);
    let (ry, oy) = (r.y + r.h, o.y + o.h);
    Rect {
        x,
        y,
        w: if rx > ox { rx } else { ox } - x,
        h: if ry > oy { ry } else { oy } - y,
    }
}

const fn build() -> Geo {
    let mut cov = [0u8; REACH * REACH];
    let mut a = 0;
    while a < REACH {
        let mut b = 0;
        while b <= a {
            let (_, c) = coverage(a as u32, b as u32);
            let q = (c * 255.0 + 0.5) as u8;
            cov[b * REACH + a] = q;
            cov[a * REACH + b] = q;
            b += 1;
        }
        a += 1;
    }
    // Row extents, from the same radius tests the coverage uses. Both only
    // shrink as b grows, so walk them down: O(C) in total.
    let (hub_r2, out) = ((R_IN - 0.5) * (R_IN - 0.5), (R_OUT + 0.5) * (R_OUT + 0.5));
    let (mut hub_end, mut row_end) = ([0u16; C as usize], [0u16; C as usize]);
    let (mut h, mut e) = (C, C);
    let mut b = 0;
    while b < C {
        let v = b as f32 + 0.5;
        // Squared radius of pixel `a - 1`.
        while h > 0 && ((h as f32 - 0.5) * (h as f32 - 0.5) + v * v) >= hub_r2 {
            h -= 1;
        }
        while e > 0 && ((e as f32 - 0.5) * (e as f32 - 0.5) + v * v) >= out {
            e -= 1;
        }
        hub_end[b as usize] = h as u16;
        row_end[b as usize] = e as u16;
        b += 1;
    }
    // Run splits: the first a whose center is on or past each border, by
    // `a + 0.5 >= v * tan`. Both only grow with b: walk them up.
    let tans = [S22 / C22, C22 / S22];
    let mut split = [[0u16; C as usize]; 2];
    let mut t = 0;
    while t < 2 {
        let mut a = 0u32;
        let mut b = 0;
        while b < C {
            let lim = (b as f32 + 0.5) * tans[t];
            while (a as f32 + 0.5) < lim && a < C {
                a += 1;
            }
            split[t][b as usize] = a as u16;
            b += 1;
        }
        t += 1;
    }
    // Each run's extent in quadrant offsets (the same in every quadrant by
    // symmetry), then each slice's box from its runs.
    let mut ext = [(u32::MAX, 0u32, u32::MAX, 0u32); RUNS];
    let mut b = 0;
    while b < REACH as u32 {
        let mut a = hub_end[b as usize] as u32;
        while a < REACH as u32 {
            if cov[b as usize * REACH + a as usize] != 0 {
                let r = run_of(&split, a, b);
                let e = &mut ext[r];
                if a < e.0 {
                    e.0 = a;
                }
                if a > e.1 {
                    e.1 = a;
                }
                if b < e.2 {
                    e.2 = b;
                }
                if b > e.3 {
                    e.3 = b;
                }
            }
            a += 1;
        }
        b += 1;
    }
    let mut bboxes = [Rect::EMPTY; SEGMENTS as usize];
    let mut q = 0;
    while q < 4 {
        let mut r = 0;
        while r < RUNS {
            let s = SEG_OF[q][r] as usize;
            bboxes[s] = union_const(bboxes[s], quadrant_rect(q, ext[r]));
            r += 1;
        }
        q += 1;
    }
    Geo {
        cov,
        hub_end,
        row_end,
        split,
        bboxes,
    }
}

// About 25 k coverage evaluations and a 48 k-pixel scan: more than the
// default const-eval budget.
#[allow(long_running_const_eval)]
static GEO: Geo = build();

const fn lut_q(x: f32, k: f32) -> u32 {
    (x * k + 0.5) as u32
}

/// coverage -> premultiplied ARGB8888 (little-endian BGRA) for one color.
const fn lut(c: [f32; 4]) -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut k = 0;
    while k < 256 {
        let kf = k as f32;
        t[k] =
            lut_q(c[2], kf) | lut_q(c[1], kf) << 8 | lut_q(c[0], kf) << 16 | lut_q(c[3], kf) << 24;
        k += 1;
    }
    t
}

static LUT_SEG: [u32; 256] = lut(SEG);
static LUT_HOV: [u32; 256] = lut(HOVER);
static LUT_DIM: [u32; 256] = lut(DIM);
static LUT_HUB: [u32; 256] = lut(HUB_C);

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

/// (sin, cos) of the slice borders, screen angles `-112.5° + k·45°`, k in
/// 0..=8 (y down, clockwise from the right): slice k lies between border k
/// and border k + 1.
const BORDER: [(f32, f32); 9] = [
    (-C22, -S22),
    (-C22, S22),
    (-S22, C22),
    (S22, C22),
    (C22, S22),
    (C22, -S22),
    (S22, -C22),
    (-S22, -C22),
    (-C22, -S22),
];

/// Signed distances from point (`px`, `py`) (relative to the center) to the
/// two borders of slice `k`; positive inside the slice.
const fn border_dist(k: usize, px: f32, py: f32) -> (f32, f32) {
    let ((s0, c0), (s1, c1)) = (BORDER[k], BORDER[k + 1]);
    (c0 * py - s0 * px, px * s1 - py * c1)
}

/// Whether a `w` x `h` box centred at (`cx`, `cy`) (relative to the wheel
/// center) lies in slice `k` with `LABEL_MARGIN` to spare: its corners and
/// edge midpoints are inside the ring and clear of both slice borders.
const fn box_fits(k: usize, cx: f32, cy: f32, w: f32, h: f32) -> bool {
    let (hw, hh) = (w / 2.0, h / 2.0);
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
    let (lo, hi) = (R_IN + LABEL_MARGIN, R_OUT - LABEL_MARGIN);
    let mut i = 0;
    while i < pts.len() {
        let (px, py) = (cx + pts[i].0, cy + pts[i].1);
        let r2 = px * px + py * py;
        let (d0, d1) = border_dist(k, px, py);
        if r2 < lo * lo || r2 > hi * hi || d0 < clear || d1 < clear {
            return false;
        }
        i += 1;
    }
    true
}

/// Widest label for slice `k`, found by search. Its center: radius R_MID
/// at the slice's middle angle.
const fn max_label_w(k: usize, cx: f32, cy: f32) -> u32 {
    let mut w = R_OUT as u32;
    while w > 0 && !box_fits(k, cx, cy, w as f32, LABEL_H as f32) {
        w -= 1;
    }
    w
}

/// Widest label per slice class (`label_class`). A label box is symmetric
/// about both axes but wider than tall, so E and W share one value, N and
/// S another, and the four diagonal slices a third. Measured on E, N, SE.
const LABEL_W: [u32; 3] = [
    max_label_w(2, R_MID, 0.0),
    max_label_w(0, 0.0, -R_MID),
    max_label_w(3, R_MID * FRAC_1_SQRT_2, R_MID * FRAC_1_SQRT_2),
];

/// Index into `LABEL_W` of slice `k`.
const fn label_class(k: u32) -> usize {
    match k % 4 {
        2 => 0,
        0 => 1,
        _ => 2,
    }
}

/// Text placed by its line box's top-left corner.
struct Label {
    text: String,
    x: i32,
    y: i32,
}

fn slice_label(k: u32, label: &str) -> Option<Label> {
    if label.is_empty() {
        return None;
    }
    let max_w = LABEL_W[label_class(k)];
    let mid = k as f32 * (TAU / SEGMENTS as f32) - FRAC_PI_2;
    let (cx, cy) = (R_MID * mid.cos(), R_MID * mid.sin());
    let text = text::fit(label, max_w);
    if text.is_empty() {
        return None;
    }
    let w = text::width(&text);
    Some(Label {
        x: (C as f32 + cx).round() as i32 - (w / 2) as i32,
        y: (C as f32 + cy).round() as i32 - (LABEL_H / 2) as i32,
        text: text.into_owned(),
    })
}

fn hub_label(text: &str) -> Option<Label> {
    let text = text::fit(text, HUB_TEXT_W);
    if text.is_empty() {
        return None;
    }
    let w = text::width(&text);
    Some(Label {
        x: C as i32 - (w / 2) as i32,
        y: C as i32 - (LABEL_H / 2) as i32,
        text: text.into_owned(),
    })
}

pub struct Wheel {
    states: [SliceState; SEGMENTS as usize],
    labels: [Option<Label>; SEGMENTS as usize],
    hub: Option<Label>,
}

impl Wheel {
    /// `hub` is cut to `HUB_TEXT_W` with an ellipsis if it is wider; each
    /// label is cut to the room its slice has at the label radius.
    pub fn new(slices: [Slice; 8], hub: &str) -> Wheel {
        let mut wheel = Wheel {
            states: [SliceState::Empty; 8],
            labels: std::array::from_fn(|_| None),
            hub: hub_label(hub),
        };
        for (k, s) in slices.into_iter().enumerate() {
            wheel.states[k] = s.state;
            wheel.labels[k] = match s.state {
                SliceState::Empty => None,
                _ => slice_label(k as u32, s.label),
            };
        }
        wheel
    }

    /// What is under a point in box coordinates: a slice (clear of its
    /// borders), else `Hub` anywhere within the outer radius, else `Outside`
    /// (including every non-finite point).
    pub fn hit(&self, x: f64, y: f64) -> Hit {
        let (px, py) = (x as f32 - C as f32, y as f32 - C as f32);
        let r2 = px * px + py * py;
        // NaN fails every comparison, so it lands in `Outside` too.
        if !(0.0..=R_OUT * R_OUT).contains(&r2) {
            return Hit::Outside;
        }
        if r2 < R_IN * R_IN {
            return Hit::Hub;
        }
        // Clockwise from slice 0's first border (north minus 22.5°).
        let mut a = fast_atan2(py, px) + FRAC_PI_2 + FRAC_PI_8;
        if a < 0.0 {
            a += TAU;
        }
        if a >= TAU {
            a -= TAU;
        }
        let k = ((a / (TAU / SEGMENTS as f32)) as usize).min(SEGMENTS as usize - 1);
        let (d0, d1) = border_dist(k, px, py);
        if d0 < GAP || d1 < GAP {
            return Hit::Hub;
        }
        Hit::Slice(k as u8)
    }

    /// Bounding box of slice `s`, or None for an index outside 0..8.
    pub fn bbox(&self, s: u8) -> Option<Rect> {
        GEO.bboxes.get(s as usize).copied()
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
    fn span(px: &mut [[u8; 4]], q: usize, b: u32, a0: u32, a1: u32, lut: &[u32; 256]) {
        if a0 >= a1 {
            return;
        }
        let row = b as usize * REACH;
        let src = &GEO.cov[row + a0 as usize..row + a1 as usize];
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

    fn fill(&self, s: u32, lit: Option<u8>) -> &'static [u32; 256] {
        match self.states[s as usize] {
            SliceState::Normal if lit == Some(s as u8) => &LUT_HOV,
            SliceState::Normal => &LUT_SEG,
            _ => &LUT_DIM,
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
        text::draw(canvas, STRIDE, l.x, l.y, &l.text, color);
    }

    /// Paint the whole wheel into `canvas` with slice `hover` highlighted.
    /// Every pixel is written, so the canvas may hold anything. Returns the
    /// damage: the whole box.
    ///
    /// `canvas` is `BOX` rows of `BOX` BGRA pixels, rows packed (stride
    /// `BOX * 4` bytes): an shm buffer created with exactly that stride.
    ///
    /// # Panics
    /// If `canvas` is shorter than `BOX * BOX * 4` bytes (in debug builds,
    /// if its length differs).
    pub fn paint_full(&self, canvas: &mut [u8], hover: Option<u8>) -> Rect {
        debug_assert_eq!(canvas.len(), CANVAS_LEN, "canvas is BOX x BOX BGRA");
        let canvas = &mut canvas[..CANVAS_LEN];
        let lit = self.lit(hover);
        // Spans cover [C - row_end, C + row_end) of each row; clear the rest.
        for (y, row) in canvas.as_chunks_mut::<STRIDE>().0.iter_mut().enumerate() {
            let b = if y as u32 >= C {
                y as u32 - C
            } else {
                C - 1 - y as u32
            };
            let e = GEO.row_end[b as usize] as usize;
            row[..(C as usize - e) * 4].fill(0);
            row[(C as usize + e) * 4..].fill(0);
        }
        let px = canvas.as_chunks_mut::<4>().0;
        for (q, segs) in SEG_OF.iter().enumerate() {
            let luts = segs.map(|s| self.fill(s, lit));
            for b in 0..C {
                let h = GEO.hub_end[b as usize] as u32;
                Self::span(px, q, b, 0, h, &LUT_HUB);
                for (r, lut) in luts.iter().enumerate() {
                    let (a0, a1) = Self::run(r, b);
                    Self::span(px, q, b, a0, a1, lut);
                }
            }
        }
        for s in 0..SEGMENTS {
            self.draw_label(canvas, s, lit);
        }
        if let Some(l) = &self.hub {
            text::draw(canvas, STRIDE, l.x, l.y, &l.text, TEXT);
        }
        Rect {
            x: 0,
            y: 0,
            w: BOX,
            h: BOX,
        }
    }

    /// The offsets `a0..a1` of run `r` (`SEG_OF` order) in quadrant row `b`.
    #[inline(always)]
    fn run(r: usize, b: u32) -> (u32, u32) {
        let (h, e) = (
            GEO.hub_end[b as usize] as u32,
            GEO.row_end[b as usize] as u32,
        );
        let m0 = (GEO.split[0][b as usize] as u32).clamp(h, e);
        let m1 = (GEO.split[1][b as usize] as u32).clamp(h, e);
        match r {
            0 => (m1, e),
            1 => (m0, m1),
            _ => (h, m0),
        }
    }

    /// Repaint slice `s`: its runs (one quadrant for a diagonal slice, two
    /// for a compass one) and its label.
    fn paint_segment(&self, canvas: &mut [u8], s: u32, lit: Option<u8>) {
        let lut = self.fill(s, lit);
        let px = canvas.as_chunks_mut::<4>().0;
        for (q, segs) in SEG_OF.iter().enumerate() {
            for (r, _) in segs.iter().enumerate().filter(|&(_, &x)| x == s) {
                for b in 0..C {
                    let (a0, a1) = Self::run(r, b);
                    Self::span(px, q, b, a0, a1, lut);
                }
            }
        }
        self.draw_label(canvas, s, lit);
    }

    /// Repaint a canvas showing `old` to show `new`; only the two slices'
    /// pixels (and labels) change. Indexes outside 0..8 count as no slice.
    /// The canvas must be one `paint_full` or `paint_hover` of *this*
    /// `Wheel` left in state `old`: after a new `Wheel`, the next paint of
    /// each buffer is `paint_full`.
    ///
    /// The returned rectangle is what changed in *this canvas*. With more
    /// than one buffer the screen shows another buffer's state, so feed it
    /// to `damage::Screen` for the damage to report. Empty if neither is a
    /// slice. Canvas layout as in `paint_full`.
    ///
    /// # Panics
    /// As `paint_full`.
    pub fn paint_hover(&self, canvas: &mut [u8], old: Option<u8>, new: Option<u8>) -> Rect {
        debug_assert_eq!(canvas.len(), CANVAS_LEN, "canvas is BOX x BOX BGRA");
        let canvas = &mut canvas[..CANVAS_LEN];
        let lit = self.lit(new);
        let mut damage = Rect::EMPTY;
        let mut done = None;
        for s in [old, new].into_iter().flatten() {
            if u32::from(s) >= SEGMENTS || done == Some(s) {
                continue;
            }
            done = Some(s);
            self.paint_segment(canvas, u32::from(s), lit);
            damage = damage.union(GEO.bboxes[s as usize]);
        }
        damage
    }
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
    use crate::atlas;
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
        c.as_chunks::<4>().0.iter().enumerate().map(|(i, p)| {
            let i = i as u32;
            (i % BOX, i / BOX, *p)
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
        // Clockwise from slice 0's first border, at north minus 22.5°.
        let mut a = py.atan2(px) + FRAC_PI_2 + FRAC_PI_8;
        while a < 0.0 {
            a += TAU;
        }
        while a >= TAU {
            a -= TAU;
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
                let at = w.hit(x as f64 + 0.5, y as f64 + 0.5);
                assert!(
                    at == Hit::Slice(s) || !matches!(at, Hit::Slice(_)),
                    "pixel {x},{y}: {s} vs {at:?}"
                );
            }
            assert!(changed > 1000, "slice {s} barely highlights ({changed})");
        }
    }

    /// Slices sit on the compass: a click straight up hits slice 0 (north),
    /// straight right slice 2 (east), and so on clockwise; the diagonals
    /// hit their own slices, and a slice spans 22.5° either way.
    #[test]
    fn hit_follows_the_compass() {
        let w = bare();
        let c = BOX as f64 / 2.0;
        let d = 150.0 / std::f64::consts::SQRT_2;
        for (s, name, (dx, dy)) in [
            (0, "N", (0.0, -150.0)),
            (1, "NE", (d, -d)),
            (2, "E", (150.0, 0.0)),
            (3, "SE", (d, d)),
            (4, "S", (0.0, 150.0)),
            (5, "SW", (-d, d)),
            (6, "W", (-150.0, 0.0)),
            (7, "NW", (-d, -d)),
        ] {
            assert_eq!(w.hit(c + dx, c + dy), Hit::Slice(s), "{name}");
            // 20° either side of the slice's direction is still the slice.
            let mid = f64::from(s) * std::f64::consts::FRAC_PI_4 - std::f64::consts::FRAC_PI_2;
            for off in [-20f64, 20.0] {
                let a = mid + off.to_radians();
                let at = w.hit(c + 150.0 * a.cos(), c + 150.0 * a.sin());
                assert_eq!(at, Hit::Slice(s), "{name} {off}°");
            }
        }
    }

    #[test]
    fn hit_tells_the_wheel_from_outside() {
        let w = bare();
        let c = BOX as f64 / 2.0;
        assert_eq!(w.hit(c, c), Hit::Hub);
        assert_eq!(w.hit(c + 63.0, c), Hit::Hub);
        assert_eq!(w.hit(c, c - 63.0), Hit::Hub);
        // Inside the wheel but not on a slice is the hub: the transparent
        // annulus between hub and ring ...
        assert_eq!(w.hit(c + 70.0, c), Hit::Hub);
        assert_eq!(w.hit(c + 79.0, c), Hit::Hub);
        assert_eq!(w.hit(c + 64.5, c), Hit::Hub);
        // ... and the ring's edge, up to the outer radius (here mid-slice,
        // so the slice).
        assert_eq!(w.hit(c + 220.0, c), Hit::Slice(2));
        // Beyond the outer radius is outside.
        assert_eq!(w.hit(c + 220.5, c), Hit::Outside);
        assert_eq!(w.hit(c + 221.0, c), Hit::Outside);
        assert_eq!(w.hit(0.0, 0.0), Hit::Outside);
        // On a slice border (the gap) it is the hub; 3 px off it, a slice
        // is. The E/SE border runs at 22.5° below the horizontal; its normal
        // (pointing into SE) is (-sin, cos).
        let (s22, c22) = std::f64::consts::FRAC_PI_8.sin_cos();
        let on = (c + 150.0 * c22, c + 150.0 * s22);
        assert_eq!(w.hit(on.0, on.1), Hit::Hub);
        let off = |k: f64| w.hit(on.0 - k * s22, on.1 + k * c22);
        assert_eq!(off(1.9), Hit::Hub);
        assert_eq!(off(-1.9), Hit::Hub);
        assert_eq!(off(3.0), Hit::Slice(3));
        assert_eq!(off(-3.0), Hit::Slice(2));
        // The axes and diagonals are mid-slice now, not borders.
        let d = 150.0 / std::f64::consts::SQRT_2;
        assert_eq!(w.hit(c + d, c + d), Hit::Slice(3));
        assert_eq!(w.hit(c, c - 150.0), Hit::Slice(0));
        // Non-finite and absurd points are outside, never a panic.
        for v in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1e300,
            -1e300,
            f64::MAX,
        ] {
            assert_eq!(w.hit(v, c), Hit::Outside, "{v}");
            assert_eq!(w.hit(c, v), Hit::Outside, "{v}");
            assert_eq!(w.hit(v, v), Hit::Outside, "{v}");
        }
    }

    /// What `hit` says matches what the paint shows: a fully opaque pixel is
    /// its slice (or the hub), a fully transparent one is outside.
    #[test]
    fn hit_is_consistent_with_paint() {
        let w = bare();
        let c = full(&w, None);
        let (mut solid, mut clear) = (0, 0);
        for (x, y, p) in pixels(&c) {
            let (idx, _) = reference_pixel(x, y);
            let at = w.hit(x as f64 + 0.5, y as f64 + 0.5);
            let a = p[3];
            if a >= 215 {
                solid += 1;
                match idx {
                    None => assert_eq!(at, Hit::Hub, "{x},{y} alpha {a}"),
                    Some(k) => assert_eq!(at, Hit::Slice(k as u8), "{x},{y} alpha {a}"),
                }
            } else if a == 0 {
                clear += 1;
                // Transparent: the wheel's hub area up to the outer radius,
                // outside beyond it.
                let (dx, dy) = (x as f32 + 0.5 - C as f32, y as f32 + 0.5 - C as f32);
                let want = if dx * dx + dy * dy <= R_OUT * R_OUT {
                    Hit::Hub
                } else {
                    Hit::Outside
                };
                assert_eq!(at, want, "{x},{y}");
            }
        }
        assert!(solid > 100_000 && clear > 50_000, "{solid} {clear}");
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
        let r = w.paint_hover(&mut after, Some(3), Some(1));
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
        let mut st = Screen::new();
        // Burn in: both buffers attached once, nothing pending.
        st.commit(
            1,
            Rect {
                x: 0,
                y: 0,
                w: BOX,
                h: BOX,
            },
        );
        st.commit(0, Rect::EMPTY);
        st.commit(1, Rect::EMPTY);
        let r0 = w.paint_hover(&mut bufs[0], None, Some(0));
        let d0 = st.commit(0, r0);
        blit(&mut screen, &bufs[0], d0);
        let r1 = w.paint_hover(&mut bufs[1], None, Some(3));
        let mut naive = screen.clone();
        blit(&mut naive, &bufs[1], r1);
        assert!(naive != full(&w, Some(3)), "premise: naive damage is wrong");
        let d1 = st.commit(1, r1);
        blit(&mut screen, &bufs[1], d1);
        assert!(screen == full(&w, Some(3)));
    }

    /// A long random walk with buffer reuse: the screen always equals a full
    /// paint, and the damage is the frame's rect plus the other buffer's.
    #[test]
    fn long_walk_is_correct_and_damage_stays_tight() {
        use crate::damage::Screen;
        let w = sample();
        let mut bufs = [full(&w, None), full(&w, None)];
        let mut held: [Option<u8>; 2] = [None; 2];
        let mut screen = bufs[0].clone();
        let mut st = Screen::new();
        let (mut rng, mut i) = (0x2545_F491_4F6C_DD1Du64, 0usize);
        let full_box = Rect {
            x: 0,
            y: 0,
            w: BOX,
            h: BOX,
        };
        let (mut seen, mut last) = ([true, false], [full_box, Rect::EMPTY]);
        for step in 0..600 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let new = match rng % 11 {
                9 => Some(200), // out of range
                10 => None,
                k => Some((k % 8) as u8),
            };
            // About one step in six reuses the buffer the compositor may hold.
            i = if (rng >> 20) % 6 == 0 { i } else { 1 - i };
            let r = w.paint_hover(&mut bufs[i], held[i], new);
            held[i] = new.filter(|&s| s < 8);
            let d = st.commit(i, r);
            // The frame's own rect plus the other buffer's last one: no history.
            if seen[i] {
                assert_eq!(d, r.union(last[1 - i]), "step {step}: damage grew");
            }
            (seen[i], last[i]) = (true, r);
            blit(&mut screen, &bufs[i], d);
            assert!(screen == full(&w, held[i]), "step {step}: hover {new:?}");
        }
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
                    with.hit(x as f64 + 0.5, y as f64 + 0.5),
                    Hit::Slice(k),
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
            let a = s as f64 * std::f64::consts::FRAC_PI_4 - std::f64::consts::FRAC_PI_2;
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
                .filter(|&(x, y, _)| w.hit(x as f64 + 0.5, y as f64 + 0.5) == Hit::Slice(1))
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
        assert_eq!(w.hit(lx, ly), Hit::Outside); // the corner is outside the ring
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

    /// The compile-time tables are the runtime formula, evaluated at run time.
    #[test]
    fn const_tables_match_runtime_recomputation() {
        fn coverage(a: u32, b: u32) -> (bool, f32) {
            let (u, v) = (a as f32 + 0.5, b as f32 + 0.5);
            let r2 = u * u + v * v;
            let sq = |x: f32| x * x;
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
            let d = (u * S22 - v * C22).abs();
            (false, ring * (d - GAP + 0.5).clamp(0.0, 1.0))
        }
        let reach = ((R_OUT + 1.0).ceil() as u32).min(C);
        assert_eq!(reach as usize, REACH);
        assert!((S22 - std::f32::consts::FRAC_PI_8.sin()).abs() < 1e-7);
        assert!((C22 - std::f32::consts::FRAC_PI_8.cos()).abs() < 1e-7);
        let mut cov = vec![0u8; REACH * REACH];
        for a in 0..reach {
            for b in 0..=a {
                let (_, c) = coverage(a, b);
                let q = (c * 255.0 + 0.5) as u8;
                cov[b as usize * REACH + a as usize] = q;
                cov[a as usize * REACH + b as usize] = q;
            }
        }
        assert!(GEO.cov[..] == cov[..], "coverage table differs");
        let (hub_r2, out) = ((R_IN - 0.5) * (R_IN - 0.5), (R_OUT + 0.5) * (R_OUT + 0.5));
        let (mut h, mut e) = (C, C);
        for b in 0..C {
            let v = b as f32 + 0.5;
            let r2 = |a: u32| (a as f32 - 0.5) * (a as f32 - 0.5) + v * v;
            while h > 0 && r2(h) >= hub_r2 {
                h -= 1;
            }
            while e > 0 && r2(e) >= out {
                e -= 1;
            }
            assert_eq!(GEO.hub_end[b as usize] as u32, h, "hub_end[{b}]");
            assert_eq!(GEO.row_end[b as usize] as u32, e, "row_end[{b}]");
        }
        // The table is cut to the reach: nothing beyond it is ever read.
        assert!(GEO.row_end.iter().all(|&e| (e as usize) < REACH));
        // Splits: the first offset whose center is past each border.
        for b in 0..C {
            let v = b as f32 + 0.5;
            for (t, tan) in [S22 / C22, C22 / S22].into_iter().enumerate() {
                let want = (0..C).find(|&a| a as f32 + 0.5 >= v * tan).unwrap_or(C);
                assert_eq!(GEO.split[t][b as usize] as u32, want, "split[{t}][{b}]");
            }
        }
        // Boxes: every ring pixel of a slice (by the paint's runs) inside its
        // box, and the box no bigger than those pixels.
        let mut boxes = [Rect::EMPTY; 8];
        for (q, segs) in SEG_OF.iter().enumerate() {
            for b in 0..reach {
                for a in GEO.hub_end[b as usize] as u32..reach {
                    if cov[b as usize * REACH + a as usize] != 0 {
                        let s = segs[run_of(&GEO.split, a, b)] as usize;
                        let r = quadrant_rect(q, (a, a, b, b));
                        boxes[s] = boxes[s].union(r);
                    }
                }
            }
        }
        assert_eq!(GEO.bboxes, boxes);
        for (name, t, c) in [
            ("seg", &LUT_SEG, SEG),
            ("hov", &LUT_HOV, HOVER),
            ("dim", &LUT_DIM, DIM),
            ("hub", &LUT_HUB, HUB_C),
        ] {
            for (k, &got) in t.iter().enumerate() {
                let q = |x: f32| (x * k as f32 + 0.5) as u32;
                let want = q(c[2]) | q(c[1]) << 8 | q(c[0]) << 16 | q(c[3]) << 24;
                assert_eq!(got, want, "lut {name}[{k}]");
            }
        }
    }

    /// The label room is a constant by symmetry: three values cover all
    /// eight slices, equal to a search with real trigonometry.
    #[test]
    fn label_room_consts_match_a_runtime_search_in_every_slice() {
        assert_eq!(LABEL_H, atlas::get().line_height());
        for k in 0..8u32 {
            let step = TAU / 8.0;
            let mid = k as f32 * step - FRAC_PI_2;
            let (cx, cy) = (R_MID * mid.cos(), R_MID * mid.sin());
            let (s0, c0) = (mid - step / 2.0).sin_cos();
            let (s1, c1) = (mid + step / 2.0).sin_cos();
            let clear = GAP + LABEL_MARGIN;
            let fits = |w: f32| {
                let (hw, hh) = (w / 2.0, LABEL_H as f32 / 2.0);
                [
                    (-hw, -hh),
                    (hw, -hh),
                    (-hw, hh),
                    (hw, hh),
                    (0.0, -hh),
                    (0.0, hh),
                    (-hw, 0.0),
                    (hw, 0.0),
                ]
                .iter()
                .all(|&(dx, dy)| {
                    let (px, py) = (cx + dx, cy + dy);
                    let r = (px * px + py * py).sqrt();
                    (R_IN + LABEL_MARGIN..=R_OUT - LABEL_MARGIN).contains(&r)
                        && c0 * py - s0 * px >= clear
                        && px * s1 - py * c1 >= clear
                })
            };
            let want = (0..=R_OUT as u32).rev().find(|&w| fits(w as f32)).unwrap();
            assert_eq!(LABEL_W[label_class(k)], want, "slice {k}");
        }
    }

    /// A reused canvas holding anything is repainted to exactly a fresh one.
    #[test]
    fn paint_full_clears_what_the_spans_do_not_cover() {
        let w = sample();
        let fresh = full(&w, Some(0));
        let mut dirty = vec![0xAAu8; SIZE];
        w.paint_full(&mut dirty, Some(0));
        assert!(dirty == fresh);
        // Also over a previous, different picture.
        let mut other = full(&w, Some(5));
        w.paint_full(&mut other, Some(0));
        assert!(other == fresh);
    }

    #[test]
    #[should_panic]
    fn paint_full_rejects_a_short_canvas() {
        sample().paint_full(&mut vec![0u8; SIZE - 4], None);
    }

    #[test]
    #[should_panic]
    fn paint_hover_rejects_a_short_canvas() {
        sample().paint_hover(&mut vec![0u8; SIZE - 4], None, Some(0));
    }

    #[test]
    fn rect_math_saturates() {
        let big = Rect {
            x: u32::MAX - 2,
            y: 0,
            w: 10,
            h: 1,
        };
        let one = Rect {
            x: 5,
            y: 0,
            w: 1,
            h: 1,
        };
        let u = big.union(one); // must not overflow-panic
        assert_eq!(u.x, 5);
        assert!(!big.contains(one));
        assert!(big.contains(Rect {
            x: u32::MAX - 2,
            y: 0,
            w: 1,
            h: 1
        }));
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
        let med_of = |mut v: Vec<u128>| {
            v.sort_unstable();
            v[v.len() / 2]
        };
        let new_us = med_of(
            (0..50)
                .map(|_| {
                    let t = std::time::Instant::now();
                    std::hint::black_box(sample());
                    t.elapsed().as_nanos()
                })
                .collect(),
        );
        let w2 = sample();
        // Fresh: a new zeroed buffer each time (page faults on first touch,
        // unless the allocator recycles the previous one).
        let mut first = 0;
        let fresh_us = med_of(
            (0..50)
                .map(|_| {
                    let mut c = vec![0u8; SIZE];
                    let t = std::time::Instant::now();
                    w2.paint_full(&mut c, None);
                    let e = t.elapsed().as_nanos();
                    std::hint::black_box(c);
                    if first == 0 {
                        first = e;
                    }
                    e
                })
                .collect(),
        );
        let mut warm = vec![0u8; SIZE];
        let warm_us = med_of(
            (0..200)
                .map(|_| {
                    let t = std::time::Instant::now();
                    w2.paint_full(&mut warm, None);
                    t.elapsed().as_nanos()
                })
                .collect(),
        );
        println!(
            "Wheel::new {} µs, paint_full fresh {} µs (very first: {} µs), warm {} µs (medians)",
            new_us / 1000,
            fresh_us / 1000,
            first / 1000,
            warm_us / 1000
        );
        println!(
            "static data: cov {} B, row extents {} B, luts {} B",
            std::mem::size_of_val(&GEO.cov),
            std::mem::size_of_val(&GEO.hub_end) * 2,
            std::mem::size_of_val(&LUT_SEG) * 4
        );
        assert!(med <= 100_000, "median {med} ns over the 100 µs budget");
    }
}
