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
2. **Measure everything.** Every function gets grilled: what does it cost, can
   it cost less, what is the leanest way to do it. Claims need numbers.
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
- Wayland: `smithay-client-toolkit` + wlr-layer-shell overlay.
- Rendering: `tiny-skia` into `wl_shm` buffers (CPU).
- First compositor adapter: niri (`niri-ipc`).
- Process model: TBD. Spawn-per-use is favored (measured cold start
  3.6 ms exec to first frame), which means zero idle cost.

## Conventions

- Run `cargo fmt` and `cargo clippy` after changes.
- Prefer `cargo check` / narrow tests before the full suite.
- No `unsafe` without a documented invariant.
