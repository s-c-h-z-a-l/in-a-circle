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
   - peak memory: <= 5 MB PSS (proportional set size: shared libc and
     loader pages count by their share; RSS is reported alongside)
   - binary: `inacircle` <= 1.5 MB; each family binary <= 1 MB of code
     (embedded data such as fonts is justified and measured separately).
     A plan that grows a binary by more than 100 KB explains why.
   - dependencies of shipped binaries: `libc` only, unless justified by a
     measurement. Build- and test-time tools may use a few pinned crates
     (e.g. offline atlas baking, test-suite loaders); a check keeps them
     out of the shipped binaries' dependency tree.
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
- **Process files stay private.** Specs, implementation plans, review
  notes and other working documents produced by skills or agents go to
  `notes/` (e.g. `notes/specs/`, `notes/plans/`), never into the repo.
  The public repo holds code, `README.md`, `CLAUDE.md` and `docs/` only:
  settled, concise, readable. No long generated text files.

## Git

- Commit often, without asking.
- **One branch per plan (stage).** Every implementation plan runs on its
  own branch off `main`. Merge it into `main` only at the very end, after
  the plan's closing review, and only if every success criterion holds
  (all tests, fmt, clippy, dependency check, budgets). Otherwise it stays
  on its branch.
- Plan branches may always be pushed. `main` gets a plan only by the merge
  above, never by a direct push.
- Commit messages: technical, terse, clear, easy to follow. Imperative
  subject line, body only when the "why" isn't obvious.

## Cloud workflow (while the cloud credit lasts, until 2026-11-04)

The full step list and scripts are private in `notes/cloud/` (`WORKFLOW.md`).
In short:

- The local session is the controller: plans, rulings, the ledger, local
  verification, merges. Implementers and reviewers run as cloud sessions
  (`claude --cloud`), one at a time on the plan branch, with the whole task
  brief inline (they cannot see `notes/`).
- Cloud sessions push code to the plan branch and their reports to a
  temporary `planN-reports` branch, deleted when the plan closes. A
  background watcher on that branch wakes the controller.
- Fix rounds go to the same implementer session
  (`claude -p "…" --cloud <session-id>`); scoped re-reviews to the same
  reviewer session.
- When the user is away, keep going task by task the same way. Stop only
  for a decision that is the user's (budgets, architecture, sandbox), at
  the end of a plan, or when the local quota runs out.
- The final word is always the local run (Landlock, the real machine, niri,
  the real notification server). After the credit: the same loop with local
  subagents.

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
  now; scale 2 later), embedded in the binary. No runtime font parsing.
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

## Testing

Test behavior, not lines. Coverage is a signal, never a target.

- Required: every public interface's contract (normal case), every
  documented edge case (empty input, limits, bad encoding), every error
  path a user can hit, every safety boundary (sandbox, size limits, never
  overwrite), and a regression test for every fixed bug (one that fails
  without the fix).
- Not required: trivial getters and constructors, or what the compiler
  already guarantees. Test internals through the public interface.
- Effort follows risk: parsers get official test suites and edge cases
  (later fuzzing); engine logic and the action contract get unit and
  integration tests; Wayland/UI code gets unit tests for its pure logic
  (geometry, hit testing, damage) plus a manual checklist on niri.
- Measure line coverage per crate with `cargo llvm-cov` at the end of each
  plan and report it. Rough signal bands: parsers ~90%, engine and
  contract ~80%, UI lower. Far below its band is a question to answer
  ("what isn't tested, and why?"), not an automatic failure.
