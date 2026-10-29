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
   investigate and fix. Within them, don't chase invisible microseconds.
   Absolute timing budgets apply to the recorded reference environment;
   other environments require their own baselines and regression checks.
   An environment includes CPU, kernel, compositor, launcher, power policy,
   binary/build identity and cache condition. Safety and resource ceilings
   remain mandatory on every supported system. Full acceptance method:
   `docs/benchmarks/acceptance.md`.
   - our CPU from wheel-mode entry to wheel commit: p95 <= 1 ms on the
     reference environment. Waiting for the compositor (configure, pointer
     enter) and the cost before `main`
     (exec, loader, the spawner's teardown: ~1 ms under niri) are measured
     and reported, not budgeted. Do our work during compositor waits.
   - our CPU from `main` to exit, excluding per-event work (hover, motion):
     p95 <= 1 ms on the reference environment. This covers reading the
     payload, classifying, and starting the action. The wheel splits this
     into its waiting points, each p95 <= 1 ms of our CPU for one input:
     wheel-mode entry to the wheel's commit, the early list to level 1,
     the drop's data to the stage-2 wheel's commit, and the choice to the
     action's start. Per-input work beyond one input scales and is
     reported, not budgeted. CPU before `main` (a launcher's `sh -c`, exec,
     loader) is reported, not budgeted.
   - hover repaint: <= 100 µs wall time per event on the reference
     environment; report p95 and maximum separately
   - while the wheel waits: no autonomous periodic work. Input/protocol
     events and finite safety deadlines may wake it. `idle_wakeups=0`
     counts only premature empty polls, not every possible wakeup; pair
     the counter with an idle observation and the absence of periodic work
   - idle cost: 0 (no resident process)
   - peak memory of `inacircle` (wheel, engine, supervisor): <= 5 MB PSS
     (5,000,000 B; Linux `kB` fields are KiB, multiply by 1024)
     (proportional set size: shared libc and loader pages count by their
     share; RSS is reported alongside). Actions are not held to it: they
     stream where the format allows, elsewhere hold at most one unit
     (a page, an image, a chapter), and enforce documented input limits
     on the data they actually produce, not only on declared sizes. The
     sandbox's `RLIMIT_DATA` net (RAM/8, 1-4 GiB, see
     `docs/resource-limits.md`) catches a limit that is wrong or missing.
   - binary: `inacircle` <= 1.5 MB; each family binary <= 1 MB of code
     (embedded data such as fonts is justified and measured separately).
     A plan that grows a binary by more than 100 KB explains why.
   - dependencies of shipped binaries: `libc` only, unless justified by a
     measurement. Build- and test-time tools may use a few pinned crates
     (e.g. offline atlas baking, test-suite loaders); a check keeps them
     out of the shipped binaries' dependency tree.
   - timing acceptance: >= 50 valid observations per metric and condition;
     p95 is sorted sample `ceil(0.95 * N)`, one-based (nearest-rank).
     Preserve valid outliers; document every failed/missing attempt.
   - regression against an environment-matched baseline: > 10% p95
     worsening requires investigation; > 15%, reproduced in an independent
     comparison and not explained by measured noise, blocks merge. These
     are provisional policy thresholds, not values derived from the old
     measurements. Calibrate with repeated A/A series; compare A/B in
     alternating order where possible. Unresolved excess noise or missing
     evidence leaves acceptance open. Never average environments together
     or relax sandbox, input limits, deadlines or overwrite protection to
     accommodate a slower machine.
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

## Workflow

- Plans are written to `notes/plans/` and run task by task: an
  implementer subagent per task, then a reviewer, fix rounds with the same
  implementer, a closing whole-branch review, then the local verification
  and the merge. The local session is the controller: plans, rulings, the
  ledger, verification, merges. Subagents run locally on the user's quota:
  keep briefs tight and reviews proportionate to risk.
- When the user is away, keep going task by task the same way. Stop only
  for a decision that is the user's (budgets, architecture, sandbox), at
  the end of a plan, or when the quota runs out.
- Before deleting a branch, prove it is merged
  (`git merge-base --is-ancestor <branch> origin/main`); archive anything
  that is not code before deleting it. Each plan's merge gets a `plan-N` tag.
- The final word is always the local run (Landlock, the real machine, niri,
  the real notification server).

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
- Actions: every action and manifest probe runs under the mandatory
  sandbox (Landlock ABI >= 6 and seccomp, x86_64 and aarch64); there is
  no weakened mode. An output group is published per input by one
  no-replace rename, and Undo moves it to the freedesktop.org Trash; the
  engine never deletes a published output. A new action that needs a call
  the policy denies needs a decision, not an exception. Contract and
  limits: `docs/architecture.md`, `docs/benchmarks/sandbox-hardening.md`.

## Conventions

- Run `cargo fmt` and `cargo clippy` after changes.
- Prefer `cargo check` / narrow tests before the full suite.
- Enable the pre-commit hook once per clone: `git config core.hooksPath scripts/git-hooks`
  (checks `cargo fmt` and that every target compiles; never use `--no-verify`).
- No `unsafe` without a documented invariant.
- No magic numbers: every limit, deadline and threshold states its reason
  (why this value, derived from what, what happens past it) in a code
  comment and in the commit body.
- Every new action comes with its MIME type in the classifier, a preset
  and group in the menu, a refreshed catalog cache, and a fake-compositor
  test that the wheel offers and runs it. `lossy` means the content can
  lose something; anything an action may drop makes it `lossy=yes`.
- Every new shipped binary is added to `scripts/shipped-bins` in its
  first commit, so the dependency and size guards cover it.

## Testing

Test behavior, not lines. Coverage is a signal, never a target.

- Required: every public interface's contract (normal case), every
  documented edge case (empty input, limits, bad encoding), every error
  path a user can hit, every safety boundary (sandbox, size limits, never
  overwrite), and a regression test for every fixed bug (one that fails
  without the fix).
- Not required: trivial getters and constructors, or what the compiler
  already guarantees. Test internals through the public interface.
- Effort follows risk: parsers get official test suites and edge cases;
  engine logic and the action contract get unit and integration tests;
  Wayland/UI code gets unit tests for its pure logic (geometry, hit
  testing, damage) plus a manual checklist on niri.
- Fuzzing: every parser that reads foreign input gets a `cargo-fuzz`
  target (nightly, test-time only) that also watches time, depth and
  memory, not just panics. Every finding becomes a regression test in the
  normal suite. Campaigns are reproducible: pinned oracle, seed corpus,
  stated run time, minimized failing input.
- Differential tests: where a recognized reference or official suite
  exists, our output is checked against it as a pinned dev-dependency.
  An encoder round-tripped through our own decoder is not independent
  evidence. Where no reference exists, the plan says what we compare to.
- Mutation testing (`cargo-mutants`) at the end of a plan, only on the
  code the plan changed, with a time limit. Surviving mutants are reported
  in the outcome; each is a question, not an automatic failure.
- A plan that changes a budgeted path (start, hover, memory, wakeups)
  also measures it on a second, untuned machine (stock kernel), and the
  numbers go to the benchmark page. Record the entire environment and
  distinguish preliminary diagnosis from >= 50-sample p95 acceptance;
  report absolute overruns even outside the reference timing gate.
- The closing verification runs the suite with
  `INACIRCLE_REQUIRE_LANDLOCK=1`.
- Tests never touch the user's home. Cargo runs every test binary through
  `scripts/test-runner.sh` (`.cargo/config.toml`): HOME and the XDG dirs
  point into a honeypot, and the run fails if anything lands there or the
  real state or download folder changes. A spawned `inacircle` gets its
  paths from `isolate()` (`bins/inacircle/tests/isolate`); an engine test
  sets the environment only through `xdg::tests::with_env`, which points
  every user path it was not given at a place nothing can be made in.
- CI (`.github/workflows/ci.yml`) is a safety net, not the authority: the
  final word is the local run.
- Measure line coverage per crate with `cargo llvm-cov` at the end of each
  plan and report it. Rough signal bands: parsers ~90%, engine and
  contract ~80%, UI lower. Far below its band is a question to answer
  ("what isn't tested, and why?"), not an automatic failure.
