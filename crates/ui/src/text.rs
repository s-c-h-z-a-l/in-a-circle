//! Text measuring, fitting and drawing with the baked atlas.
//!
//! Glyphs sit on integer pen positions (advances are rounded when baked).
//! A character the atlas lacks takes no room and draws nothing; callers that
//! must show it faithfully check `Atlas::has_all` first.

use crate::atlas;
use std::borrow::Cow;

const ELLIPSIS: char = '…';

/// Width of `s` in pixels: the sum of its glyph advances.
pub fn width(s: &str) -> u32 {
    let a = atlas::get();
    s.chars()
        .filter_map(|c| a.glyph(c))
        .map(|g| g.advance)
        .sum()
}

/// `s` if it fits in `max_w`, else its longest prefix that fits together
/// with a trailing `…`, cut on a character boundary. Empty if not even the
/// ellipsis fits.
pub fn fit(s: &str, max_w: u32) -> Cow<'_, str> {
    if width(s) <= max_w {
        return Cow::Borrowed(s);
    }
    let ell = width("…");
    let mut used = 0;
    let mut cut = 0;
    for (i, c) in s.char_indices() {
        let adv = atlas::get().glyph(c).map_or(0, |g| g.advance);
        if used + adv + ell > max_w {
            break;
        }
        used += adv;
        cut = i + c.len_utf8();
    }
    if used + ell > max_w {
        return Cow::Borrowed("");
    }
    let mut out = String::with_capacity(cut + ELLIPSIS.len_utf8());
    out.push_str(&s[..cut]);
    out.push(ELLIPSIS);
    Cow::Owned(out)
}

/// Draws `s` with its line box's top-left at (`x`, `y`) onto a premultiplied
/// BGRA canvas (`stride` bytes per row). `color` is the premultiplied BGRA of
/// full coverage; each pixel is composited with `over`. Clipped to the canvas.
pub fn draw(canvas: &mut [u8], stride: usize, x: i32, y: i32, s: &str, color: [u8; 4]) {
    let a = atlas::get();
    let rows = canvas.len().checked_div(stride).unwrap_or(0);
    let cols = stride / 4;
    let base = y + a.ascent();
    let mut pen = x;
    for c in s.chars() {
        let Some(g) = a.glyph(c) else { continue };
        let (gx, gy) = (pen + g.xoff, base + g.yoff);
        pen += g.advance as i32;
        for row in 0..g.h as i32 {
            let py = gy + row;
            if py < 0 || py as usize >= rows {
                continue;
            }
            for col in 0..g.w as i32 {
                let px = gx + col;
                if px < 0 || px as usize >= cols {
                    continue;
                }
                let cov = u32::from(g.alpha[(row as u32 * g.w + col as u32) as usize]);
                if cov == 0 {
                    continue;
                }
                let o = py as usize * stride + px as usize * 4;
                let sa = div255(u32::from(color[3]) * cov);
                for k in 0..4 {
                    let src = div255(u32::from(color[k]) * cov);
                    canvas[o + k] = (src + div255(u32::from(canvas[o + k]) * (255 - sa))) as u8;
                }
            }
        }
    }
}

fn div255(x: u32) -> u32 {
    (x + 127) / 255
}

/// What the hub shows for the dropped files. One file: its name, cut to
/// `max_w`, unless the atlas lacks a character of it, then
/// `1 file · <type_label>`. Several: `n / total files`. No names: `Nothing to do`.
pub fn hub_text(names: &[&str], total: usize, type_label: &str, max_w: u32) -> String {
    let text = match names {
        [] => return "Nothing to do".to_string(),
        [name] if total == 1 && atlas::get().has_all(name) => (*name).to_string(),
        [_] if total == 1 => format!("1 file · {type_label}"),
        _ => format!("{} / {total} files", names.len()),
    };
    fit(&text, max_w).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas;

    #[test]
    fn width_sums_advances() {
        let a = atlas::get();
        assert_eq!(width(""), 0);
        let adv = |c| a.glyph(c).unwrap().advance;
        assert_eq!(width("A"), adv('A'));
        assert_eq!(width("Aőx "), adv('A') + adv('ő') + adv('x') + adv(' '));
        // A character outside the atlas takes no room (callers check has_all).
        assert_eq!(width("A漢"), adv('A'));
    }

    #[test]
    fn fit_cuts_on_char_boundary_with_ellipsis() {
        let s = "Árvíztűrő tükörfúrógép.txt";
        let full = width(s);
        assert!(matches!(fit(s, full), Cow::Borrowed(_)));
        assert_eq!(fit(s, full + 100), s);
        let cut = fit(s, full - 1);
        assert!(cut.ends_with('…') && *cut != *s, "{cut}");
        assert!(width(&cut) < full);
        // The cut is the longest prefix that still fits with the ellipsis.
        let body = cut.trim_end_matches('…');
        assert!(s.starts_with(body));
        let next = s[body.len()..].chars().next().unwrap();
        let longer = format!("{body}{next}…");
        assert!(width(&longer) > full - 1);
        // Tight limits: only the ellipsis, or nothing at all.
        let ell = width("…");
        assert_eq!(fit(s, ell), "…");
        assert_eq!(fit(s, ell - 1), "");
        assert_eq!(fit(s, 0), "");
        assert_eq!(fit("", 0), "");
    }

    /// Premultiplied-over of `color` at coverage `a` onto `dst`, per channel.
    fn over(dst: u8, color: u8, color_a: u8, a: u8) -> u8 {
        let d = |x: u32| ((x + 127) / 255) as u8;
        let src = d(color as u32 * a as u32);
        let sa = d(color_a as u32 * a as u32);
        src + d(dst as u32 * (255 - sa as u32))
    }

    #[test]
    fn draw_matches_reference_pixels() {
        let a = atlas::get();
        let (w, h) = (40usize, 24usize);
        let stride = w * 4;
        let bg = [7u8, 9, 11, 200];
        let mut canvas: Vec<u8> = bg.iter().copied().cycle().take(stride * h).collect();
        let color = [200u8, 150, 100, 255];
        let (x0, y0) = (3, 2);
        draw(&mut canvas, stride, x0, y0, "Hi", color);

        // Independent reference: place each glyph at the running pen.
        let mut expect: Vec<u8> = bg.iter().copied().cycle().take(stride * h).collect();
        let mut pen = x0;
        for c in "Hi".chars() {
            let g = a.glyph(c).unwrap();
            for gy in 0..g.h as i32 {
                for gx in 0..g.w as i32 {
                    let cov = g.alpha[(gy as u32 * g.w + gx as u32) as usize];
                    let px = pen + g.xoff + gx;
                    let py = y0 + a.ascent() + g.yoff + gy;
                    if cov == 0 {
                        continue;
                    }
                    assert!(px >= 0 && py >= 0 && (px as usize) < w && (py as usize) < h);
                    let o = py as usize * stride + px as usize * 4;
                    for k in 0..4 {
                        expect[o + k] = over(expect[o + k], color[k], color[3], cov);
                    }
                }
            }
            pen += g.advance as i32;
        }
        assert_eq!(canvas, expect);
        // Something was drawn, and the first glyph's full-coverage pixel is exact.
        assert_ne!(
            canvas,
            bg.iter()
                .copied()
                .cycle()
                .take(stride * h)
                .collect::<Vec<u8>>()
        );
        // Golden pin of the rendered word (catches atlas or blend drift).
        assert_eq!(fnv(&canvas), GOLDEN_HI);
    }

    const GOLDEN_HI: u64 = 7361427187234151445;

    fn fnv(b: &[u8]) -> u64 {
        b.iter().fold(0xcbf29ce484222325, |h, &x| {
            (h ^ x as u64).wrapping_mul(0x100000001b3)
        })
    }

    #[test]
    fn draw_clips_to_the_canvas() {
        let (w, h) = (10usize, 10usize);
        let stride = w * 4;
        let mut canvas = vec![0u8; stride * h];
        // Entirely off-canvas on every side: nothing changes, no panic.
        for (x, y) in [(-200, 0), (200, 0), (0, -200), (0, 200), (-3, -5), (8, 7)] {
            draw(&mut canvas, stride, x, y, "Wq", [255, 255, 255, 255]);
        }
        let mut one = vec![0u8; stride * h];
        draw(&mut one, stride, -4, -3, "W", [255, 255, 255, 255]);
        assert!(one.iter().any(|&b| b != 0));
    }

    #[test]
    fn hub_text_fits_and_falls_back() {
        let wide = 10_000;
        assert_eq!(hub_text(&["notes.txt"], 1, "text", wide), "notes.txt");
        // Long accented name is cut with an ellipsis to the hub width.
        let long = "Árvíztűrő tükörfúrógép hosszú jegyzetek 2026.txt";
        let max = 128;
        let t = hub_text(&[long], 1, "text", max);
        assert!(t.ends_with('…') && width(&t) <= max, "{t}");
        // Characters outside the atlas: count and type instead of the name.
        assert_eq!(hub_text(&["漢字.txt"], 1, "text", wide), "1 file · text");
        // Several files: how many of how many.
        assert_eq!(hub_text(&["a", "b", "c"], 5, "text", wide), "3 / 5 files");
        assert_eq!(hub_text(&["a", "b"], 2, "image", wide), "2 / 2 files");
        // Nothing usable.
        assert_eq!(hub_text(&[], 0, "text", wide), "Nothing to do");
        // Even the fallback is cut to the width.
        let tiny = width("1 fi…");
        let t = hub_text(&["漢字.txt"], 1, "text", tiny);
        assert!(t.ends_with('…') && width(&t) <= tiny, "{t}");
    }
}
