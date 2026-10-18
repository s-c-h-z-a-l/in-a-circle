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

## Reproduce

```sh
cd spikes/latency && cargo build --release
./target/release/launch direct 30          # exec directly
./target/release/launch niri 30            # through niri's spawn path (niri only)
./target/release/launch direct 30 --noop   # exec floor
```
