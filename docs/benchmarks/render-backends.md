# Render backends: CPU vs GLES vs wgpu, and our own rasterizer

**Question:** the wheel is a static overlay that changes on hover. What
should draw it?

**Verdict:** CPU, with our own analytic rasterizer. The GPU wins only
while animating, and our CPU path beats it there too once it does less
work.

## Method

Spike: [`spikes/render-bench/`](../../spikes/render-bench). Every backend
draws the same scene: an 8-segment ring in a 480x480 box with one
highlighted segment and a hub. Protocol per run: 5 times (show, animate 240
frames with a different highlighted segment every frame, hide), then 10 s
idle. Two surface sizes: `box` (480x480 layer surface) and `full`
(full-screen overlay). GPU backends also run `lazy` (GPU context only while
shown). 3 runs, medians; cold start 10 runs.

- `cpu`: tiny-skia into wl_shm
- `gles`: EGL + GLES 3.0 (glow), the wheel is one fragment shader
- `gpu-wgpu`: wgpu on Vulkan, same shader in WGSL
- `cpu-sdf`: our own rasterizer (see below)

## Results

| | CPU tiny-skia | GLES eager | GLES lazy | wgpu eager | wgpu lazy |
|---|---|---|---|---|---|
| idle RSS | **4.4 MB** | 77 MB | 77 MB | 88.5 MB | 19 MB |
| idle PSS | **1.9 MB** | 26 MB | 26 MB | 50 MB | 16 MB |
| idle VRAM | **0** | 3 MiB | 3 MiB | 18 MiB | 0 |
| threads | 1 | 1 | 1 | 6 | 1 |
| idle wakeups/s | 0.1 | 0.1 | 0.1 | **104** | 0.1 |
| cold exec to first frame | **3.6 ms** | 64 ms | n/a | 97 ms | n/a |
| warm show to frame (box) | 3.9 ms | **1.9 ms** | 4.4 ms | 5.8 ms | **107 ms** |
| warm show to frame (full) | 14 ms | **2.0 ms** | 4.5 ms | 5.4 ms | 107 ms |
| CPU per animated frame | ~1020 µs | ~650 µs | ~700 µs | **~125 µs** | ~126 µs |
| peak RSS while shown (full) | 61 MB | 77 MB | 78 MB | 88.5 MB | 90 MB |
| binary | 864 KB | 691 KB | | 4.2 MB | |
| crates | 39 | 39 | | 81 | |

"Show to frame" includes waiting for the frame callback (up to one
refresh, 4.2 ms at 240 Hz).

### Findings

- **Idle is what matters.** The tool is idle almost all the time. Loading
  the NVIDIA driver costs 70-90 MB RSS, and that memory never comes back.
- **Lazy GLES gains nothing.** After `eglTerminate` the driver libraries stay
  loaded: same RSS, same VRAM, and showing gets slower.
- **wgpu eager spins while idle:** 6 threads and ~104 wakeups/s.
  That rules it out for anything resident.
- **wgpu lazy idles well but takes 107 ms to show,** which is visible lag.
- **The GPU wins only while animating,** and this benchmark is the worst
  case (a full redraw every frame). The real wheel redraws only on hover
  changes.
- Text and icons are not in the scene. They are trivial on the CPU; on the
  GPU they need a glyph atlas and texture management.

## Own rasterizer (`cpu-sdf`)

No rasterizer dependency. Per-pixel analytic coverage (the same math as
the GPU shader), written straight into the wl_shm buffer with no
intermediate pixmap.

| version | first paint | hover repaint | idle anon |
|---|---|---|---|
| tiny-skia (reference) | ~1000 µs | ~1000 µs (always full) | 1.2 MB |
| v1: naive, libm `atan2` | ~5000 µs | 1590 µs | 0.2 MB |
| v2: fast atan, row spans, no memset | ~2700 µs | 845 µs | 0.2 MB |
| **v3: coverage mask + color LUT** | **~300 µs** (incl. mask) | **~57 µs** | 0.65 MB* |

\* The 460 KB mask is freed on hide; malloc keeps the heap. Irrelevant for
spawn-per-use, because the process exits.

**The first own version was 5x slower than tiny-skia.** Writing it
ourselves did not make it fast. Doing less work did: a precomputed mask,
touching only the pixels that change, and no float-to-int conversion in
the hot loop.

| | tiny-skia | cpu-sdf v3 |
|---|---|---|
| hover repaint | ~1000 µs | **~57 µs** |
| CPU per animated frame | ~1020 µs | **~68 µs** (wgpu: 125 µs) |
| full-screen peak RSS | 61 MB | **7.1 MB** (fresh shm pages are never touched) |
| idle RSS | 4.4 MB | 3.6 MB |
| crates | 39 | 32 |
| binary | 848 KB | 663 KB |

**Correctness:** checked against an independent tiny-skia render. Mean
absolute difference is 0.033/255. About 20 of 921,600 channels differ by
more than 24, all at segment corners (polygon versus analytic AA). A hover
repaint is bit-identical to a full repaint.

**Damage bug (found later, in the [drag probe](drag-into-circle.md)):**
the spike damaged the segments that changed *relative to the buffer
being drawn*. With two buffers in turn, the screen shows the other one,
so a highlight could stay on screen. The buffer contents were right (that
is what the test checked); the reported damage was not. Damage must cover
what differs from the screen: the previously shown and the new segment.
The timings above are unaffected.

## Reproduce

```sh
cd spikes/render-bench
./bench.sh 3 > results.txt            # all backends; the wheel will flash on screen
python3 summarize.py results.txt      # medians as a markdown table
cargo test --release -p cpu-sdf       # correctness vs tiny-skia
./target/release/cpu-sdf [--full]     # own rasterizer alone
```
