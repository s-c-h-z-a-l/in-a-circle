//! Text quality spike: pre-baked alpha atlas at exact sizes (the reference)
//! vs one SDF atlas scaled to each size. Reports quality vs reference, atlas
//! bytes, label render time, and writes a comparison PNG.
//!
//! Usage: font <font.ttf> <out-dir>

use std::time::Instant;

const SIZES: [u32; 6] = [11, 13, 15, 18, 24, 32];
const SAMPLE: &str = "Fordítás  Átméretezés  Copy URL  QR kód  ÁRVÍZTŰRŐ tükörfúrógép 0123";
const LABEL: &str = "Átméretezés";

fn charset() -> Vec<char> {
    let mut v: Vec<char> = (32u8..127).map(char::from).collect();
    v.extend("áéíóöőúüűÁÉÍÓÖŐÚÜŰ".chars());
    v
}

/// A glyph bitmap with placement in its own pixel units.
struct Glyph {
    ch: char,
    w: usize,
    h: usize,
    /// Left edge relative to the pen. `oy`: alpha = bottom edge, SDF = top
    /// edge, relative to the baseline (y up).
    ox: f32,
    oy: f32,
    adv: f32,
    data: Vec<u8>,
}

struct Atlas {
    /// Size (px) the glyph units are expressed in.
    base: f32,
    /// SDF spread in base px; 0 = plain alpha.
    spread: f32,
    glyphs: Vec<Glyph>,
}

impl Atlas {
    fn bytes(&self) -> usize {
        // bitmap + 4 x f32 metrics + w/h as u16
        self.glyphs.iter().map(|g| g.data.len() + 16 + 4).sum()
    }
    fn get(&self, c: char) -> &Glyph {
        self.glyphs
            .iter()
            .find(|g| g.ch == c)
            .unwrap_or(&self.glyphs[0])
    }
}

fn bake_alpha(font: &fontdue::Font, px: f32) -> Atlas {
    let glyphs = charset()
        .into_iter()
        .map(|ch| {
            let (m, data) = font.rasterize(ch, px);
            Glyph {
                ch,
                w: m.width,
                h: m.height,
                ox: m.xmin as f32,
                oy: m.ymin as f32,
                adv: m.advance_width,
                data,
            }
        })
        .collect();
    Atlas {
        base: px,
        spread: 0.0,
        glyphs,
    }
}

/// SDF from an `over`x oversampled raster; brute-force nearest opposite pixel.
fn bake_sdf(font: &fontdue::Font, base: f32, spread: f32, over: f32) -> Atlas {
    let glyphs = charset()
        .into_iter()
        .map(|ch| {
            let (m, hi) = font.rasterize(ch, base * over);
            let am = font.metrics(ch, base);
            let pad = spread.ceil() as usize;
            let w = (m.width as f32 / over).ceil() as usize + 2 * pad;
            let h = (m.height as f32 / over).ceil() as usize + 2 * pad;
            let inside = |x: i32, y: i32| {
                x >= 0
                    && y >= 0
                    && (x as usize) < m.width
                    && (y as usize) < m.height
                    && hi[y as usize * m.width + x as usize] >= 128
            };
            let r = (spread * over).ceil() as i32 + 1;
            let mut data = vec![0u8; w * h];
            for ty in 0..h {
                for tx in 0..w {
                    // texel center in hi-res bitmap coords (y down)
                    let cx = ((tx as f32 - pad as f32 + 0.5) * over) as i32;
                    let cy = ((ty as f32 - pad as f32 + 0.5) * over) as i32;
                    let ins = inside(cx, cy);
                    let mut best = (r * r) as f32;
                    for dy in -r..=r {
                        for dx in -r..=r {
                            let d2 = (dx * dx + dy * dy) as f32;
                            if d2 < best && inside(cx + dx, cy + dy) != ins {
                                best = d2;
                            }
                        }
                    }
                    let d = (best.sqrt() - 0.5).max(0.0) / over; // base px to the edge
                    let sd = if ins { d } else { -d };
                    let v = 0.5 + sd / (2.0 * spread);
                    data[ty * w + tx] = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                }
            }
            Glyph {
                ch,
                w,
                h,
                ox: m.xmin as f32 / over - pad as f32,
                // Texel row 0 sits `pad` above the hi-res bitmap's top edge.
                oy: (m.ymin as f32 + m.height as f32) / over + pad as f32,
                adv: am.advance_width,
                data,
            }
        })
        .collect();
    Atlas {
        base,
        spread,
        glyphs,
    }
}

/// Coverage canvas (0..1), one row of text.
struct Canvas {
    w: usize,
    h: usize,
    px: Vec<f32>,
}

fn draw(atlas: &Atlas, text: &str, size: f32, c: &mut Canvas, x0: f32, baseline: f32) {
    let k = size / atlas.base;
    let mut pen = x0;
    for ch in text.chars() {
        let g = atlas.get(ch);
        let pen_r = pen.round();
        if atlas.spread == 0.0 {
            let gx = pen_r as i32 + g.ox as i32;
            let gy = (baseline - g.oy) as i32 - g.h as i32;
            for y in 0..g.h {
                for x in 0..g.w {
                    let (cx, cy) = (gx + x as i32, gy + y as i32);
                    if cx >= 0 && cy >= 0 && (cx as usize) < c.w && (cy as usize) < c.h {
                        let p = &mut c.px[cy as usize * c.w + cx as usize];
                        *p = p.max(g.data[y * g.w + x] as f32 / 255.0);
                    }
                }
            }
        } else {
            let qx = pen_r + g.ox * k;
            let qy = baseline - g.oy * k;
            let (qw, qh) = (g.w as f32 * k, g.h as f32 * k);
            for cy in qy.floor().max(0.0) as usize..((qy + qh).ceil() as usize).min(c.h) {
                for cx in qx.floor().max(0.0) as usize..((qx + qw).ceil() as usize).min(c.w) {
                    // bilinear sample at the texel space position
                    let u = (cx as f32 + 0.5 - qx) / k - 0.5;
                    let v = (cy as f32 + 0.5 - qy) / k - 0.5;
                    let (x0, y0) = (u.floor(), v.floor());
                    let (fx, fy) = (u - x0, v - y0);
                    let s = |x: f32, y: f32| {
                        if x < 0.0 || y < 0.0 || x >= g.w as f32 || y >= g.h as f32 {
                            0.0
                        } else {
                            g.data[y as usize * g.w + x as usize] as f32 / 255.0
                        }
                    };
                    let val = (s(x0, y0) * (1.0 - fx) + s(x0 + 1.0, y0) * fx) * (1.0 - fy)
                        + (s(x0, y0 + 1.0) * (1.0 - fx) + s(x0 + 1.0, y0 + 1.0) * fx) * fy;
                    let dist = (val - 0.5) * 2.0 * atlas.spread * k; // target px
                    let a = (dist + 0.5).clamp(0.0, 1.0);
                    let p = &mut c.px[cy * c.w + cx];
                    *p = p.max(a);
                }
            }
        }
        pen += g.adv * k;
    }
}

fn write_png(path: &str, w: usize, h: usize, rgb: &[u8]) {
    let f = std::fs::File::create(path).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
    e.set_color(png::ColorType::Rgb);
    e.set_depth(png::BitDepth::Eight);
    e.write_header().unwrap().write_image_data(rgb).unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let font = fontdue::Font::from_bytes(
        std::fs::read(&args[1]).unwrap(),
        fontdue::FontSettings::default(),
    )
    .unwrap();
    let out = &args[2];

    let t = Instant::now();
    let alpha: Vec<Atlas> = SIZES.iter().map(|&s| bake_alpha(&font, s as f32)).collect();
    println!(
        "bake alpha (all sizes): {:.0} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
    let sdf_cfgs = [(16.0, 2.0), (24.0, 3.0), (32.0, 4.0)];
    let sdfs: Vec<Atlas> = sdf_cfgs
        .iter()
        .map(|&(b, s)| {
            let t = Instant::now();
            let a = bake_sdf(&font, b, s, 8.0);
            println!(
                "bake sdf base={b} spread={s}: {:.0} ms",
                t.elapsed().as_secs_f64() * 1e3
            );
            a
        })
        .collect();

    println!("\n## Atlas bytes ({} glyphs)", charset().len());
    for (s, a) in SIZES.iter().zip(&alpha) {
        println!("alpha {s:>2}px: {:>6} B", a.bytes());
    }
    for a in &sdfs {
        println!(
            "sdf base {:>2} spread {}: {:>6} B (all sizes)",
            a.base,
            a.spread,
            a.bytes()
        );
    }

    println!("\n## Quality vs exact alpha (mean abs diff /255, % pixels off by >32/255)");
    let width = 720;
    let mut image_rows: Vec<(String, Canvas)> = Vec::new();
    for (si, &size) in SIZES.iter().enumerate() {
        let lh = (size as f32 * 1.6).ceil() as usize;
        let base_y = (size as f32 * 1.2).round();
        let mut reference = Canvas {
            w: width,
            h: lh,
            px: vec![0.0; width * lh],
        };
        draw(&alpha[si], SAMPLE, size as f32, &mut reference, 8.0, base_y);
        let mut line = format!("{size:>2}px:");
        let mut sdf_rows = Vec::new();
        for a in &sdfs {
            let mut c = Canvas {
                w: width,
                h: lh,
                px: vec![0.0; width * lh],
            };
            draw(a, SAMPLE, size as f32, &mut c, 8.0, base_y);
            let (mut sum, mut big, mut n) = (0.0f64, 0, 0);
            for (p, q) in c.px.iter().zip(&reference.px) {
                if *p > 0.0 || *q > 0.0 {
                    let d = (p - q).abs() * 255.0;
                    sum += d as f64;
                    n += 1;
                    if d > 32.0 {
                        big += 1;
                    }
                }
            }
            line += &format!(
                "  sdf{}: {:5.1} ({:4.1}%)",
                a.base,
                sum / n as f64,
                100.0 * big as f64 / n as f64
            );
            sdf_rows.push((format!("sdf{} @{size}", a.base), c));
        }
        println!("{line}");
        image_rows.push((format!("alpha @{size}"), reference));
        image_rows.extend(sdf_rows);
    }

    println!("\n## Render time, label \"{LABEL}\" @15px (avg of 2000)");
    for (name, a) in [
        ("alpha15", &alpha[2]),
        ("sdf16", &sdfs[0]),
        ("sdf24", &sdfs[1]),
        ("sdf32", &sdfs[2]),
    ] {
        let mut c = Canvas {
            w: 160,
            h: 24,
            px: vec![0.0; 160 * 24],
        };
        let t = Instant::now();
        for _ in 0..2000 {
            c.px.fill(0.0);
            draw(a, LABEL, 15.0, &mut c, 2.0, 18.0);
        }
        println!(
            "{name:>8}: {:6.2} µs",
            t.elapsed().as_secs_f64() * 1e6 / 2000.0
        );
    }

    // Comparison image: white text on the wheel's segment color, plus a 4x
    // zoom of the 13px and 15px rows.
    let bg = [38.0f32, 38.0, 46.0];
    let total_h: usize = image_rows.iter().map(|(_, c)| c.h).sum();
    let mut rgb = vec![0u8; width * total_h * 3];
    let mut y0 = 0;
    for (_, c) in &image_rows {
        for y in 0..c.h {
            for x in 0..width {
                let a = c.px[y * width + x];
                let i = ((y0 + y) * width + x) * 3;
                for k in 0..3 {
                    rgb[i + k] = (bg[k] * (1.0 - a) + 240.0 * a) as u8;
                }
            }
        }
        y0 += c.h;
    }
    write_png(&format!("{out}/text-compare.png"), width, total_h, &rgb);
    let order: Vec<&str> = image_rows.iter().map(|(n, _)| n.as_str()).collect();
    println!("\nrows top to bottom: {}", order.join(" | "));

    let zoom: usize = std::env::var("ZOOM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let zrows: Vec<&(String, Canvas)> = image_rows
        .iter()
        .filter(|(n, _)| n.ends_with("@13") || n.ends_with("@15"))
        .collect();
    let zw = std::env::var("ZW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
        * zoom;
    let zh: usize = zrows.iter().map(|(_, c)| c.h * zoom).sum();
    let mut z = vec![0u8; zw * zh * 3];
    let mut y0 = 0;
    for (_, c) in &zrows {
        for y in 0..c.h * zoom {
            for x in 0..zw {
                let a = c.px[(y / zoom) * width + x / zoom];
                let i = ((y0 + y) * zw + x) * 3;
                for k in 0..3 {
                    z[i + k] = (bg[k] * (1.0 - a) + 240.0 * a) as u8;
                }
            }
        }
        y0 += c.h * zoom;
    }
    write_png(&format!("{out}/text-zoom.png"), zw, zh, &z);
    let zorder: Vec<&str> = zrows.iter().map(|(n, _)| n.as_str()).collect();
    println!("zoom rows: {}", zorder.join(" | "));
}
