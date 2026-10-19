# Drag into the circle

**Interaction:** grab a file in the file manager (left button held),
press the hotkey, the wheel appears at the cursor, drag onto a slice,
release.

**Verdict:** **it works, over the standard protocol.** The compositor runs
the hotkey during the drag. The wheel, acting as a drop target, receives
the drag and, on drop, exact full URIs (`text/uri-list`), from Nautilus
and Thunar alike. No accessibility scraping, no clipboard tricks, no
file-manager plugins.

The [integration probe](#integration-probe-the-real-surface-layout) then ran
it on the real stack: own wire protocol, wheel subsurface, our rasterizer,
slice targeting, Esc. Taking keyboard focus does not break the drag.


## Method

Probe: [`spikes/latency/src/bin/fm.rs`](../../spikes/latency/src/bin/fm.rs),
`fm dnd`: a full-screen layer surface acting as a `wl_data_device` drop
target. It accepts **copy only** and logs the offered MIME types, the
negotiated action, the drop, and the payload. It was triggered by a
temporary niri keybind (`Super+F1 repeat=false`). The tests were done by
hand on throwaway files.

**What this first probe is not:** the drop target was the whole tinted
full-screen surface, with **no keyboard focus** (`KeyboardInteractivity::None`)
and **no wheel subsurface**. "Wheel mapped" below means that surface. The
integration probe below covers the rest.

## Results (niri 26.04)

| file manager | files | wheel mapped | drag enters wheel | drop | payload | action |
|---|---|---|---|---|---|---|
| Nautilus 50 | 1 | 0.7 ms | 4.28 s | yes | `file:///…/clip.mp4` | copy |
| Nautilus 50 | 3 | 0.6 ms | 3.43 s | yes | 3 URIs, non-ASCII percent-encoded | copy |
| Thunar 4.20 | 1 | 0.6 ms | 2.51 s | yes | `file:///…/k%C3%A9p%201.png` | copy |
| Thunar 4.20 | 3 | 0.4 ms | 2.75 s | yes | 3 URIs | copy |
| (control) | no drag | ~1.6 ms | n/a | n/a | n/a | closes on click |

All source files stayed in place.

### Findings

- **niri runs the keybind while a drag is in progress.**
- **The MIME types arrive when the drag enters the wheel,** before the drop,
  so the wheel can show type-specific slices while you are still dragging.
- **The enter appears to come with the first pointer motion** after the
  wheel maps. The tester held still for 2 s; the enter arrived at
  2.5-4.3 s. The wheel can show a neutral state and switch on the first
  movement, which the user makes anyway.
- **Accept copy only.** Sources offer copy|move (Nautilus also ask). With a
  move, the source may delete the original after the drop. Thunar also
  offers a legacy `DELETE` target, which must never be requested.
- **A held hotkey repeats.** Without `repeat=false` niri spawned the probe
  433 times while the key was held. The wheel needs a single-instance lock
  regardless of compositor settings; the probe uses `flock`.

## Integration probe: the real surface layout

Probe: [`spikes/wire/src/bin/drag.rs`](../../spikes/wire/src/bin/drag.rs).
Own wire protocol (`libc` only), full-screen layer surface with a 1x1
buffer scaled by `wp_viewporter`, the wheel in a subsurface with an
**empty input region**, the [own rasterizer](render-backends.md#own-rasterizer-cpu-sdf)
painting while the compositor works out the cursor. Hover follows motion.
Over a slice the offer is accepted (copy only); over the hub or outside
the ring it is rejected, so a drop there cancels. Keyboard interactivity
is a parameter. 4 rounds by hand, 49 runs, Nautilus 50, niri 26.04.

| | result |
|---|---|
| keyboard `exclusive` / `on_demand` / `none` during a drag | **drag continues in all three**; enter, motion and drop all arrive |
| events land on | the parent surface, every time (the empty input region works) |
| drop on a slice | exact URIs: 1 file in 117-220 µs, 3 files with accents (`jegyzet%20%C3%A9kezet.txt`) in 122 µs |
| drop on the hub / outside the ring | we reject the MIME type and read nothing; files untouched. **niri still sends `drop`, not a cancel** (see Findings) |
| Esc during a drag | the key never reaches us: the compositor's drag grab takes it and cancels the drag; we get `leave` and exit |
| Esc / click without a drag | close the wheel |
| wheel appears | without a drag: 2.0-2.4 ms after `main`; during a drag: **on the first movement** (see below) |

### Findings

- **niri sends `leave` right after `drop`.** The offer stays valid until
  `finish`; a client that forgets the offer on `leave` never reads the
  payload. (The first run of this probe did exactly that.)
- **Damage must be against the screen, not the buffer.** With two buffers
  in turn, damaging only what changed in the buffer being drawn left stale
  highlights and partial triangles on screen. Damage the previously shown
  and the new segment. A unit test now composites through damage rects.
- **`accept(null)` alone does not cancel the drop on niri.** Off a slice
  the probe sends `accept(serial, null)` but keeps the copy action from
  `set_actions`; niri still delivers `drop` (also after an explicit
  `accept(null)` that followed an accept). The source is never told the
  drop failed. Next: also `set_actions(none)` off a slice, and send the
  initial accept state on enter (the probe skips it when the first
  position is on the hub). (Pointed out in an external review.)
- **Outside the ring nothing is selected,** and a drop there runs nothing.
  Classic pie menus select by direction even past the rim (faster, more
  forgiving); here the release runs an action on files, so a wrong action
  costs more than a missed one.

### Why the wheel waits for the first movement during a drag

Wayland has no "where is the cursor" request, by design. A client learns
the position only when the compositor sends an enter event to one of its
surfaces. Without a drag, niri sends `wl_pointer.enter` as soon as the
overlay maps, so the wheel appears at once. During a drag the drag owns the
pointer, so there is no `wl_pointer.enter`; niri sends
`wl_data_device.enter` only when the pointer moves. Until then we cannot
know where to put the wheel.

Showing it somewhere else (say, the screen center) and jumping on the
first movement would be worse than appearing a frame later, and the hand
is usually moving anyway. **Next:** ask niri upstream whether a newly
mapped surface under a drag could receive `wl_data_device.enter` at once,
as `wl_pointer.enter` already does; measure other compositors.

## Open

- Other compositors: does the hotkey fire during a drag, and when does
  `wl_data_device.enter` arrive (sway, Hyprland, KDE)?
- Thunar and other toolkits through the integration probe (the first probe
  covered Thunar).
- **Drag released before any movement.** We then never get a data-device
  enter; when the drag ends, niri sends `wl_pointer.enter`, and the probe
  would show the wheel as if no drag had happened. Not tested.
- **Several outputs:** the layer surface goes to the compositor's chosen
  output; a drag started on another output is untested.

## Reproduce

```sh
cd spikes/latency && cargo build --release
# bind a key in your compositor, non-repeating, to:
#   <repo>/spikes/latency/target/release/fm dnd
# grab a file, press the key, move, drop. Log: $XDG_RUNTIME_DIR/inacircle-dnd.log

# integration probe; bind a non-repeating key to:
#   <repo>/spikes/wire/target/release/drag [exclusive|ondemand|none]
cd spikes/wire && cargo build --release && cargo test --release --bin drag
# Log: $XDG_RUNTIME_DIR/inacircle-drag.log
```
