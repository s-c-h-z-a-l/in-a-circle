# Startup latency: where the time goes

**Question:** from exec to photons, what costs what, and how much of it
is ours?

**Verdict:** our own work is ~0.6 ms. The rest belongs to the compositor:
spawning us, and above all presenting a newly mapped surface.

## Method

Spike: [`spikes/latency/`](../../spikes/latency) (`probe`, `launch`).
`probe` uses wayland-client directly (no SCTK). It timestamps each phase in
`CLOCK_MONOTONIC` and gets real scanout time from `wp_presentation`, not
from the frame callback. `launch` starts it either directly or through
the compositor's spawn path (`niri msg action spawn`, the same code path a
keybind uses, minus input handling). 30 runs each.

## Results (p50)

| phase | time |
|---|---|
| exec to `main` (direct) | 0.47 ms |
| `main` to connect | 0.02 ms |
| registry roundtrip + binds | 0.13 ms |
| shm buffer (480x480) | 0.25 ms |
| layer surface configure | 0.2 ms |
| **`main` to first commit (ours)** | **~0.6 ms** |
| commit to frame callback | 0.8 ms |
| **commit to presented (new surface)** | **~15 ms** |
| commit to presented (update of a mapped surface) | 4.6 ms (p90 7.1) |
| `niri msg` itself (exec + IPC) | 4.9 ms |
| niri spawn to our `main` (excluding `niri msg`) | ~4-4.5 ms |

### Findings

- **A frame callback is not presentation.** On niri, a newly mapped surface
  reaches scanout ~10 ms later than an update to an existing one.
- Spawning through the compositor costs ~4 ms before our code runs.
- An own wire protocol could save at most 0.1-0.2 ms here, so it is not a
  latency decision (see [wire-protocol.md](wire-protocol.md)).

## Startup CPU: ours vs the spawner's

Measured later with the [integration probe](drag-into-circle.md#integration-probe-the-real-surface-layout)
(`getrusage` at each step) and a no-op binary that only reads its CPU
time at `main`. 5-15 runs each, p50.

| | CPU at `main` | `main` to wheel commit | exec to commit |
|---|---|---|---|
| probe, spawned by niri (keybind) | 1.52-1.94 ms | 0.45-0.52 ms | 2.0-2.5 ms |
| probe, spawned by an interactive zsh | 0.58-0.63 ms | 0.44-0.49 ms | 1.06-1.11 ms |
| no-op, dynamic, via `niri msg action spawn` | 1.54 ms | | |
| no-op, static (glibc), via niri | 1.45 ms | | |
| no-op, dynamic, from a shell | 0.50 ms | | |
| no-op, static (glibc), from a shell | 0.34 ms | | |

- **Our work is ~0.47 ms CPU** from `main` to the wheel commit, whoever
  spawns us. ~0.4 ms of it is the first paint (mostly page faults on the
  fresh buffer), done while the compositor works out the cursor.
- **Before `main`, the cost depends on the parent.** niri's spawn path adds
  ~1 ms to any binary, even a no-op. Most likely the child inherits a copy
  of niri's large address space at fork and tears it down at exec; not
  verified in niri's source.
- **Static linking saves ~100-150 µs** before `main`, but a static glibc
  binary is 1.25 MB, over the 500 KB budget. musl is untested.
- Hence the budget counts from `main`; the pre-`main` cost is measured and
  reported, not budgeted.

## Reproduce

```sh
cd spikes/latency && cargo build --release
./target/release/launch direct 30          # exec directly
./target/release/launch niri 30            # through niri's spawn path (niri only)
./target/release/launch direct 30 --noop   # exec floor
```
