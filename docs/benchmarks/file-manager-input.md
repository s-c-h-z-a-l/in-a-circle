# File manager input: how do selected files reach the wheel?

**Interaction tested here:** select a file in the file manager, press and
hold the hotkey, the wheel appears, move toward a segment, release to
choose. No dragging. (Tangerine and similar tools only accept files from
the desktop. Linux setups rarely have one, so input from file managers is
the point.)

**Question:** how do we learn which files are selected?

**Verdict:** **rejected; files come in by dragging them into the wheel
instead** (see [drag-into-circle.md](drag-into-circle.md)). A plain
selection is **invisible** over Wayland protocols in every file manager we
tested. Every way to read it has a cost we don't accept:

| route | why not |
|---|---|
| synthesized Ctrl+C (virtual keyboard) + clipboard save/restore | side effects: a running clipboard manager records the copy in its history; while the user still holds the hotkey's modifier, the file manager may receive e.g. Super+Ctrl+C |
| Nautilus extension API | a plugin living inside another process all day (+22 MB as Python); Nautilus only; no plugins by design |
| AT-SPI (accessibility tree) | the selection is readable, but the full path is only exposed by Thunar; elsewhere it must be reverse-engineered from localized UI labels |

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

A Python interpreter is too heavy. A native extension would be smaller, but
it is still a plugin inside the file manager, and Nautilus-only.

## 3. AT-SPI (accessibility)

Probes: [`spikes/observe/`](../../spikes/observe) (`observe.py` passive
watch, `atspi_dump.py`, `atspi_query.py`). The query reads state, not
events, so it needs no background process. The user made the selection
by mouse; the probe only reads.

**Passive watch (Nautilus):** a click produces only AT-SPI events
(`focused`, then `selected` ~80-100 ms later, on button release). Clipboard,
primary selection, D-Bus and inotify show nothing. A drag produces no
event on any channel.

| file manager | view | selection | how | time (Python) | full path |
|---|---|---|---|---|---|
| Nautilus 50 (GTK4) | grid | yes, multiple | `SELECTED` state on table cells; tree walk to the table | ~5.7 ms (54 nodes) | no; only path-bar labels (localized) |
| Thunar 4.20 | icon | no (file names not in the tree) | | | |
| Thunar 4.20 | list | yes | `Collection.get_matches` | ~1 ms | yes, location text field |
| Nemo 6.6 | icon | yes | `Selection` on the layered pane | not measured | no; path-bar buttons |
| Nemo 6.6 | list | yes | `Collection.get_matches` | ~1.3 ms | no |

- GTK4 (Nautilus) does not implement `Collection`, so a walk is needed. A
  naive full walk costs 20-40 ms and 600-1800 D-Bus calls.
- Nautilus appends a localized type suffix to file names in the tree.
- The query must run before we take keyboard focus, filtered to the
  active window; otherwise a second window of the same app gives a stale
  selection.

## Open

Not pursued further after [drag-into-circle.md](drag-into-circle.md).
Dolphin and PCManFM-Qt were never tested.

## Reproduce

```sh
cd spikes/latency && cargo build --release
# select files in a file manager, press Ctrl+C, then:
./target/release/fm clip
# Nautilus API probe (needs nautilus-python):
cp spikes/nautilus/inacircle_probe.py ~/.local/share/nautilus-python/extensions/
nautilus -q; nautilus &   # log: $XDG_RUNTIME_DIR/inacircle-nautilus.log
# AT-SPI (needs python-gobject + at-spi2-core); select files by mouse first:
python3 spikes/observe/atspi_query.py nautilus 10
python3 spikes/observe/atspi_dump.py Thunar
python3 spikes/observe/observe.py 20 /tmp/test-folder /tmp/observe.log
```
