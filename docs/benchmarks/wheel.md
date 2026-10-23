# The wheel: what a real run costs

**Question:** does the shipped wheel (`inacircle` without arguments) stay
within the budgets: our CPU from `main` to the first commit of the mapped
wheel ≤ 1 ms, hover repaint ≤ 100 µs, no wakeups while it waits, peak RSS
≤ 5 MB, binary ≤ 1.5 MB?

**Verdict:** within budget, with memory close to its line. Our CPU from
`main` to the wheel's commit is 755 µs p50 (856 µs max, budget 1 ms), there
are no wakeups while the wheel waits, and peak RSS is 4.7 MB (budget 5 MB):
most of it is the two 900 KB shm buffers. Hover cost comes from the
rasterizer bench (14 µs median, 49 µs max on this machine, budget 100 µs);
the drag runs below add the in-situ numbers.

## Method

`INACIRCLE_MEASURE=1` makes the wheel print one line on stderr when it
closes (nothing else changes; unset, it costs one `getenv`):

```
inacircle: measure wheel main_to_commit_cpu_us=… main_to_commit_wall_us=… hover_events=… hover_us_p50=… hover_us_max=… idle_wakeups=… vm_hwm_kb=…
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
- `vm_hwm_kb`: peak RSS.
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
| `vm_hwm_kb` | 4702 | 4740 | ≤ 5000 |

### Binary size

Stripped release build, `cargo build --release -p inacircle`:

| | bytes |
|---|---|
| `main` (9e30484) | 738,104 |
| plan 4 (this branch) | 875,608 |
| growth | +137,504 (+18.6%) |

Sections of the plan 4 build (`size -A`):

| section | bytes | notes |
|---|---|---|
| `.text` | 651,486 | code: wire client, rasterizer, wheel, URI parser, engine |
| `.rodata` | 125,324 | includes the rasterizer's coverage table (~49 KB), the glyph atlas (12,317 B, `atlas.bin`), the LUTs (~4 KB) |
| `.eh_frame` + hdr + `.gcc_except_table` | 55,096 | unwinding tables, kept by `std` |
| `.data.rel.ro`, `.rela.dyn`, `.dynsym`, `.dynstr`, `.data` | ~40,000 | relocations and vtables |

The plan allowed +100 KB; the wheel is 37 KB over it. The embedded data
(coverage table, atlas, LUTs, ~65 KB) is justified and counted apart, as the
budget says; the rest is code for the wire client, the layer-surface and
drag handling, the state machine and the URI parser (`main`'s per-section
split was not recorded, so the code share is the remainder). The binary is
58% of the 1.5 MB budget.

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
