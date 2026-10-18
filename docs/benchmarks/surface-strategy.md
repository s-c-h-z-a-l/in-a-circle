# Surface strategy, and daemon vs spawn-per-use

**Question:** how do we get the wheel on screen *at the cursor*, with
keyboard focus, as fast and as cheaply as possible? Should a resident
daemon keep a surface ready?

**Verdict:** a full-screen layer surface with a **1x1 buffer scaled by
wp_viewporter**, with the wheel in a 480x480 **subsurface**.
**Spawn-per-use:** a daemon presents no faster.

## Method

Spike: [`spikes/latency/src/bin/surf.rs`](../../spikes/latency/src/bin/surf.rs).
Every mode ends in the same state: wheel visible at the cursor, with
keyboard focus. Time is measured from `main` to `wp_presentation`. Wayland
has no "where is the cursor" request, so the cursor position comes from
`wl_pointer.enter` on a full-screen surface. 20 runs per mode; the daemon
mode runs 15 triggers.

- `box`: 480x480 layer surface, wheel in the first commit (centered, no
  cursor position; reference only)
- `full`: full-screen shm buffer (29.5 MB at 5120x1440); map transparent,
  wait for enter events, then draw the wheel at the cursor
- `vp`: full-screen surface with a 1x1 buffer scaled by `wp_viewporter`;
  wheel in a subsurface at the cursor
- `daemon`: a persistent `vp` surface, click-through (empty input region)
  and unfocused while idle; a trigger re-enables input and focus

## Results

| mode | cursor + focus | wheel commit | presented p10 / p50 / p90 | peak RSS |
|---|---|---|---|---|
| box | n/a | 0.58 ms | 10.2 / 10.3 / 18.6 ms | 3.5 MB |
| full | 10.8 ms | 11.8 ms | 18.7 / 26.9 / **47.7** ms | 6.4 MB |
| **vp** | **1.3 ms** | **1.6 ms** | 10.2 / 18.6 / 18.8 ms | **3.4 MB** |
| daemon | 0.1-1 ms after trigger | n/a | 11.6 / ~20 ms (bimodal) | 3.5 MB resident |

Compositor CPU (niri, 10 s, 10 ms resolution): 64 ms baseline vs 66 ms
with a static transparent surface mapped. That is within noise.

### Findings

- **A full-size buffer delays everything:** niri sends the enter events
  only after importing the 29.5 MB buffer.
- **vp costs the same memory as box and puts the wheel at the cursor.** The
  two commits (map, then wheel) usually cost one extra compositor cycle
  (~8.4 ms); in the best case (p10) they land in the same frame.
- **The daemon is not faster.** It needs the same two commits. The hotkey
  still has to spawn a process, because Wayland deliberately has no
  global-hotkey protocol. The daemon would save ~1 ms of setup in exchange
  for 3.5 MB resident forever.
- When the input region is restored, niri sends `wl_pointer.enter` without
  any pointer motion.
- Presentation times come in ~8.4 ms steps. This is niri scheduling
  and still needs to be investigated. (VRR is unlikely to be the cause:
  on this machine it is on-demand, active only for fullscreen games.)

**Estimated keypress to photons on niri:** ~4 ms spawn + 0.5 ms exec +
1.6 ms ours + compositor (p10 ~8.6 / p50 ~17 ms) = **~15-23 ms, of which
~2 ms is ours.**

## Reproduce

```sh
cd spikes/latency && cargo build --release
./target/release/surf box|full|vp
./target/release/surf daemon
./target/release/surf vp --hold 12   # keep it mapped (compositor cost)
```
