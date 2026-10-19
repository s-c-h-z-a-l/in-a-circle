# Drag into the circle

**Interaction:** grab a file in the file manager (left button held),
press the hotkey, the wheel appears at the cursor, drag onto a slice,
release.

**Verdict:** **it works, over the standard protocol.** The compositor runs
the hotkey during the drag. The wheel, acting as a drop target, receives
the drag and, on drop, exact full URIs (`text/uri-list`), from Nautilus
and Thunar alike. No accessibility scraping, no clipboard tricks, no
file-manager plugins.

## Method

Probe: [`spikes/latency/src/bin/fm.rs`](../../spikes/latency/src/bin/fm.rs),
`fm dnd`: a full-screen layer surface acting as a `wl_data_device` drop
target. It accepts **copy only** and logs the offered MIME types, the
negotiated action, the drop, and the payload. It was triggered by a
temporary niri keybind (`Super+F1 repeat=false`). The tests were done by
hand on throwaway files.

**What the probe is not:** the drop target was the whole tinted
full-screen surface, with **no keyboard focus** (`KeyboardInteractivity::None`)
and **no wheel subsurface**. "Wheel mapped" below means that surface.
Slice targeting, Esc to cancel, and the real surface layout are untested
(see Open).

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

## Open

- Other compositors: does the hotkey fire during a drag (sway, Hyprland,
  KDE)?
- Slice selection from `wl_data_device.motion` coordinates (hover, drop
  position); not logged yet.
- **Keyboard focus during a drag.** Esc needs `Exclusive` keyboard
  interactivity. Does taking focus from the source app cancel or alter
  its drag?
- **Cursor position.** During a drag there is no `wl_pointer.enter` (the
  drag owns the pointer), so the wheel's position must come from
  `wl_data_device.enter`/`motion`, which arrives with the first movement.
- **Wheel subsurface needs an empty input region.** Data device enter and
  leave are per surface, with a new offer on every enter. With an empty
  input region on the wheel, all pointer and drag events go to the
  full-screen parent, in one coordinate space, with one offer.

## Reproduce

```sh
cd spikes/latency && cargo build --release
# bind a key in your compositor, non-repeating, to:
#   <repo>/spikes/latency/target/release/fm dnd
# grab a file, press the key, move, drop. Log: $XDG_RUNTIME_DIR/inacircle-dnd.log
```
