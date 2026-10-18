# File manager input: how do selected files reach the wheel?

**Interaction:** select a file in the file manager, press and hold the
hotkey, the wheel appears, move toward a segment, release to choose. No
dragging. (Tangerine and similar tools only accept files from the
desktop. Linux setups rarely have one, so input from file managers is the
point.)

**Question:** how do we learn which files are selected?

**Verdict:** a plain selection is **invisible** over Wayland protocols in
every file manager we tested; only Ctrl+C exposes it. Nautilus, however,
reports every selection change to extensions through its official API.
Plan: a **native Nautilus extension** (zero side effects), with a universal
fallback for other file managers (virtual-keyboard Ctrl+C plus clipboard
save/restore through data-control).

## 1. What file managers expose over protocols

Probe: [`spikes/latency/src/bin/fm.rs`](../../spikes/latency/src/bin/fm.rs)
(`fm clip`). The file manager was opened on a test folder (names with a
space and an accent), then Ctrl+A and Ctrl+C were sent with `wtype`. Focus
was checked before every key.

| file manager | selection only: primary selection | Ctrl+C: clipboard |
|---|---|---|
| Nautilus 50.3 | nothing | `text/uri-list`, `x-special/gnome-copied-files`, plain paths, portal types; 3/3 files, accents correct |
| Thunar 4.20 | nothing | same |
| Nemo 6.6 | nothing | same |

Dolphin and PCManFM-Qt are not yet tested. A clipboard manager running
in the session may re-own the clipboard and re-offer its types.

## 2. Nautilus extension API

Probe: [`spikes/nautilus/inacircle_probe.py`](../../spikes/nautilus/inacircle_probe.py)
(nautilus-python 4.2). Nautilus 50 calls `MenuProvider.get_file_items(files)`
on **every selection change** (about 100 ms after it), not only for the
context menu:

| action | `get_file_items` |
|---|---|
| Ctrl+A | `clip.mp4, doc.pdf, kép 1.png` |
| Home | `clip.mp4` |
| Right | `doc.pdf` |

**Cost of a Python extension, inside Nautilus, all day:**

| | RSS | PSS | anon |
|---|---|---|---|
| no extension | 149 MB | 80 MB | 36 MB |
| Python extension | 171 MB | 96 MB | 49 MB |
| **delta** | **+22 MB** | **+16 MB** | **+13 MB** |

A Python interpreter is too heavy. The same API from a native C (or Rust
cdylib) extension should cost kilobytes; this is still to be measured.

## Open

- Hold/release detection: is the hotkey's release delivered to our
  surface, or only the modifier change?
- Does a synthesized Ctrl+C reach the file manager unmodified while the
  user still holds the hotkey's modifier?
- Multiple Nautilus windows and stale selections (combine the activated
  toplevel's `app_id` with a timestamp).

## Reproduce

```sh
cd spikes/latency && cargo build --release
# select files in a file manager, press Ctrl+C, then:
./target/release/fm clip
# Nautilus API probe (needs nautilus-python):
cp spikes/nautilus/inacircle_probe.py ~/.local/share/nautilus-python/extensions/
nautilus -q; nautilus &   # log: $XDG_RUNTIME_DIR/inacircle-nautilus.log
```
