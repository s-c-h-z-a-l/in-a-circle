//! CPU backend: tiny-skia rasterizes the wheel box, copied into a wl_shm buffer.

use std::f32::consts::TAU;

use common::shm::slot::{Buffer, SlotPool};
use common::wheel::{self, BOX, GAP, R_HUB, R_IN, R_OUT, SEGMENTS};
use common::{Ctx, Frame, Renderer};
use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, Pixmap, Transform};
use wayland_client::protocol::{wl_shm, wl_surface::WlSurface};

const ARC_STEPS: u32 = 24;

struct Cpu {
    segments: Vec<Path>,
    hub: Path,
    pixmap: Pixmap,
    pool: Option<SlotPool>,
    buffers: Vec<Buffer>,
    size: (u32, u32),
}

fn paint(c: [f32; 4]) -> Paint<'static> {
    let a = c[3];
    let mut p = Paint::default();
    p.set_color(Color::from_rgba(c[0] / a, c[1] / a, c[2] / a, a).unwrap());
    p.anti_alias = true;
    p
}

fn sector(i: u32) -> Path {
    let c = BOX as f32 / 2.0;
    let seg = TAU / SEGMENTS as f32;
    let (a0, a1) = (i as f32 * seg, (i + 1) as f32 * seg);
    let mut pb = PathBuilder::new();
    let (o0, o1) = (a0 + GAP / R_OUT, a1 - GAP / R_OUT);
    let (i0, i1) = (a0 + GAP / R_IN, a1 - GAP / R_IN);
    for s in 0..=ARC_STEPS {
        let a = o0 + (o1 - o0) * s as f32 / ARC_STEPS as f32;
        let (x, y) = (c + R_OUT * a.cos(), c + R_OUT * a.sin());
        if s == 0 {
            pb.move_to(x, y)
        } else {
            pb.line_to(x, y)
        }
    }
    for s in (0..=ARC_STEPS).rev() {
        let a = i0 + (i1 - i0) * s as f32 / ARC_STEPS as f32;
        pb.line_to(c + R_IN * a.cos(), c + R_IN * a.sin());
    }
    pb.close();
    pb.finish().unwrap()
}

impl Renderer for Cpu {
    fn init(_: &Ctx) -> Self {
        let c = BOX as f32 / 2.0;
        Cpu {
            segments: (0..SEGMENTS).map(sector).collect(),
            hub: PathBuilder::from_circle(c, c, R_HUB).unwrap(),
            pixmap: Pixmap::new(BOX, BOX).unwrap(),
            pool: None,
            buffers: Vec::new(),
            size: (0, 0),
        }
    }

    fn attach(&mut self, cx: &Ctx, _: &WlSurface, width: u32, height: u32) {
        self.size = (width, height);
        // Room for two buffers (double buffering).
        self.pool = Some(SlotPool::new((width * height * 4 * 2) as usize, cx.shm).unwrap());
    }

    fn draw(&mut self, surface: &WlSurface, f: &Frame) {
        // Rasterize only the wheel box.
        let pm = &mut self.pixmap;
        pm.fill(Color::TRANSPARENT);
        let seg = paint(wheel::SEG);
        let hov = paint(wheel::HOVER);
        for (i, p) in self.segments.iter().enumerate() {
            let pt = if i as u32 == f.hover { &hov } else { &seg };
            pm.fill_path(p, pt, FillRule::Winding, Transform::identity(), None);
        }
        pm.fill_path(
            &self.hub,
            &paint(wheel::HUB),
            FillRule::Winding,
            Transform::identity(),
            None,
        );

        // Pick a buffer the compositor released, or allocate one (max 2).
        let pool = self.pool.as_mut().unwrap();
        let (w, h) = self.size;
        let stride = w as i32 * 4;
        let idx = match self.buffers.iter().position(|b| b.canvas(pool).is_some()) {
            Some(i) => i,
            None => {
                let (b, canvas) = pool
                    .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
                    .unwrap();
                canvas.fill(0);
                self.buffers.push(b);
                self.buffers.len() - 1
            }
        };
        let buf = &self.buffers[idx];
        let canvas = buf.canvas(pool).unwrap();

        // Copy box into place, RGBA (premul) -> BGRA (wl ARGB8888 little endian).
        let (ox, oy) = wheel::origin(w, h);
        let src = pm.data();
        let row = BOX as usize * 4;
        for y in 0..BOX.min(h) as usize {
            let d0 = ((oy as usize + y) * w as usize + ox as usize) * 4;
            let dst = &mut canvas[d0..d0 + row];
            let s = &src[y * row..(y + 1) * row];
            for (d, s) in dst
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(s.as_chunks::<4>().0)
            {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = s[3];
            }
        }

        surface.damage_buffer(ox as i32, oy as i32, BOX as i32, BOX as i32);
        buf.attach_to(surface).unwrap();
        surface.commit();
    }

    fn detach(&mut self) {
        self.buffers.clear();
        self.pool = None;
    }
}

fn main() {
    common::run::<Cpu>("cpu");
}
