# Spikes

Throwaway measurement code. **This is not the product.** Each spike
answers one question with numbers, and the answer lives in
[`docs/benchmarks/`](../docs/benchmarks). The code stays here so every
result can be reproduced.

| spike | question | report |
|---|---|---|
| [`render-bench/`](render-bench) | CPU vs GLES vs wgpu; tiny-skia vs our own rasterizer | [render-backends.md](../docs/benchmarks/render-backends.md) |
| [`latency/`](latency) | startup latency, context detection, surface strategy, file manager input, drag into the circle | [latency.md](../docs/benchmarks/latency.md), [context-detection.md](../docs/benchmarks/context-detection.md), [surface-strategy.md](../docs/benchmarks/surface-strategy.md), [file-manager-input.md](../docs/benchmarks/file-manager-input.md), [drag-into-circle.md](../docs/benchmarks/drag-into-circle.md) |
| [`wire/`](wire) | own Wayland wire protocol vs wayland-client | [wire-protocol.md](../docs/benchmarks/wire-protocol.md) |
| [`font/`](font) | baked alpha glyph atlas vs SDF | [text-rendering.md](../docs/benchmarks/text-rendering.md) |
| [`observe/`](observe), [`nautilus/`](nautilus) | which events a file manager exposes (AT-SPI, D-Bus, clipboard, inotify) | [file-manager-input.md](../docs/benchmarks/file-manager-input.md) |

Spikes are written to answer questions quickly: they `unwrap()`, they
have few tests, and some put windows on your screen while they run. Don't
build on them; the real code will be written from scratch against the
decisions they produced.
