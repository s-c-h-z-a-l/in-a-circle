# Text rendering: baked alpha atlas vs SDF atlas

**Question:** the wheel needs short labels. Should glyphs come from one
SDF atlas (any size from one table) or from alpha bitmaps baked at the
exact sizes we use?

**Verdict:** **baked alpha at exact sizes.** It is the reference quality by
definition, ~19x faster to draw, and smaller as long as the UI uses a
couple of fixed sizes. SDF only pays off for arbitrary or animated sizes,
and single-channel SDF is visibly softer at UI sizes.

## Method

Spike: [`spikes/font/`](../../spikes/font), an offline tool (fontdue and png
are build-time only and never ship). Font: Noto Sans Medium (OFL). Charset:
ASCII plus Hungarian accented letters, 113 glyphs.

- **Alpha:** fontdue rasterizes each glyph at the exact target size.
  This is the reference.
- **SDF:** rasterized at 8x oversampling, brute-force signed distance,
  stored as u8 at base size 16/24/32 with spread 2/3/4 px. Drawn with
  bilinear sampling and a 1-target-pixel AA ramp.
- Both are placed on integer pen positions and the same integer baseline,
  so only glyph shape is compared. Metric: mean absolute coverage
  difference against the reference over pixels covered by either, plus the
  share of pixels off by more than 32/255.

## Results

### Quality vs the exact-size reference (mean abs diff /255, pixels off by more than 32)

| size | sdf16 | sdf24 | sdf32 |
|---|---|---|---|
| 11 px | 21.8 (23.1%) | 12.7 (7.6%) | 9.8 (3.1%) |
| 13 px | 21.6 (24.0%) | 11.9 (5.6%) | 9.3 (2.9%) |
| 15 px | 21.5 (22.7%) | 10.3 (4.1%) | 8.1 (2.2%) |
| 18 px | 22.2 (23.4%) | 9.9 (3.2%) | 8.0 (1.8%) |
| 24 px | 21.6 (25.4%) | 10.7 (5.5%) | 8.0 (2.0%) |
| 32 px | 21.1 (25.9%) | 10.6 (8.5%) | 8.0 (3.3%) |

Visually (8x zoom, 13 px and 15 px), sdf16 has thin, grey stems; sdf24 and
sdf32 come close but stay slightly softer than the reference.

### Size (113 glyphs, bitmap + metrics)

| atlas | bytes |
|---|---|
| alpha 13 px | 10 KB |
| alpha 15 px | 12 KB |
| alpha 13 + 15 px, plus 26 + 30 px for scale 2 | ~22 KB + ~70 KB = **~92 KB** |
| sdf16 (all sizes) | 23 KB |
| sdf24 (all sizes) | 48 KB |
| sdf32 (all sizes) | 82 KB |

### Draw time, 11-glyph label at 15 px

| | time |
|---|---|
| alpha | **1.9 µs** |
| sdf (any base) | ~35 µs |

Both are negligible against the 2 ms budget.

## Notes

- **A bug was caught by the numbers.** The first run showed large,
  non-monotonic differences (sdf32 worse at 13 px than at 11 px). The
  cause was placement: a fractional baseline in one path and an SDF quad
  anchored to a rounded-up height. A visual check alone would have missed
  it.
- **Open: fractional scaling** (1.25, 1.5). Options are baking the nearest
  integer scale, or rasterizing at runtime from an embedded, subsetted font
  (exact at any scale; needs an own TrueType rasterizer).

## Reproduce

```sh
cd spikes/font && cargo build --release
./target/release/font /usr/share/fonts/noto/NotoSans-Medium.ttf /tmp
ZOOM=8 ZW=120 ./target/release/font /usr/share/fonts/noto/NotoSans-Medium.ttf /tmp   # detail crop
# writes /tmp/text-compare.png and /tmp/text-zoom.png
```
