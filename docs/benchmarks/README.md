# Benchmarks

Measurements that shaped In.A.Circle's architecture. Each page has the
method, the machine, the numbers, the verdict, and how to reproduce it.
Spike code lives in [`spikes/`](../../spikes).

| # | Question | Verdict | Page |
|---|---|---|---|
| 1 | GPU or CPU rendering? | CPU. A GPU context costs 70-90 MB idle for a ~2 ms gain. | [render-backends.md](render-backends.md) |
| 2 | Rasterizer dependency or our own? | Our own: one-octant table, ~0.4 ms cold first paint, ~14 µs hover repaint (tiny-skia: ~1 ms each). | [render-backends.md](render-backends.md#own-rasterizer-cpu-sdf) |
| 3 | Where does startup latency (and CPU) go? | Our work is ~0.5 ms CPU from `main`; the compositor owns the rest, and niri's spawn adds ~1 ms CPU before `main`. | [latency.md](latency.md) |
| 4 | How expensive is context detection? | ~1.2 ms over pure protocols; MIME types are enough to classify. | [context-detection.md](context-detection.md) |
| 5 | Which surface strategy? Daemon or spawn-per-use? | 1x1 buffer + wp_viewporter + subsurface; spawn-per-use (a daemon presents no faster). | [surface-strategy.md](surface-strategy.md) |
| 6 | wayland-client or our own wire protocol? | Our own: 1 dependency instead of 18, 344 KB vs 607 KB, same latency. | [wire-protocol.md](wire-protocol.md) |
| 7 | SDF atlas or baked alpha for text? | Baked alpha at exact sizes: reference quality, 19x faster, ~92 KB for 2 sizes x 2 scales. | [text-rendering.md](text-rendering.md) |
| 8 | Can the wheel see the file manager's selection? | Not cleanly: invisible over protocols; Ctrl+C, a Nautilus extension or AT-SPI each cost too much. Rejected in favor of #9. | [file-manager-input.md](file-manager-input.md) |
| 9 | Grab a file, press the hotkey, drop it on a slice? | Works over the standard protocol, on the real surface layout, with keyboard focus: exact URIs, copy-only, Esc cancels. The wheel appears on the first movement during a drag; file types are known before the drop (Nautilus, Thunar, Nemo, Firefox: under 2 ms). | [drag-into-circle.md](drag-into-circle.md) |
| 10 | Is a 635 KiB `inacircle` a problem? What does a run cost? | No: half is `std` (its backtrace symbolizer alone ~173 KiB), and untouched pages are never faulted in. Budget raised to 1.5 MB. A small run: ~0.7 ms our CPU, 2.85 MB peak RSS; 65,536 outputs: 3.39 MB (42.5 MB before streaming). | [binary-size.md](binary-size.md) |
| 11 | What do desktop notifications cost? | Within every target: final notice ~75 µs of our CPU, peak RSS ~2.9 MB with or without `--notify`, +86 KB binary. | [notifications.md](notifications.md) |
| 12 | What does the wheel cost end to end? | On the tuned reference machine: 755 µs of our CPU from `main` to the wheel's commit, no idle wakeups, peak 2.5 MB PSS (RSS 4.7 MB, of which 2.2 MB shared libc), 879 KB binary (+137 KB, 65 KB of it embedded data). | [wheel.md](wheel.md) |
| 13 | What does sandboxing `--manifest` cost? | Within budget: +~0.22 ms until `spawn` returns (p50 ~290 µs vs ~65 µs), +~0.16 ms until the output is read (p50 ~1.6 ms vs ~1.43 ms); paid once per new or changed binary, never on a cache hit. | [manifest-sandbox.md](manifest-sandbox.md) |
| 14 | Does the same wheel work on ordinary niri and Plasma systems, and what changes its cost? | Manual conversion/Undo checks pass on both, without product changes. Startup CPU exceeds 1 ms; energy policy and two buffer paints explain much of the cost. Full specifications and auditable run data. | [wheel-portability.md](wheel-portability.md) |

## Reference machine

All numbers are from one machine unless stated otherwise. It is **tuned,
not stock**, which matters for some results:

| | |
|---|---|
| CPU | AMD Ryzen 7 5700X (Zen 3, 8C/16T), **all-core OC 4650 MHz @ 1.1 V**, `performance` governor (fixed max clock) |
| RAM | 96 GB DDR4-3200, dual channel |
| GPU | NVIDIA RTX 3080 Ti Founders Edition, 12 GB; **`nvidia-open` kernel module 615.71.09**; power limit 280 W (stock 350 W), +75 MHz core, +500 MHz memory |
| OS | CachyOS (Arch-based), kernel 7.2.9-cachyos: BORE/EEVDF scheduler, `PREEMPT_DYNAMIC`, packages built for x86-64-v3 |
| Kernel flags | **`mitigations=off`**, `tsc=reliable clocksource=tsc`, `nowatchdog` |
| Compositor | niri 26.04 |
| Display | MSI MPG491CX OLED, 5120x1440 @ 240 Hz, scale 1. VRR is on-demand: fixed 240 Hz on the desktop, variable only for fullscreen games |

**How the tuning skews the numbers:**

- `mitigations=off` makes syscalls and context switches cheaper. Our
  microsecond figures (spawn, wire protocol, AT-SPI round trips) are
  syscall-heavy, so expect them to be **somewhat higher on a stock
  kernel**.
- A fixed, overclocked CPU clock lowers both CPU time and its variance.
  On a laptop with frequency scaling, the first invocation after idle
  will be slower.
- The GPU tuning only affects the GPU backends in
  [render-backends.md](render-backends.md), which we did not choose.
- The original compositor-side numbers (presentation time, enter-event
  latency) are niri-specific. The later [portability report](wheel-portability.md)
  adds a ThinkCentre on EndeavourOS/niri and an X280 on CachyOS/Plasma; it
  distinguishes client commit timing from actual presentation.

## Additional measured systems

The 2026-10-25 [niri/Plasma report](wheel-portability.md#machines-and-operating-conditions)
records complete hardware, firmware, package, display and power-policy
specifications for the ThinkCentre M910q and ThinkPad X280. Both use
frequency scaling and enabled mitigations. Their results must not be
pooled with the tuned desktop or treated as a BORE comparison.

## Budgets

These measurements set the budgets new code must stay within (see
`CLAUDE.md`). Spikes double as regression checks.
