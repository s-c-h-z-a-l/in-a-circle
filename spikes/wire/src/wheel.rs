//! The cpu-sdf wheel rasterizer (spikes/render-bench/cpu-sdf), without its
//! wayland-client glue: coverage mask built once, then a hover change only
//! rewrites the two affected segments' pixels.

use std::f32::consts::TAU;

/// Side of the wheel's square buffer, in pixels.
pub const BOX: u32 = 480;
pub const SEGMENTS: u32 = 8;
const R_OUT: f32 = 220.0;
const R_IN: f32 = 80.0;
const R_HUB: f32 = 64.0;
const GAP: f32 = 2.0;

/// Premultiplied RGBA.
const SEG: [f32; 4] = premul([0.15, 0.15, 0.18], 0.85);
const HOVER: [f32; 4] = premul([0.95, 0.75, 0.20], 0.95);
const HUB_C: [f32; 4] = premul([0.08, 0.08, 0.10], 0.90);

const fn premul(c: [f32; 3], a: f32) -> [f32; 4] {
    [c[0] * a, c[1] * a, c[2] * a, a]
}

/// Mask index for hub/empty pixels.
const HUB: u8 = u8::MAX;
/// Hover value for "no segment".
pub const NONE: u32 = u32::MAX;

#[derive(Clone, Copy)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Bounding box of segment `i` inside the wheel box, padded for AA.
fn seg_bbox(i: u32) -> Rect {
    let c = BOX as f32 / 2.0;
    let seg = TAU / SEGMENTS as f32;
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for s in 0..=64 {
        let a = (i as f32 + s as f32 / 64.0) * seg;
        for r in [R_IN, R_OUT] {
            let (x, y) = (c + r * a.cos(), c + r * a.sin());
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
    }
    let x0 = (x0 - 2.0).max(0.0) as u32;
    let y0 = (y0 - 2.0).max(0.0) as u32;
    let x1 = ((x1 + 2.0).ceil() as u32).min(BOX);
    let y1 = ((y1 + 2.0).ceil() as u32).min(BOX);
    Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    }
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
/// outside the ring). Same angle convention as the mask.
pub fn segment_at(x: f64, y: f64) -> u32 {
    let c = BOX as f32 / 2.0;
    let (px, py) = (x as f32 - c, y as f32 - c);
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

/// Segment index (or HUB) and 8-bit coverage of pixel (x, y) in the box.
#[inline(always)]
fn classify(x: u32, y: u32) -> (u8, u8) {
    let c = BOX as f32 / 2.0;
    let (px, py) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
    let r = (px * px + py * py).sqrt();
    let q = |k: f32| (k * 255.0 + 0.5) as u8;
    if r < R_IN - 0.5 {
        return (HUB, q((R_HUB - r + 0.5).clamp(0.0, 1.0)));
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
    (idx as u8, q(ring * gap))
}

/// Horizontal span of row `y` that intersects the outer circle (+1 px AA).
#[inline(always)]
fn row_span(y: u32) -> Option<(u32, u32)> {
    let c = BOX as f32 / 2.0;
    let dy = y as f32 + 0.5 - c;
    let r = R_OUT + 1.0;
    let h = r * r - dy * dy;
    if h <= 0.0 {
        return None;
    }
    let dx = h.sqrt();
    Some((
        ((c - dx).floor().max(0.0)) as u32,
        ((c + dx).ceil() as u32).min(BOX),
    ))
}

/// coverage -> premultiplied ARGB8888 (little-endian BGRA) for one color.
fn lut(c: [f32; 4]) -> Box<[u32; 256]> {
    let mut t = Box::new([0u32; 256]);
    for (k, v) in t.iter_mut().enumerate() {
        let q = |x: f32| (x * k as f32 + 0.5) as u32;
        *v = q(c[2]) | q(c[1]) << 8 | q(c[0]) << 16 | q(c[3]) << 24;
    }
    t
}

pub struct Wheel {
    bboxes: Vec<Rect>,
    /// Per box pixel: (segment index << 8) | coverage.
    mask: Vec<u16>,
    lut_seg: Box<[u32; 256]>,
    lut_hov: Box<[u32; 256]>,
    lut_hub: Box<[u32; 256]>,
}

impl Wheel {
    pub fn new() -> Wheel {
        let mut mask = vec![(HUB as u16) << 8; (BOX * BOX) as usize];
        for y in 0..BOX {
            let Some((x0, x1)) = row_span(y) else {
                continue;
            };
            for x in x0..x1 {
                let (idx, cov) = classify(x, y);
                mask[(y * BOX + x) as usize] = (idx as u16) << 8 | cov as u16;
            }
        }
        Wheel {
            bboxes: (0..SEGMENTS).map(seg_bbox).collect(),
            mask,
            lut_seg: lut(SEG),
            lut_hov: lut(HOVER),
            lut_hub: lut(HUB_C),
        }
    }

    #[inline(always)]
    fn color(&self, m: u16, hover: u32) -> u32 {
        let (idx, cov) = ((m >> 8) as u8, (m & 0xFF) as usize);
        if idx == HUB {
            self.lut_hub[cov]
        } else if idx as u32 == hover {
            self.lut_hov[cov]
        } else {
            self.lut_seg[cov]
        }
    }

    /// Paint the whole wheel into a BOX x BOX canvas. Returns the damage.
    pub fn paint_full(&self, canvas: &mut [u8], hover: u32) -> Rect {
        let all = Rect {
            x: 0,
            y: 0,
            w: BOX,
            h: BOX,
        };
        self.paint(canvas, all, hover, None);
        all
    }

    /// Repaint a canvas showing `old` to show `new`; only the two segments'
    /// pixels change. Returns the damaged rects.
    pub fn paint_hover(&self, canvas: &mut [u8], old: u32, new: u32) -> Vec<Rect> {
        // 254 matches no mask index (segments are 0..8, hub is 255).
        let id = |s: u32| if s == NONE { 254 } else { s as u8 };
        let only = Some([id(old), id(new)]);
        let mut damage = Vec::with_capacity(2);
        for s in [old, new] {
            if s != NONE {
                let r = self.bboxes[s as usize];
                self.paint(canvas, r, new, only);
                damage.push(r);
            }
        }
        damage
    }

    fn paint(&self, canvas: &mut [u8], r: Rect, hover: u32, only: Option<[u8; 2]>) {
        for y in r.y..r.y + r.h {
            let Some((sx0, sx1)) = row_span(y) else {
                continue;
            };
            let (x0, x1) = (sx0.max(r.x), sx1.min(r.x + r.w));
            if x0 >= x1 {
                continue;
            }
            let row = (y * BOX + x0) as usize * 4;
            let dst = &mut canvas[row..row + (x1 - x0) as usize * 4];
            let src = &self.mask[(y * BOX + x0) as usize..(y * BOX + x1) as usize];
            for (px, &m) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src) {
                let idx = (m >> 8) as u8;
                if only.is_none_or(|o| idx == o[0] || idx == o[1]) {
                    *px = self.color(m, hover).to_le_bytes();
                }
            }
        }
    }
}
