//! CPU backend, no rasterizer dependency: the wheel is shaded analytically per
//! pixel (same math as the GPU shader), straight into the wl_shm buffer.
//!
//! First paint computes a mask (segment index + coverage per pixel). After
//! that a hover change is a palette lookup over the two affected segments'
//! pixels only; each buffer remembers which segment it shows highlighted.

use std::f32::consts::TAU;
use std::time::Instant;

use common::shm::slot::{Buffer, SlotPool};
use common::wheel::{self, BOX, GAP, R_HUB, R_IN, R_OUT, SEGMENTS};
use common::{Ctx, Frame, Renderer};
use wayland_client::protocol::{wl_shm, wl_surface::WlSurface};

/// Mask index for hub/empty pixels.
const HUB: u8 = u8::MAX;

#[derive(Clone, Copy)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
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

/// atan2 via a minimax polynomial on [0, 1]; max error ~1e-5 rad
/// (0.002 px at R_OUT). libm atan2 dominated the per-pixel cost.
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
        let q = |x: f32| (x * k as f32 + 0.5) as u32; // x*255 * k/255
        *v = q(c[2]) | q(c[1]) << 8 | q(c[0]) << 16 | q(c[3]) << 24;
    }
    t
}

struct Slot {
    buf: Buffer,
    /// Hover state currently painted in this buffer; None = never painted.
    hover: Option<u32>,
}

struct CpuSdf {
    bboxes: Vec<Rect>,
    /// Per box pixel: (segment index << 8) | coverage. Built on first paint.
    mask: Vec<u16>,
    lut_seg: Box<[u32; 256]>,
    lut_hov: Box<[u32; 256]>,
    lut_hub: Box<[u32; 256]>,
    pool: Option<SlotPool>,
    slots: Vec<Slot>,
    size: (u32, u32),
    frames: u32,
    full_us: f64,
    partial_us: f64,
}

impl CpuSdf {
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

    fn build_mask(&mut self) {
        self.mask = vec![(HUB as u16) << 8; (BOX * BOX) as usize];
        for y in 0..BOX {
            let Some((x0, x1)) = row_span(y) else {
                continue;
            };
            for x in x0..x1 {
                let (idx, cov) = classify(x, y);
                self.mask[(y * BOX + x) as usize] = (idx as u16) << 8 | cov as u16;
            }
        }
    }

    /// Write rect `r` (box coords). With `only`, write just those segments'
    /// pixels: everything else in the rect does not depend on hover.
    fn paint(
        &self,
        canvas: &mut [u8],
        w: u32,
        o: (u32, u32),
        r: Rect,
        hover: u32,
        only: Option<[u8; 2]>,
    ) {
        for y in r.y..r.y + r.h {
            let Some((sx0, sx1)) = row_span(y) else {
                continue;
            };
            let (x0, x1) = (sx0.max(r.x), sx1.min(r.x + r.w));
            if x0 >= x1 {
                continue;
            }
            let row = ((o.1 + y) * w + o.0 + x0) as usize * 4;
            let dst = &mut canvas[row..row + (x1 - x0) as usize * 4];
            let src = &self.mask[(y * BOX + x0) as usize..(y * BOX + x1) as usize];
            for (px, &m) in dst.chunks_exact_mut(4).zip(src) {
                let idx = (m >> 8) as u8;
                if only.is_none_or(|o| idx == o[0] || idx == o[1]) {
                    px.copy_from_slice(&self.color(m, hover).to_le_bytes());
                }
            }
        }
    }
}

impl CpuSdf {
    fn init_offline() -> Self {
        CpuSdf {
            bboxes: (0..SEGMENTS).map(seg_bbox).collect(),
            mask: Vec::new(),
            lut_seg: lut(wheel::SEG),
            lut_hov: lut(wheel::HOVER),
            lut_hub: lut(wheel::HUB),
            pool: None,
            slots: Vec::new(),
            size: (0, 0),
            frames: 0,
            full_us: 0.0,
            partial_us: 0.0,
        }
    }
}

impl Renderer for CpuSdf {
    fn init(_: &Ctx) -> Self {
        Self::init_offline()
    }

    fn attach(&mut self, cx: &Ctx, _: &WlSurface, width: u32, height: u32) {
        self.size = (width, height);
        self.pool = Some(SlotPool::new((width * height * 4 * 2) as usize, cx.shm).unwrap());
        self.frames = 0;
        self.partial_us = 0.0;
    }

    fn draw(&mut self, surface: &WlSurface, f: &Frame) {
        let t = Instant::now();
        if self.mask.is_empty() {
            self.build_mask();
        }
        let mut pool = self.pool.take().unwrap();
        let (w, h) = self.size;
        let idx = match self
            .slots
            .iter()
            .position(|s| s.buf.canvas(&mut pool).is_some())
        {
            Some(i) => i,
            None => {
                // Fresh memfd pages are already zero; touching them would
                // fault them all in (29 MB at full screen).
                let (buf, _) = pool
                    .create_buffer(w as i32, h as i32, w as i32 * 4, wl_shm::Format::Argb8888)
                    .unwrap();
                self.slots.push(Slot { buf, hover: None });
                self.slots.len() - 1
            }
        };
        let prev = self.slots[idx].hover;
        let canvas = self.slots[idx].buf.canvas(&mut pool).unwrap();
        let o = wheel::origin(w, h);
        match prev {
            None => {
                let all = Rect {
                    x: 0,
                    y: 0,
                    w: BOX,
                    h: BOX,
                };
                self.paint(canvas, w, o, all, f.hover, None);
                surface.damage_buffer(o.0 as i32, o.1 as i32, BOX as i32, BOX as i32);
                self.full_us = t.elapsed().as_secs_f64() * 1e6;
            }
            Some(old) if old != f.hover => {
                for s in [old, f.hover] {
                    let r = self.bboxes[s as usize];
                    self.paint(canvas, w, o, r, f.hover, Some([old as u8, f.hover as u8]));
                    surface.damage_buffer(
                        (o.0 + r.x) as i32,
                        (o.1 + r.y) as i32,
                        r.w as i32,
                        r.h as i32,
                    );
                }
                self.partial_us += t.elapsed().as_secs_f64() * 1e6;
                self.frames += 1;
            }
            Some(_) => {}
        }
        let slot = &mut self.slots[idx];
        slot.hover = Some(f.hover);
        slot.buf.attach_to(surface).unwrap();
        surface.commit();
        self.pool = Some(pool);
    }

    fn detach(&mut self) {
        println!(
            "M sdf full_paint_us={:.1} partial_paint_avg_us={:.1} partial_frames={}",
            self.full_us,
            self.partial_us / self.frames.max(1) as f64,
            self.frames
        );
        self.slots.clear();
        self.pool = None;
        // Spawn-per-use would exit here; for the daemon case free it.
        self.mask = Vec::new();
    }
}

fn main() {
    common::run::<CpuSdf>("cpu-sdf");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Transform};

    /// Independent reference: tiny-skia paths, same geometry as the `cpu` spike.
    fn reference(hover: u32) -> Vec<u8> {
        let c = BOX as f32 / 2.0;
        let seg = TAU / SEGMENTS as f32;
        let mut pm = Pixmap::new(BOX, BOX).unwrap();
        let paint = |col: [f32; 4]| {
            let a = col[3];
            let mut p = Paint::default();
            p.set_color(Color::from_rgba(col[0] / a, col[1] / a, col[2] / a, a).unwrap());
            p
        };
        for i in 0..SEGMENTS {
            let (a0, a1) = (i as f32 * seg, (i + 1) as f32 * seg);
            let mut pb = PathBuilder::new();
            let n = 256;
            for s in 0..=n {
                let a = a0 + GAP / R_OUT + (a1 - a0 - 2.0 * GAP / R_OUT) * s as f32 / n as f32;
                let (x, y) = (c + R_OUT * a.cos(), c + R_OUT * a.sin());
                if s == 0 {
                    pb.move_to(x, y)
                } else {
                    pb.line_to(x, y)
                }
            }
            for s in (0..=n).rev() {
                let a = a0 + GAP / R_IN + (a1 - a0 - 2.0 * GAP / R_IN) * s as f32 / n as f32;
                pb.line_to(c + R_IN * a.cos(), c + R_IN * a.sin());
            }
            pb.close();
            let col = if i == hover { wheel::HOVER } else { wheel::SEG };
            pm.fill_path(
                &pb.finish().unwrap(),
                &paint(col),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        let hub = PathBuilder::from_circle(c, c, R_HUB).unwrap();
        pm.fill_path(
            &hub,
            &paint(wheel::HUB),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
        pm.data().to_vec() // RGBA premul
    }

    fn ours(hover: u32, via_partial_from: Option<u32>) -> Vec<u8> {
        let mut r = CpuSdf::init_offline();
        r.build_mask();
        let mut canvas = vec![0u8; (BOX * BOX * 4) as usize];
        let all = Rect {
            x: 0,
            y: 0,
            w: BOX,
            h: BOX,
        };
        match via_partial_from {
            None => r.paint(&mut canvas, BOX, (0, 0), all, hover, None),
            Some(old) => {
                r.paint(&mut canvas, BOX, (0, 0), all, old, None);
                for s in [old, hover] {
                    r.paint(
                        &mut canvas,
                        BOX,
                        (0, 0),
                        r.bboxes[s as usize],
                        hover,
                        Some([old as u8, hover as u8]),
                    );
                }
            }
        }
        // BGRA -> RGBA
        canvas.chunks_exact_mut(4).for_each(|p| p.swap(0, 2));
        canvas
    }

    fn diff(a: &[u8], b: &[u8]) -> (f64, u8, usize) {
        let mut sum = 0u64;
        let mut max = 0u8;
        let mut big = 0;
        for (x, y) in a.iter().zip(b) {
            let d = x.abs_diff(*y);
            sum += d as u64;
            max = max.max(d);
            if d > 24 {
                big += 1;
            }
        }
        (sum as f64 / a.len() as f64, max, big)
    }

    #[test]
    fn matches_reference_render() {
        for hover in [0, 3, 7] {
            let (mean, max, big) = diff(&ours(hover, None), &reference(hover));
            println!("hover={hover} mean_abs={mean:.3} max={max} px_over_24={big}");
            assert!(mean < 0.5, "mean diff {mean}");
            assert!(big < 200, "{big} channels differ > 24");
        }
    }

    #[test]
    fn partial_repaint_equals_full_paint() {
        for (old, new) in [(0, 1), (2, 6), (7, 0)] {
            assert_eq!(ours(new, Some(old)), ours(new, None), "{old}->{new}");
        }
    }
}
