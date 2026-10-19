# Architecture

How In.A.Circle is put together, and why. Measurements behind these
decisions are in [`benchmarks/`](benchmarks). Nothing here is
implemented in product code yet; the [spikes](../spikes) proved the parts.

## Overview

```
 hotkey ─► sources ─► classifier ─► catalog + planner ─► wheel ─► executor ─► supervisor ─► notification
           drag       content →     what to offer,        draw,    wheel        waits on the
           (later:    MIME type     8 fixed directions    hover,   closes,      action, reports,
           selection,                                     choose   action       offers undo
           clipboard,                                              starts
           window)                                                    │
                                                ═══ CONTRACT ═══      ▼
                                       manifest + invocation    action binaries, one per family,
                                       (a CLI contract)         spawned on use: built in, or a
                                                                wrapper around an installed tool
```

Everything is spawn-per-use. The hotkey is the user's compositor keybind
running `inacircle`; the process shows the wheel, hands the choice to an
action, waits for it, reports, and exits. Nothing stays resident.

**The contract is the stable part.** Every action depends on how it
describes itself and how it is called, so that is fixed early. The engine
behind it (classifier, planner, wheel) is ours and can change freely.

## Settled foundations

From the measurements: spawn-per-use with zero idle cost; our own Wayland
wire protocol (`libc` only, opcodes checked against the protocol XMLs); a
full-screen layer surface with a 1x1 buffer scaled by `wp_viewporter`, the
wheel in a subsurface with an empty input region; our own CPU rasterizer
with damage against what is on screen; an offline-baked glyph atlas; file
input by [dragging into the wheel](benchmarks/drag-into-circle.md), copy
only; a single instance and hard deadlines while the overlay is up. Budgets
are in `CLAUDE.md`.

## Actions and families

- `inacircle` is the wheel and the engine (≤ 500 KB). Actions are separate
  executables, started after the drop.
- **One binary per family.** A family is a shared internal model: a
  document tree (text and office documents), a value tree (JSON, YAML,
  TOML, CSV), a file list (archives), a pixel grid (images), pages (PDF),
  samples (audio). About six or seven families are expected; the first
  version needs one or two.
- **Built-in actions follow the same contract as third-party ones.** Our
  own actions prove the contract works.
- Locations: `$prefix/lib/inacircle/actions/` (built in) and
  `$XDG_DATA_HOME/inacircle/actions/` (user, plugins).
- Three groupings, kept apart: the **type** of the input (internal), the
  **group** of a target as the wheel shows it ("Image", "Document"), and
  the **family** of the code that does the work.
- **Across families:** shared crates at build time (deflate, zip, png,
  xml, the document tree). An edge lives in the family that does the hard
  part: PDF → PNG belongs to the PDF family, which links the PNG encoder.
  As a fallback, the planner chains two edges through a lossless
  intermediate file, mostly for external tools.

## Manifest

- Each family binary describes itself: `<family> --manifest`. The engine
  collects the answers into a cache (`$XDG_CACHE_HOME/inacircle/`) and
  rebuilds it only when an action binary changed (a few `stat` calls per
  start). The binary is the single source of truth; the same pattern as
  fontconfig's or gdk-pixbuf's caches.
- Per action: id and family; input and output as **MIME types**; one or
  many inputs (`batch=no` for one at a time); label and group; cost and
  whether it loses information; required external tool (`requires=`,
  checked on `PATH` at start; missing means a greyed-out slice that says
  what to install).
- A plugin may add text types by extension (`type text/x-typst ext=typ`).
- Format: line-based, tab-separated, versioned header. A small parser of
  our own, no JSON dependency.

```
inacircle-manifest 1
action	text.md-to-html	in=text/markdown	out=text/html	label=HTML	group=document	cost=10
action	text.txt-to-md	in=text/plain	out=text/markdown	label=Markdown	group=document	cost=10
```

## Invocation

```
<family> run <action-id> --work <temp dir> [--opt key=value] -- <absolute paths>
```

- argv only, never a shell. The engine decodes `file://` URIs and passes
  local files only. stdin is `/dev/null`, the working directory is the
  temp dir. Very long input lists go through `--inputs-from <file>`.
- **An action writes only into the temp dir.** The engine moves results to
  their final names with an atomic `rename`. So the output policy lives in
  one place, undo knows exactly what was created, no half-written file
  ever appears in the user's folder, and a broken plugin cannot overwrite
  anything.
- stdout, one event per line:

  ```
  progress 0.42
  done notes.md -> notes.html
  skip blob.bin unsupported
  fail broken.md read error at line 12
  warn tables dropped (Markdown has no merged cells)
  ```

  stderr is free-form; on failure its last lines go into the message.
- Exit codes: 0 done, 1 failed, 2 contract or invocation error (ours, not
  the file's), 3 unsupported input.
- Cancel with `SIGTERM`; the engine removes the temp dir either way. The
  engine sets `RLIMIT_FSIZE` on the action, which caps output size for
  every plugin (archive bombs) without any plugin code.
- **Every action runs sandboxed** with Landlock, set up in the child
  between `fork` and `execve`: writes only into the temp dir; reads only
  the inputs, the action binary and the system paths needed to run it; no
  network where the kernel supports it. The best available Landlock ABI is
  used. Where Landlock is missing, actions from the user or plugin
  directory do not run; built-in actions run, and the history and the
  notification say they ran unsandboxed.

## Classifier

- **Our own, built in,** not the system's `mime.cache`: the same file must
  give the same wheel on every machine, and the system database differs
  between distributions and versions. Recognising a type we have no action
  for is useless anyway.
- Binary formats: magic bytes in the first few KB decide; content beats a
  wrong extension.
- Zip containers are looked into: ODF and EPUB declare their type in the
  `mimetype` entry; OOXML is told apart by the central directory at the
  end of the file (`word/document.xml`, `xl/workbook.xml`, …).
- Text: no magic and valid UTF-8 without NUL bytes means text; the exact
  kind (Markdown, reStructuredText, Org, …) comes from the extension, since
  content cannot tell them apart. HTML and JSON are also recognised by
  content.
- Unknown types get an explicit "no action for .bin" instead of an empty or
  meaningless wheel.
- With many files, the first few decide what is offered; the rest are
  checked after the drop.

## Catalog, planner and the wheel

- **The catalog is the set of edges** (what the system can do, from the
  manifests). **The menu is a set of presets** per input type (what the
  wheel offers): curated defaults, replaceable by the user.
- For each preset the planner finds a route: a direct edge, or **at most
  two steps** through a lossless intermediate. No route means a greyed-out
  slice with the reason.
- **Eight fixed directions.** A target always sits in the same direction,
  whatever the input; unused directions are dimmed. Muscle memory is the
  point. The actual layout is a design decision.
- At most eight slices per ring and at most one sub-level, used only when
  the presets do not fit; then the first ring holds the target groups.
- **Mixed selections** offer the presets of the most common type (ties
  broken by a fixed type order) plus actions that make sense for many files
  ("all into one archive"). The other files are skipped, and the hub says
  so ("15 / 20").

## Executor and supervisor

- After the choice the wheel closes and releases the lock; the same
  process stays as the **supervisor** while the action runs (a new session
  with `setsid`, started through `fork` and `execve` so the sandbox and
  limits can be applied in between), then exits. It frees the wheel's buffers
  before waiting.
- **One process per target folder** of a batch (each folder gets its own
  temp dir, so the final move stays an atomic `rename`); actions declaring
  `batch=no` run per file, in parallel up to the number of cores.
- **A batch never stops to ask.** Unsupported or broken files are skipped,
  one failure does not stop the rest, name conflicts get a new name.
  Skipping is always safe here: actions only copy and never overwrite.
- **No automatic kill.** The hard deadlines protect the user from a stuck
  overlay; once the wheel is closed, a stuck action blocks nobody, and a
  fixed limit would be too short for a video and pointless for a text file.
  The user cancels instead.

## Feedback

- The supervisor is the action's parent, so `waitpid` gives the exact
  result. No file watching, no daemon.
- Desktop notifications through **our own `libc`-only D-Bus client**
  (`org.freedesktop.Notifications`), so they appear in whatever
  notification centre the user runs.
- Every action ends in a notification ("18 done, 2 skipped: …"), with
  **Undo** where the server supports action buttons. Actions running
  longer than a second first get a "running" notification with progress
  and **Cancel**. The supervisor waits for Undo for a bounded time (about
  ten seconds after completion).
- Mistakes are handled by safety and undo, not by confirmation dialogs:
  people click through confirmations, and they tax every correct choice.

## Output policy

In the engine, in one place:

1. Next to the source. If that folder is not writable, the XDG download
   folder, and the notification says so.
2. Same base name, new extension. When the format stays the same, a fixed
   suffix, English and Latin-only by default (`-small`, `-compressed`),
   configurable.
3. Never overwrite: `name (1).ext`, `name (2).ext`, without asking.
4. Several outputs from one input go into a folder (`doc/doc-01.png`, …).
5. Extraction: a single top-level folder is extracted as is; loose files go
   into a folder named after the archive.
6. The temp dir is `.inacircle-<random>` inside the target folder (so the
   final move is an atomic `rename`), always removed at the end; stale ones
   from a crash are cleaned by the next run (our pattern only, older than a
   day).
7. Undo deletes only files we created, and only if unchanged since (size,
   mtime, inode); otherwise it keeps them and says why.

## Context sources

A source (drag now; selection, clipboard and the active window later)
yields one or more MIME types plus lazy access to the data, read only when
an action runs. Past that point nothing depends on where the input came
from; the hub shows it. With several sources, a fixed priority decides:
drag, selection, clipboard, active window, nothing. "Nothing" shows
running jobs and history.

## Jobs and history

No daemon:

- history: an append-only log in `$XDG_STATE_HOME/inacircle/history`;
- running jobs: each supervisor registers itself in
  `$XDG_RUNTIME_DIR/inacircle/jobs/<pid>` (state, progress, updated
  sparingly);
- cancelling: a signal to the supervisor.

A jobs view (name, paths, what into what, progress, estimated time,
cancel) is a reader of these files and is itself spawn-per-use: a CLI, a
window, or later a web page. A web UI must not be an always-on local HTTP
server: that is a resident process, and any web page can reach `localhost`.

## Configuration

- `$XDG_CONFIG_HOME/inacircle/config.toml`, full **TOML 1.0** with a parser
  of our own (shared with the data family, tested against `toml-test`).
  Without a file, built-in defaults apply.
- No reload watcher is needed: every hotkey press reads the file afresh.
- Configurable: presets (a type's list replaces the built-in one as a
  whole), suffixes, notifications, look (colours, opacity, sizes). Fonts
  and font sizes are limited to the variants baked into the atlas.

## Repository layout

```
crates/   wire, dbus, ui, engine, manifest, toml, doc
bins/     inacircle, inacircle-text
spikes/   measurements, not product code
```

Product code is written from these decisions, not copied from the spikes.

## File types before the drop

On drag enter, files offer only `text/uri-list`, which says nothing about
the type; the URI list itself is guaranteed only after the drop.

- **The base path works everywhere:** drop on the wheel hands the files
  over, then a second wheel (click mode, the drag is over) offers the
  type-specific targets.
- **A shortcut on top of it, not a second path:** if the URI list can be
  read during the drag (`wl_data_offer.receive` before the drop) within a
  deadline, the wheel shows the type-specific slices already, and the drop
  is the final choice. If it does not arrive, the same wheel shows a single
  "drop here" target and the second step follows.

Whether Nautilus and Thunar serve the list before the drop is not measured
yet; other applications may never do it, which is why the shortcut cannot
be the only path.
