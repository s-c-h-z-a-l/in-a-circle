# Context detection over pure Wayland protocols

**Question:** how fast can we learn what the user is working on (the
selection, the clipboard, the active app) without any compositor IPC?

**Verdict:** ~1.2 ms end to end. MIME types alone classify the context;
data is read lazily, only when an action runs.

## Method

Spike: [`spikes/latency/src/bin/ctx.rs`](../../spikes/latency/src/bin/ctx.rs).
It maps a 1x1 layer surface with exclusive keyboard interactivity. When
the surface gets keyboard focus, the compositor sends the clipboard
(`wl_data_device`) and primary selection (`zwp_primary_selection_v1`)
offers. The active app comes from `wlr-foreign-toplevel-management`, read
before taking focus. Contents were set with `wl-copy`. 3 open windows, 10
runs, medians.

## Results

| | text | URL | 15 files (uri-list) | 1 MB text |
|---|---|---|---|---|
| registry | 154 µs | 158 µs | 148 µs | 158 µs |
| foreign-toplevel roundtrip | 104 µs | 92 µs | 96 µs | 97 µs |
| commit to keyboard enter | 776 µs | 454 µs | 100 µs | 800 µs |
| commit to offers available | 782 µs | 459 µs | 107 µs | 806 µs |
| clipboard read | 99 µs | 85 µs | 496 µs | 1414 µs |
| primary read | 56 µs | 52 µs | 64 µs | 837 µs |
| **main to done** | **1.43 ms** | **1.24 ms** | **1.04 ms** | 3.58 ms |

### Findings

- Selection offers arrive together with focus, as the protocol guarantees.
  This is compositor-independent.
- The active app (`kitty`) is read correctly from foreign-toplevel.
- **The offered MIME types are enough to classify** (`text/uri-list` means
  files, `image/png` means an image). Text needs a small peek (up to 4 KB,
  ~0.1 ms) to tell a URL from plain text.
- Read latency depends on the source application. A clipboard manager
  re-owning the selection was measurably slower (2.8 ms for 1.3 MB).
- Taking focus has a side effect: the original app loses it until the
  wheel closes. Actions that paste back into it must account for this.

## Reproduce

```sh
cd spikes/latency && cargo build --release
wl-copy "hello"; wl-copy --primary "selected"   # note: overwrites your clipboard
./target/release/ctx
```
