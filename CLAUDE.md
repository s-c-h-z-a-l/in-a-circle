# In.A.Circle

> Everything is one gesture away.

A context-aware radial action menu (think GTA weapon wheel) for Linux/Wayland.
Press a hotkey, the wheel appears at the cursor and offers only the actions
that apply to what you're working on: selected text, a URL on the clipboard,
the files you're dragging, the focused window. Don't look for the app that
does the job — grab the thing and do it in one gesture.

It is a tool. A dead simple tool. Nothing extra.

**Target: zero idle footprint.** If it's measurable while you're not using it,
it's a bug.

## Rules

1. **Minimal resources by design, not by later optimization.** Before writing
   anything, think through what costs the least (CPU, RAM, VRAM, wakeups,
   dependencies, binary size, latency). We don't build first and trim later.
2. **Budgets, measured.** Architecture decisions need numbers (see
   `docs/benchmarks/`). New code must stay within these budgets; past them,
   grill and fix. Within them, don't chase invisible microseconds.
   - our CPU from `main` to wheel commit: <= 1 ms. Waiting for the
     compositor (configure, pointer enter) and the cost before `main`
     (exec, loader, the spawner's teardown: ~1 ms under niri) are measured
     and reported, not budgeted. Do our work during compositor waits.
   - our CPU from `main` to exit, excluding per-event work (hover, motion):
     <= 1 ms. This covers reading the payload, classifying, and starting
     the action.
   - hover repaint: <= 100 µs per event
   - while the wheel waits: no wakeups except input events
   - idle cost: 0 (no resident process)
   - peak RSS: <= 5 MB
   - binary: <= 500 KB
   - dependencies: `libc` only, unless justified by a measurement
3. **CPU rendering, damage-only.** Measured: a GPU context costs 70-90 MB RSS
   idle on NVIDIA for a ~2 ms gain on a static overlay. Redraw only what changed.
   (See `spikes/render-bench/`.)
4. **Grill me.** When the user proposes something, attack it. Find the flaws,
   the hidden costs, the simpler alternative. Kill weak ideas without mercy.
   Agreement must be earned.
5. **Language:** anything that goes to GitHub (code, comments, commits, docs/,
   this file) is in English. Anything that stays local (`notes/`, chat) is in
   Hungarian.

## Docs

- `notes/` is **private**, gitignored, never committed. Brainstorming, drafts.
- `docs/` is **public**. Settled design decisions and roadmap. Promote from
  `notes/` once decided. `docs/architecture.md` is the engine and the
  action contract; change it only by decision.
- Every benchmark that informs a decision gets a page in `docs/benchmarks/`
  (method, machine, numbers, verdict, how to reproduce), committed with it.

## Git

- Commit often, without asking.
- Branch, push, and merge into `main` **only when the user says so**.
- Commit messages: technical, terse, clear, easy to follow. Imperative
  subject line, body only when the "why" isn't obvious.

## Stack

- Rust. No GUI toolkit; the wheel is custom drawn.
- **Primary target: niri.** Developed and measured on niri first; it must
  work there before anything else. The code stays compositor-agnostic so
  porting is testing, not rewriting.
- **Compositor-agnostic.** Only standard Wayland protocols (core, stable,
  staging/ext, and wlr where widely implemented), detected at runtime with
  graceful degradation. No compositor IPC, no per-compositor adapters.
  The hotkey is the user's compositor keybind running `inacircle`.
- Wayland client: **own wire protocol**, `libc` only (see `spikes/wire/`).
  Same latency as wayland-client, 1 dependency instead of 18, 344 KB vs
  607 KB binary. Opcodes must be checked against the protocol XMLs.
  Events that carry an fd must take it even when ignored; treat
  `MSG_CTRUNC` as an error.
- Overlay: wlr-layer-shell.
- Text: alpha glyph atlas baked offline at the exact sizes used (scale 1
  and 2), embedded in the binary. No runtime font parsing.
- Rendering: own analytic rasterizer straight into `wl_shm` buffers (CPU),
  no rasterizer dependency. One-octant coverage table mirrored by the
  wheel's symmetry + LUT. Damage-only repaint, damaged against what is on
  screen, not against the buffer being drawn.
- Process model: **spawn-per-use** (measured: a resident daemon presents no
  faster, and the hotkey spawns a process either way). Zero idle cost.
- Surface: full-screen layer surface with a 1x1 buffer scaled by
  wp_viewporter (cursor position + focus), wheel in a subsurface with an
  **empty input region**, so all pointer and drag events land on the
  parent. Paint the wheel while waiting for the cursor position; only the
  subsurface position depends on it.
- File input: **drag into the circle** (`wl_data_device`): grab files,
  press the hotkey, drop on a slice. Accept copy only, never request the
  `DELETE` target. Off a slice, reject both the MIME type and the action
  (`accept(null)` + `set_actions(none)`); niri still sends `drop` after
  `accept(null)` alone. Close the wheel and release the lock before a
  long-running action starts. During a drag the position comes from
  `wl_data_device.enter`/`motion`, not `wl_pointer.enter`.
- Context: keyboard focus -> selection/clipboard offers; classify by MIME
  type, read data lazily.
- Safety: single instance (`flock`), and a hard deadline on every wait,
  data pipe reads included. A full-screen overlay that hangs locks the
  user out of the desktop.

## Conventions

- Run `cargo fmt` and `cargo clippy` after changes.
- Prefer `cargo check` / narrow tests before the full suite.
- No `unsafe` without a documented invariant.
