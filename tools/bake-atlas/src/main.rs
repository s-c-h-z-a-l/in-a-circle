//! Offline glyph atlas baking: `bake-atlas <font.ttf> <out.bin>`.
//!
//! Rasterizes the wheel's charset at 15 px with fontdue and writes the binary
//! atlas the `ui` crate embeds. Deterministic. Never shipped.

// Layout (little endian), read in place by `ui::atlas`:
//   0  "IACA"          magic
//   4  u16 version     (1)
//   6  u16 size        pixel size the glyphs were rasterized at
//   8  i16 ascent      rounded, pixels above the baseline
//  10  i16 descent     rounded, pixels below the baseline (positive)
//  12  i16 line_gap    rounded
//  14  u16 reserved    (0)
//  16  u32 glyph_count
//  20  u32 alpha_len
//  24  u64 font_len    byte length of the source font
//  32  u64 font_hash   FNV-1a of the source font
//  40  glyph table, `glyph_count` entries of 20 bytes sorted by codepoint:
//        u32 codepoint, u16 w, u16 h, i16 xoff, i16 yoff (top edge, y down
//        from the baseline), u16 advance (rounded), u16 reserved (0),
//        u32 offset into the alpha data
//      alpha data: `alpha_len` bytes of w*h coverage, row-major
const SIZE_PX: u16 = 15;

fn charset() -> Vec<char> {
    let mut v: Vec<char> = (0x20u8..=0x7E).map(char::from).collect();
    v.extend("áéíóöőúüűÁÉÍÓÖŐÚÜŰ…".chars());
    v.sort_unstable();
    v
}

fn bake(font_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let font = fontdue::Font::from_bytes(font_bytes, fontdue::FontSettings::default())
        .map_err(|e| format!("font: {e}"))?;
    let px = f32::from(SIZE_PX);
    let lm = font
        .horizontal_line_metrics(px)
        .ok_or("font has no horizontal metrics")?;
    let chars = charset();
    let mut table = Vec::new();
    let mut alpha: Vec<u8> = Vec::new();
    for &c in &chars {
        if font.lookup_glyph_index(c) == 0 {
            return Err(format!("font lacks {c:?}"));
        }
        let (m, bitmap) = font.rasterize(c, px);
        let top = -(m.ymin + m.height as i32);
        table.extend((c as u32).to_le_bytes());
        table.extend(
            u16::try_from(m.width)
                .map_err(|e| e.to_string())?
                .to_le_bytes(),
        );
        table.extend(
            u16::try_from(m.height)
                .map_err(|e| e.to_string())?
                .to_le_bytes(),
        );
        table.extend(
            i16::try_from(m.xmin)
                .map_err(|e| e.to_string())?
                .to_le_bytes(),
        );
        table.extend(i16::try_from(top).map_err(|e| e.to_string())?.to_le_bytes());
        table.extend((m.advance_width.round() as u16).to_le_bytes());
        table.extend(0u16.to_le_bytes());
        table.extend(
            u32::try_from(alpha.len())
                .map_err(|e| e.to_string())?
                .to_le_bytes(),
        );
        alpha.extend(bitmap);
    }
    let mut out = Vec::with_capacity(40 + table.len() + alpha.len());
    out.extend(b"IACA");
    out.extend(1u16.to_le_bytes());
    out.extend(SIZE_PX.to_le_bytes());
    out.extend((lm.ascent.round() as i16).to_le_bytes());
    out.extend(((-lm.descent).round() as i16).to_le_bytes());
    out.extend((lm.line_gap.round() as i16).to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((chars.len() as u32).to_le_bytes());
    out.extend((alpha.len() as u32).to_le_bytes());
    out.extend((font_bytes.len() as u64).to_le_bytes());
    out.extend(fnv1a(font_bytes).to_le_bytes());
    out.extend(table);
    out.extend(alpha);
    Ok(out)
}

fn fnv1a(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325, |h, &x| {
        (h ^ u64::from(x)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: bake-atlas <font.ttf> <out.bin>");
        std::process::exit(2);
    }
    let font = std::fs::read(&args[1]).unwrap_or_else(|e| fail(&format!("{}: {e}", args[1])));
    let out = bake(&font).unwrap_or_else(|e| fail(&e));
    std::fs::write(&args[2], out).unwrap_or_else(|e| fail(&format!("{}: {e}", args[2])));
}

fn fail(msg: &str) -> ! {
    eprintln!("bake-atlas: {msg}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u64_at(b: &[u8], o: usize) -> u64 {
        u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
    }

    /// The committed atlas must be what this tool bakes from the recorded
    /// font. Skips when the font is not on this machine or is another build.
    #[test]
    fn atlas_bin_matches_the_bake() {
        let committed = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/ui/src/atlas.bin"
        ))
        .unwrap();
        assert!(committed.len() > 40, "atlas.bin is empty");
        let path = std::env::var("INACIRCLE_FONT")
            .unwrap_or_else(|_| "/usr/share/fonts/noto/NotoSans-Medium.ttf".into());
        let Ok(font) = std::fs::read(&path) else {
            eprintln!("skipped: no font at {path}");
            return;
        };
        let (len, hash) = (u64_at(&committed, 24), u64_at(&committed, 32));
        if len != font.len() as u64 || hash != fnv1a(&font) {
            eprintln!("skipped: {path} is not the font the atlas was baked from");
            return;
        }
        assert!(
            bake(&font).unwrap() == committed,
            "atlas.bin is stale: re-bake"
        );
    }

    #[test]
    fn bake_is_deterministic() {
        let path = std::env::var("INACIRCLE_FONT")
            .unwrap_or_else(|_| "/usr/share/fonts/noto/NotoSans-Medium.ttf".into());
        let Ok(font) = std::fs::read(&path) else {
            eprintln!("skipped: no font at {path}");
            return;
        };
        assert!(bake(&font).unwrap() == bake(&font).unwrap());
    }

    #[test]
    fn bad_font_is_an_error() {
        assert!(bake(b"not a font").is_err());
    }
}
