# Benchmarks

Measurements that shaped In.A.Circle's architecture. Each page has the
method, the machine, the numbers, the verdict, and how to reproduce it.
Spike code lives in [`spikes/`](../../spikes).

| # | Question | Verdict | Page |
|---|---|---|---|
| 1 | GPU or CPU rendering? | CPU. A GPU context costs 70-90 MB idle for a ~2 ms gain. | [render-backends.md](render-backends.md) |
| 2 | Rasterizer dependency or our own? | Our own: 17x faster hover repaint than tiny-skia, 7 MB vs 61 MB peak at full screen. | [render-backends.md](render-backends.md#own-rasterizer-cpu-sdf) |
| 3 | Where does startup latency go? | Our work is ~0.6 ms; the compositor owns the rest. | [latency.md](latency.md) |
| 4 | How expensive is context detection? | ~1.2 ms over pure protocols; MIME types are enough to classify. | [context-detection.md](context-detection.md) |
| 5 | Which surface strategy? Daemon or spawn-per-use? | 1x1 buffer + wp_viewporter + subsurface; spawn-per-use (a daemon presents no faster). | [surface-strategy.md](surface-strategy.md) |
| 6 | wayland-client or our own wire protocol? | Our own: 1 dependency instead of 18, 344 KB vs 607 KB, same latency. | [wire-protocol.md](wire-protocol.md) |

## Reference machine

All numbers are from one machine unless stated otherwise:

- AMD Ryzen 7 5700X (8C/16T), 94 GB RAM, CachyOS (Linux 7.2.9)
- NVIDIA RTX 3080 Ti, proprietary driver 615.71.09
- niri 26.04, one output 5120x1440 @ 240 Hz, scale 1, VRR on-demand

Compositor-side numbers (presentation time, enter-event latency) are
niri-specific. Other compositors still need to be measured.

## Budgets

These measurements set the budgets new code must stay within (see
`CLAUDE.md`). Spikes double as regression checks.
