# The wheel: what a real run costs

**Question:** does the shipped wheel (`inacircle` without arguments) stay
within the budgets: our CPU from `main` to the first commit of the mapped
wheel ≤ 1 ms, hover repaint ≤ 100 µs, no wakeups while it waits, peak memory
≤ 5 MB PSS, binary ≤ 1.5 MB?

**Historical reference-machine verdict:** within budget. Our CPU from `main` to the wheel's commit is
755 µs p50 (856 µs max, budget 1 ms), there are no wakeups while the wheel
waits, and peak memory is 2.5 MB PSS (budget 5 MB). RSS reads 4.7 MB, but
2.2 MB of it is libc and loader pages shared with every process; our own
part is the two 900 KB shm buffers, 160 kB of heap and the binary's pages.
A drop of 1024 files adds about 120 kB at stage 2. The initial hover row
is the rasterizer bench (14 µs median, 49 µs max); in-situ drag results
follow below.

**Portability update (2026-10-25):** the same plan-7 binaries passed manual
conversion tests on EndeavourOS/niri/Nautilus and CachyOS/Plasma/Dolphin,
including notification Undo. Startup CPU exceeded 1 ms on both additional
systems; the X280 also exceeded the early-list and hover limits. Memory
and counted idle wakeups remained within their limits. See the
[cross-machine report](wheel-portability.md) for full specifications,
controls and raw records. The [acceptance policy](acceptance.md) now defines
reference/environment scope, ≥50 observations, nearest-rank p95 and
regression thresholds. The historical reference numbers here are not a
universal budget claim or p95 acceptance under that policy.
The [fresh reference report](wheel-reference.md) supplies three complete
50-run startup CPU p95 checks with environment and failure records.

## Method

`INACIRCLE_MEASURE=1` makes the wheel print one line on stderr when it
closes (nothing else changes; unset, it costs one `getenv`):

```
inacircle: measure wheel main_to_commit_cpu_us=… main_to_commit_wall_us=… hover_events=… hover_us_p50=… hover_us_max=… idle_wakeups=… pss_kb=… vm_hwm_kb=…
```

- `main_to_commit_cpu_us`: our `getrusage(RUSAGE_SELF)` user + system CPU
  from the start of the wheel mode to the first `wl_surface.commit` of the
  mapped wheel. Exec, loader and the spawner's teardown are before it and
  not counted; the wall time is the same interval, so it includes waiting
  for the compositor. The commit requests are queued at the marker; this
  is neither a socket-flush nor a presentation timestamp.
- `hover_us_*`: elapsed wall time (`Instant::elapsed`) of each hover
  repaint (paint, damage, requests),
  `hover_events` of them; p50 is the lower middle.
- `idle_wakeups`: `poll` returns with nothing to do while the wheel waits
  (before its deadline). With no input it must be 0.
- `pss_kb`: the highest proportional set size sampled after a full paint
  (`/proc/self/smaps_rollup`): the budget's measure. Shared libc and loader
  pages count by their share; the kernel keeps no PSS peak, so the surface
  samples it.
- `vm_hwm_kb`: peak RSS, reported alongside (it counts shared pages whole).
- Binary size: the stripped release `inacircle` against `main` at 9e30484
  (738,104 B).

## Machine

The [reference machine](README.md#reference-machine), niri.

## Results

Runs: 20 starts from a terminal under niri, a plain start (`Nothing to do`),
closed by Esc sent with `wtype` 0.6 s later, p50 and max. Hover and wakeups: 5 runs of a drag from Nautilus, 10 s of moving
over the slices, then 10 s with the pointer still.

| | p50 | max | budget |
|---|---|---|---|
| `main_to_commit_cpu_us` | 755 | 856 | ≤ 1000 |
| `main_to_commit_wall_us` | 1115 | 2688 | reported |
| `hover_us` (per event, rasterizer bench) | 14 | 49 | ≤ 100 |
| `idle_wakeups` (no input) | 0 | 0 | 0 |
| `pss_kb` (KiB) | 2545 | 2575 | historical ≤ 5000 KiB |
| `vm_hwm_kb` (reported) | 4630 | 4820 | — |

The `hover_us` row is `cargo test -p ui --release -- --ignored` (the
rasterizer alone), and the `idle_wakeups` row comes from the plain starts.
The historical 5000 KiB table threshold is 5,120,000 B. Current acceptance
uses the stated 5 MB ceiling as 5,000,000 B; see the
[memory method](acceptance.md#memory-and-idle-evidence). The observations
above are below both, but those thresholds are not interchangeable.

### Drag runs (the user's manual test on niri, Nautilus)

The historical wheel waiting points for one input (CPU except the
explicitly wall-time hover row; the budget is per point):

| point | one run | budget |
|---|---|---|
| `main` → the wheel's commit | 828 µs | ≤ 1 ms |
| the drop's data → the stage-2 wheel's commit | 260 µs | ≤ 1 ms |
| the early list → the lit level-1 wheel's commit (`list_to_level1_cpu_us`) | see below | ≤ 1 ms |
| choice → classification complete (`run` start − `choice_abs` + catalog + classify; see below) | 303 µs | legacy proxy |
| hover repaint, in situ (after the stage-2 prepaint fix; was ~150 µs) | 17–23 µs | ≤ 100 µs |
| per input event (pointer, drag motion), from 4 runs of 109–941 events | ~4.3 µs | per event |
| peak memory | 2.6 MB PSS (RSS 5.06 MB) | ≤ 5 MB PSS |

The choice expression ends at the classification mark, before recorder
setup and executor invocation. It was formerly described as action start;
it does not independently establish the time of the child's `execve`.

`list_to_level1_cpu_us` counts our CPU from the early list's arrival
(counted only when the machine accepts it) to the first commit of the lit
level-1 wheel; the span includes parsing, classifying and building the
menu. Release binary against the fake compositor (this cloud VM), one file,
8 runs: median 320 µs, max 358 µs (budget 1 ms). The niri numbers come
from the user's manual test and are not recorded here yet.

#### Startup paint moved, not removed (hardening branch)

The second shm buffer is no longer painted before the first commit (the
wheel it would show is replaced at once by the pointer or the drag).
It is first painted at the stage-1 swap or by the prepaint after the
first commit. Release binary against the fake compositor, 10 alternating
runs, same machine, medians (ranges in parentheses):

| point | before | after | change |
|---|---|---|---|
| `main` → the wheel's commit (`main_to_commit_cpu_us`) | 639 µs (600–696) | 354 µs (343–375) | -285 µs |
| the early list → the lit level-1 wheel's commit (`list_to_level1_cpu_us`) | ~155 µs (130–207) | ~335 µs (304–345) | +~180 µs |

Part of the saving moved to the swap: the first use of buffer 1 touches
its pages and does a full paint there. Both points stay well under their
1 ms budget; the net is about 105 µs on the drag path, and the full
285 µs on a path without a list (the buffer is painted after the commit,
outside the window). The hover maximum (about 100 µs, the full paint of a
just-released stale buffer in the first events after a swap) is unchanged
within noise; it is not a result of that change.

Before `main`, the niri bind's `sh -c` cost ~2.2 ms of CPU charged to the
same PID (it `exec`s us); from a terminal that part is ~0.5 ms. A bind
without a shell avoids it. A 1024-file drop takes ~3 ms from the drop to
stage 2 (about 3 µs per file to classify), reported, not budgeted.

### A large drop

The drop path classifies every dropped file while both shm buffers are
mapped, so a large drop is the memory worst case. Measured by
`a_large_drop_stays_within_the_memory_budget` (`bins/inacircle/tests/wheel.rs`):
the release binary against a fake compositor (Unix socket, registry,
layer-shell configure, data device), 1024 small text files dropped as one
`text/uri-list` (~31 KB, under the 64 KiB cap), the catalog cache primed
with one action; `Pss` read from `/proc/<pid>/smaps_rollup` once stage 2
is drawn (the cloud runs below read `VmHWM`, before the budget moved to
PSS; on the reference machine: PSS 2281 kB for 1 file, 2403 kB for 1024). Five runs, cloud VM (not the reference machine), so the absolute
numbers are indicative; the difference between the rows is the drop's own
cost.

| drop | VmHWM min | max | budget |
|---|---|---|---|
| 1 file | 4824 kB | 4840 kB | ≤ 5120 kB |
| 1024 files | 4916 kB | 4976 kB | ≤ 5120 kB |

Before the slimming (the raw bytes kept through classification, every
file's name and type and canonical path held): 1024 files peaked at
5096 kB, 1 file at 4828 kB. The drop now frees the URI bytes first, tallies
types (count and one name each) and dedups by a hash of the canonical path.
What remains of the difference is mostly the dropped paths themselves
(needed for the run) and file-backed pages that vary by tens of kB between
runs.

### Binary size

Stripped release build, `cargo build --release -p inacircle`:

| | bytes |
|---|---|
| `main` (9e30484) | 738,104 |
| plan 4 (this branch) | 880,760 |
| growth | +142,656 (+19.3%) |

Sections of the plan 4 build (`size -A`):

| section | bytes | notes |
|---|---|---|
| `.text` | 655,038 | code: wire client, rasterizer, wheel, URI parser, engine |
| `.rodata` | 126,388 | includes the rasterizer's coverage table (~49 KB), the glyph atlas (12,317 B, `atlas.bin`), the LUTs (~4 KB), the row extents and splits (~2.4 KB) |
| `.eh_frame` + hdr + `.gcc_except_table` | 55,632 | unwinding tables, kept by `std` |
| `.data.rel.ro`, `.rela.dyn`, `.dynsym`, `.dynstr`, `.data` | ~40,000 | relocations and vtables |

The plan allowed +100 KB; the wheel is 43 KB over it (the final-review
fixes added 5 KB: the cache-only catalog load, the per-type tally and the
compass-centred slice runs). The embedded data
(coverage table, atlas, LUTs, ~65 KB) is justified and counted apart, as the
budget says; the rest is code for the wire client, the layer-surface and
drag handling, the state machine and the URI parser (`main`'s per-section
split was not recorded, so the code share is the remainder). The binary is
59% of the 1.5 MB budget.

## Reproduce

Build with `cargo build --release -p inacircle`. Bind it in niri
(`config.kdl`):

```
binds {
    Mod+Space { spawn "sh" "-c" "INACIRCLE_MEASURE=1 exec /path/to/inacircle 2>>/tmp/wheel.log"; }
}
```

or from a terminal, after focusing another window and with the pointer on
it: `sleep 2; INACIRCLE_MEASURE=1 ./target/release/inacircle`.

Each run:

1. **Start cost (20 runs):** press the key and wait for the wheel
   (`Nothing to do`), then press Esc.
2. **Hover and wakeups (5 runs):** start a drag of one file in Nautilus,
   press the key, drop on the wheel, move the pointer over all slices for
   10 s, keep it still for 10 s, press Esc.

Take the `measure wheel` lines from the log; p50 and max over the runs.

**Large drop:** `cargo test --release -p inacircle --test wheel --
a_large_drop_stays_within_the_memory_budget --nocapture` reports the lower
of two stage-2 PSS samples for each workload and asserts that the 1024-file
minimum is ≤5120 KiB (5,242,880 B). A debug build checks only the drop's
increment. This smoke neither records every peak nor enforces the exact
5,000,000 B acceptance ceiling; a passing result is not sufficient
[memory acceptance evidence](acceptance.md#memory-and-idle-evidence).
