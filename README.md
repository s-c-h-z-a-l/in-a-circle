# In.A.Circle

> Everything is one gesture away.

A context-aware radial action menu for Linux on Wayland, like the GTA
weapon wheel. Press a hotkey and a wheel appears at the cursor, offering
only the actions that apply to what you're working on: the files you're
dragging, the text you selected, the URL on your clipboard.

Don't look for the app that does the job. Grab the thing and do it in one
gesture.

## Status

**Early development. There is nothing to install yet.**

Every architectural decision has been measured first, and the reports are
public in [`docs/benchmarks/`](docs/benchmarks). The engine runs headless
(a command line that classifies files, runs sandboxed actions, undoes
them and reports through desktop notifications), and `inacircle` without
arguments is the wheel: drag a file, press the hotkey, drop it on a category
(Document), then on a target (Markdown, Plain text).
The same binaries have passed manual conversion and notification Undo
checks on [niri](https://github.com/YaLTeR/niri) with Nautilus and on
Plasma Wayland with Dolphin. [Measurements on both systems](docs/benchmarks/wheel-portability.md)
include the performance limits that remain open. There are no packages
or presets worth installing yet. The first usable version is a **File Circle**.

## How it will work

**Files:** grab a file (or several) in your file manager, press the hotkey
while dragging, and drop it on a slice: convert, compress, resize, share.
This works over the standard Wayland drag-and-drop protocol, so it works
with any file manager.
([measured](docs/benchmarks/drag-into-circle.md) with Nautilus and Thunar.)

**Context decides.** A PNG won't get currency conversion. A URL gets link
actions, selected text gets text actions. Classification is deterministic,
so the same input always gives the same wheel, and your muscle memory
holds.

## Why

[Tangerine](https://tangerineformac.com/) is a radial file converter for
macOS. One reel about it got around a million likes, and the comments
kept asking "is there one for Linux?". [Filee](https://github.com/KnifeLemon/Filee) brings one
to Windows and, as a preview, Linux, but its drag gesture needs X11; on
Wayland it falls back to a drop zone and the right-click menu.

In.A.Circle is built for Wayland from the start, and not only for files.

## Principles

- **Zero idle footprint.** No resident process: the wheel starts on the
  hotkey and exits when done.
- **Tiny.** Our own Wayland wire protocol and our own rasterizer, with
  `libc` as the only dependency. The spikes come in around 350 KB.
- **CPU, not GPU.** On NVIDIA a GPU context costs 70-90 MB of idle RSS for
  a ~2 ms gain on a static overlay. ([details](docs/benchmarks/render-backends.md))
- **Standard protocols only.** No compositor IPC and no per-compositor
  adapters. Developed on niri first; other compositors are a matter of
  testing, not rewriting.

Measured on a tuned desktop ([reference machine](docs/benchmarks/README.md#reference-machine)); expect somewhat higher numbers on stock systems.

| | measured |
|---|---|
| idle cost | 0 (no process) |
| our CPU, start to wheel committed | ~0.5 ms (process start adds 0.3-1.9 ms, depending on what spawns it) |
| wheel on screen after start | 10-19 ms; the wheel is committed at ~1.6 ms, most of it waiting for the compositor's pointer enter. The rest is compositor scheduling |
| hover repaint | ~14 µs |
| peak memory | ~3.5 MB |

## Repository

- [`docs/architecture.md`](docs/architecture.md): how it is put together,
  and why
- [`docs/benchmarks/`](docs/benchmarks): the measurements and the decisions
  they drove
- [`spikes/`](spikes): throwaway code that produced those numbers (not the
  product)

## Contributing

Too early for code contributions. Ideas, questions and "would this work
on my setup?" are welcome as [issues](https://github.com/s-c-h-z-a-l/in-a-circle/issues). Once the first
version runs, testing on other compositors and hardware will be the most
valuable help.

## License

[MIT](LICENSE)
