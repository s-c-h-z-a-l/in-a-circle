# The wheel: what a real run costs

**Question:** does the shipped wheel (`inacircle` without arguments) stay
within the budgets: our CPU from `main` to the first commit of the mapped
wheel ≤ 1 ms, hover repaint ≤ 100 µs, no wakeups while it waits, peak memory
≤ 5 MB PSS, binary ≤ 1.5 MB?

**Verdict:** within budget. Our CPU from `main` to the wheel's commit is
755 µs p50 (856 µs max, budget 1 ms), there are no wakeups while the wheel
waits, and peak memory is 2.5 MB PSS (budget 5 MB). RSS reads 4.7 MB, but
2.2 MB of it is libc and loader pages shared with every process; our own
part is the two 900 KB shm buffers, 160 kB of heap and the binary's pages.
A drop of 1024 files adds about 120 kB at stage 2. The hover row is the
rasterizer bench (14 µs median, 49 µs max, budget 100 µs), not an in-situ
number: the drag runs (hover and wakeups while dragging) are pending the
user's manual test on niri.

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
  for the compositor.
- `hover_us_*`: the time of each hover repaint (paint, damage, requests),
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
| `pss_kb` | 2545 | 2575 | ≤ 5000 |
| `vm_hwm_kb` (reported) | 4630 | 4820 | — |

The `hover_us` row is `cargo test -p ui --release -- --ignored` (the
rasterizer alone), and the `idle_wakeups` row comes from the plain starts.

### Drag runs (the user's manual test on niri, Nautilus)

The wheel's waiting points, our CPU for one input (the budget is per point):

| point | one run | budget |
|---|---|---|
| `main` → the wheel's commit | 828 µs | ≤ 1 ms |
| the drop's data → the stage-2 wheel's commit | 260 µs | ≤ 1 ms |
| the choice → the action's start (`run` start − `choice_abs` + catalog + classify) | 303 µs | ≤ 1 ms |
| hover repaint, in situ (after the stage-2 prepaint fix; was ~150 µs) | 17–23 µs | ≤ 100 µs |
| per input event (pointer, drag motion), from 4 runs of 109–941 events | ~4.3 µs | per event |
| peak memory | 2.6 MB PSS (RSS 5.06 MB) | ≤ 5 MB PSS |

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
a_large_drop_stays_within_the_rss_budget --nocapture` prints both peaks and
fails above 5 MB (a debug build checks only the drop's increment).
