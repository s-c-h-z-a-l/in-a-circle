# In.A.Circle

> Everything is one gesture away.

A context-aware radial action menu (think GTA weapon wheel) for Linux/Wayland.
Press a hotkey, the wheel appears at the cursor and offers only the actions
that apply to what you're working on: selected text, a URL on the clipboard,
selected files, the focused window. Don't look for the app that does the job —
select the thing and do it in one gesture.

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
   - our time from exec to wheel commit: <= 2 ms
   - CPU per invocation: <= 1 ms
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
  `notes/` once decided.
- Every benchmark that informs a decision gets a page in `docs/benchmarks/`
  (method, machine, numbers, verdict, how to reproduce), committed with it.

## Git

- Commit often, without asking.
- Branch, push, and merge into `main` **only when the user says so**.
- Commit messages: technical, terse, clear, easy to follow. Imperative
  subject line, body only when the "why" isn't obvious.

## System

- **Never run `pacman -Sy`** (partial upgrade breaks the NVIDIA stack).
  If a package sync is needed, use `pacman -Syu` only.

## Stack

- Rust. No GUI toolkit; the wheel is custom drawn.
- **Compositor-agnostic.** Only standard Wayland protocols (core, stable,
  staging/ext, and wlr where widely implemented), detected at runtime with
  graceful degradation. No compositor IPC, no per-compositor adapters.
  The hotkey is the user's compositor keybind running `inacircle`.
- Wayland client: **own wire protocol**, `libc` only (see `spikes/wire/`).
  Same latency as wayland-client, 1 dependency instead of 18, 344 KB vs
  607 KB binary. Opcodes must be checked against the protocol XMLs.
- Overlay: wlr-layer-shell.
- Text: alpha glyph atlas baked offline at the exact sizes used (scale 1
  and 2), embedded in the binary. No runtime font parsing.
- Rendering: own analytic rasterizer straight into `wl_shm` buffers (CPU),
  no rasterizer dependency. Coverage mask + LUT, damage-only repaint.
- Process model: **spawn-per-use** (measured: a resident daemon presents no
  faster, and the hotkey spawns a process either way). Zero idle cost.
- Surface: full-screen layer surface with a 1x1 buffer scaled by
  wp_viewporter (cursor position + focus), wheel in a subsurface.
- Context: keyboard focus -> selection/clipboard offers; classify by MIME
  type, read data lazily.

## Conventions

- Run `cargo fmt` and `cargo clippy` after changes.
- Prefer `cargo check` / narrow tests before the full suite.
- No `unsafe` without a documented invariant.
