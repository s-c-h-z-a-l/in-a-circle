//! Text measuring, fitting and drawing with the baked atlas.
//!
//! Advances are 26.6 fixed point and accumulate before rounding: glyph k sits
//! at `round(x + sum of the advances before it)`, and a text's width is the
//! rounded sum, so measuring and drawing agree. A character the atlas lacks
//! takes no room and draws nothing; callers that must show it faithfully
//! check `Atlas::has_all` first.

use crate::atlas::{self, SUBPIXEL};
use std::borrow::Cow;

const ELLIPSIS: char = '…';
const HALF: u64 = (SUBPIXEL / 2) as u64;

fn round_px(fixed: u64) -> u32 {
    ((fixed + HALF) / SUBPIXEL as u64) as u32
}

fn advance(c: char) -> u64 {
    atlas::get().glyph(c).map_or(0, |g| u64::from(g.advance))
}

/// Width of `s` in pixels: the rounded sum of its glyph advances.
pub fn width(s: &str) -> u32 {
    round_px(s.chars().map(advance).sum())
}

/// `s` if it fits in `max_w`, else its longest prefix that fits together
/// with a trailing `…`, cut on a character boundary. Empty if not even the
/// ellipsis fits.
pub fn fit(s: &str, max_w: u32) -> Cow<'_, str> {
    if width(s) <= max_w {
        return Cow::Borrowed(s);
    }
    let ell = advance(ELLIPSIS);
    if round_px(ell) > max_w {
        return Cow::Borrowed("");
    }
    let mut used = 0;
    let mut cut = 0;
    for (i, c) in s.char_indices() {
        let next = used + advance(c);
        if round_px(next + ell) > max_w {
            break;
        }
        used = next;
        cut = i + c.len_utf8();
    }
    let mut out = String::with_capacity(cut + ELLIPSIS.len_utf8());
    out.push_str(&s[..cut]);
    out.push(ELLIPSIS);
    Cow::Owned(out)
}

/// Draws `s` with its line box's top-left at (`x`, `y`) onto a premultiplied
/// BGRA canvas (`stride` bytes per row). `color` is the premultiplied BGRA of
/// full coverage; each pixel is composited with `over`. Clipped to the canvas;
/// any `x`/`y` is safe (the pen math is 64-bit).
pub fn draw(canvas: &mut [u8], stride: usize, x: i32, y: i32, s: &str, color: [u8; 4]) {
    let a = atlas::get();
    let rows = canvas.len().checked_div(stride).unwrap_or(0) as i64;
    let cols = (stride / 4) as i64;
    let base = i64::from(y) + i64::from(a.ascent());
    let mut pen = i64::from(x) * SUBPIXEL as i64;
    for c in s.chars() {
        let Some(g) = a.glyph(c) else { continue };
        let gx = ((pen + HALF as i64) >> 6) + i64::from(g.xoff);
        let gy = base + i64::from(g.yoff);
        pen += i64::from(g.advance);
        for row in 0..i64::from(g.h) {
            let py = gy + row;
            if py < 0 || py >= rows {
                continue;
            }
            for col in 0..i64::from(g.w) {
                let px = gx + col;
                if px < 0 || px >= cols {
                    continue;
                }
                let cov = u32::from(g.alpha[(row * i64::from(g.w) + col) as usize]);
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
/// `1 file · <type_label>`; an empty name counts as unrenderable. Several: `n / total files`. No names: `Nothing to do`.
pub fn hub_text(names: &[&str], total: usize, type_label: &str, max_w: u32) -> String {
    debug_assert!(
        names.len() <= total,
        "{} names for {total} files",
        names.len()
    );
    let text = match names {
        [] => return "Nothing to do".to_string(),
        [name] if total == 1 && !name.is_empty() && atlas::get().has_all(name) => {
            (*name).to_string()
        }
        [_] if total == 1 => format!("1 file · {type_label}"),
        _ => format!("{} / {total} files", names.len()),
    };
    fit(&text, max_w).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas;

    /// Rounded sum of 26.6 advances, as `width` must measure.
    fn rounded(sum: u32) -> u32 {
        (sum + 32) >> 6
    }

    #[test]
    fn width_sums_advances() {
        let a = atlas::get();
        assert_eq!(width(""), 0);
        let adv = |c| a.glyph(c).unwrap().advance;
        assert_eq!(width("A"), rounded(adv('A')));
        assert_eq!(
            width("Aőx "),
            rounded(adv('A') + adv('ő') + adv('x') + adv(' '))
        );
        // Fractions accumulate before rounding: eight 'e's are not 8 * round(e).
        let e = adv('e');
        assert_eq!(width("eeeeeeee"), rounded(8 * e));
        assert_ne!(e % 64, 0);
        // A character outside the atlas takes no room (callers check has_all).
        assert_eq!(width("A漢"), width("A"));
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
        // Measuring and fitting agree at every limit.
        for max in 0..=full {
            let f = fit(s, max);
            assert!(width(&f) <= max, "max {max}: {f:?} is {}", width(&f));
            if !f.is_empty() && *f != *s {
                let body = f.trim_end_matches('…');
                let next = s[body.len()..].chars().next().unwrap();
                assert!(
                    width(&format!("{body}{next}…")) > max,
                    "max {max}: {f:?} not maximal"
                );
            }
        }
        // Tight limits: only the ellipsis, or nothing at all.
        let ell = width("…");
        assert_eq!(fit(s, ell), "…");
        assert_eq!(fit(s, ell - 1), "");
        assert_eq!(fit(s, 0), "");
        assert_eq!(fit("", 0), "");
    }

    /// Premultiplied-over of `color` at coverage `a` onto `dst`, per channel.
    fn over(dst: u8, color: u8, color_a: u8, a: u8) -> u8 {
        let d = |x: u32| (x + 127) / 255;
        let src = d(color as u32 * a as u32);
        let sa = d(color_a as u32 * a as u32);
        (src + d(dst as u32 * (255 - sa))) as u8
    }

    #[test]
    fn draw_matches_reference_pixels() {
        let a = atlas::get();
        let (w, h) = (60usize, 24usize);
        let stride = w * 4;
        let bg = [7u8, 9, 11, 200];
        let word = "Hiemt";
        let mut canvas: Vec<u8> = bg.iter().copied().cycle().take(stride * h).collect();
        let color = [200u8, 150, 100, 255];
        let (x0, y0) = (3, 2);
        draw(&mut canvas, stride, x0, y0, word, color);

        // Independent reference: glyph k sits at round(x0 + sum of the
        // fractional advances before it).
        let mut expect: Vec<u8> = bg.iter().copied().cycle().take(stride * h).collect();
        let mut sum = 0.0f64;
        for c in word.chars() {
            let g = a.glyph(c).unwrap();
            let pen = (f64::from(x0) + sum).round() as i32;
            for gy in 0..g.h as i32 {
                for gx in 0..g.w as i32 {
                    let cov = g.alpha[(gy as u32 * g.w + gx as u32) as usize];
                    if cov == 0 {
                        continue;
                    }
                    let px = pen + g.xoff + gx;
                    let py = y0 + a.ascent() + g.yoff + gy;
                    assert!(px >= 0 && py >= 0 && (px as usize) < w && (py as usize) < h);
                    let o = py as usize * stride + px as usize * 4;
                    for k in 0..4 {
                        expect[o + k] = over(expect[o + k], color[k], color[3], cov);
                    }
                }
            }
            sum += f64::from(g.advance) / 64.0;
        }
        assert_eq!(canvas, expect);
        assert_ne!(
            canvas,
            bg.iter()
                .copied()
                .cycle()
                .take(stride * h)
                .collect::<Vec<u8>>()
        );
        // Golden pin of the rendered word (catches atlas or blend drift).
        assert_eq!(fnv(&canvas), GOLDEN);
    }

    const GOLDEN: u64 = 9400555634322009208;

    fn fnv(b: &[u8]) -> u64 {
        b.iter().fold(0xcbf29ce484222325, |h, &x| {
            (h ^ x as u64).wrapping_mul(0x100000001b3)
        })
    }

    #[test]
    fn draw_clips_to_the_canvas() {
        let word = "Wqő.";
        let color = [255u8, 255, 255, 255];
        // Reference: the word drawn unclipped into a big canvas.
        let (bw, bh) = (80usize, 60usize);
        let (ox, oy) = (30i32, 20i32);
        let mut big = vec![0u8; bw * 4 * bh];
        draw(&mut big, bw * 4, ox, oy, word, color);
        assert!(big.iter().any(|&b| b != 0));
        let (w, h) = (10usize, 10usize);
        let cases = [
            (-4, -3),
            (-200, 0),
            (200, 0),
            (0, -200),
            (0, 200),
            (-3, -5),
            (8, 7),
            (5, 3),
            (-9, 2),
            (3, -8),
            (0, 0),
        ];
        for (x, y) in cases {
            let mut c = vec![0u8; w * 4 * h];
            draw(&mut c, w * 4, x, y, word, color);
            // Crop: the small canvas sits at (cx, cy) in the big canvas so
            // that the word lands at (x, y) in the small one.
            let mut expect = vec![0u8; w * 4 * h];
            for r in 0..h as i32 {
                for col in 0..w as i32 {
                    let (bx, by) = (col + ox - x, r + oy - y);
                    if bx >= 0 && by >= 0 && (bx as usize) < bw && (by as usize) < bh {
                        let s = by as usize * bw * 4 + bx as usize * 4;
                        let d = r as usize * w * 4 + col as usize * 4;
                        expect[d..d + 4].copy_from_slice(&big[s..s + 4]);
                    }
                }
            }
            assert_eq!(c, expect, "at ({x}, {y})");
        }
    }

    #[test]
    fn draw_survives_extreme_coordinates() {
        let mut c = vec![0u8; 16 * 4 * 16];
        for x in [i32::MAX, i32::MAX - 3, i32::MIN, i32::MIN + 3, 0] {
            for y in [i32::MAX, i32::MAX - 3, i32::MIN, i32::MIN + 3, 0] {
                draw(&mut c, 16 * 4, x, y, "Wq.…", [255, 255, 255, 255]);
            }
        }
        // Only the (0, 0) draws touched the canvas, and they did.
        assert!(c.iter().any(|&b| b != 0));
        let mut d = vec![0u8; 16 * 4 * 16];
        draw(&mut d, 16 * 4, i32::MAX - 3, i32::MAX - 3, "Wq", [255; 4]);
        draw(&mut d, 16 * 4, i32::MIN, i32::MIN, "Wq", [255; 4]);
        assert!(d.iter().all(|&b| b == 0));
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
        // An empty name is as unrenderable as a missing glyph.
        assert_eq!(hub_text(&[""], 1, "text", wide), "1 file · text");
        // Whatever the hub shows must be drawable: every glyph is in the atlas.
        let a = atlas::get();
        for out in [
            hub_text(&["notes.txt"], 1, "text", wide),
            hub_text(&[long], 1, "text", max),
            hub_text(&["漢字.txt"], 1, "text", wide),
            hub_text(&["漢字.txt"], 1, "text", tiny),
            hub_text(&["a", "b", "c"], 5, "text", wide),
            hub_text(&[""], 1, "image", wide),
            hub_text(&[], 0, "text", wide),
        ] {
            assert!(a.has_all(&out), "{out:?} has a glyph the atlas lacks");
        }
    }
}
