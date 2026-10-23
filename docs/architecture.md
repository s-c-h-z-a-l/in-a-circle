# Architecture

How In.A.Circle is put together, and why. Measurements behind these
decisions are in [`benchmarks/`](benchmarks); the [spikes](../spikes)
proved the parts. The engine runs headless today (see
[Headless CLI](#headless-cli)), with desktop notifications on request;
the wheel is not built yet.

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

- `inacircle` is the wheel and the engine (≤ 1.5 MB). Actions are separate
  executables, started after the drop.
- **One binary per family.** A family is a shared internal model: a
  document tree (text and office documents), a value tree (JSON, YAML,
  TOML, CSV), a file list (archives), a pixel grid (images), pages (PDF),
  samples (audio). About six or seven families are expected; the first
  version needs one or two.
- **Built-in actions follow the same contract as third-party ones.** Our
  own actions prove the contract works.
- Locations, in priority order: `/usr/lib/inacircle/actions/` and
  `/usr/local/lib/inacircle/actions/` (built in), then
  `$XDG_DATA_HOME/inacircle/actions/` (user, plugins). Every executable
  regular file there (no dot names) is an action binary; on a duplicate
  action id the first one found wins. `INACIRCLE_ACTIONS_DIR` (a
  colon-separated list) replaces all three; actions found through it
  count as built in.
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
  collects the answers into a cache and reruns a binary only when it
  changed (a few `stat` calls per start). The binary is the single source
  of truth; the same pattern as fontconfig's or gdk-pixbuf's caches.
- The cache, `$XDG_CACHE_HOME/inacircle/catalog`, is a manifest-style file
  (header `inacircle-catalog 0`): per binary a `bin` line (path, mtime,
  size, built in or not) followed by its `action` lines; it is valid only
  while the set of binaries and their mtime and size match it exactly.
- Per action: id and family; input and output as **MIME types** plus the
  output extension (`ext=`); one or many inputs (`batch=no` for one at a
  time); label and group; cost and whether it loses information (`lossy=` describes the text content, text
  and line breaks, not how it renders); required external tool (`requires=`,
  checked on `PATH` at start; missing means a greyed-out slice that says
  what to install).
- A plugin may add text types by extension (`type text/x-typst ext=typ`).
- Format: line-based, tab-separated, versioned header. A small parser of
  our own, no JSON dependency.

```
inacircle-manifest 0
action	text.txt-to-md	family=text	in=text/plain	out=text/markdown	ext=md	label=Markdown	group=document	cost=10	lossy=no	batch=yes
action	text.md-to-txt	family=text	in=text/markdown	out=text/plain	ext=txt	label=Plain text	group=document	cost=10	lossy=yes	batch=yes
```

## Invocation

```
<family> run <action-id> --work <temp dir> [--opt key=value] [--inputs-from <file>] -- <absolute paths>
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
  progress<TAB>0.42
  done<TAB>/abs/notes.md<TAB>0/notes.html
  skip<TAB>/abs/blob.bin<TAB>unsupported
  fail<TAB>/abs/broken.md<TAB>read error at line 12
  warn<TAB>/abs/tables.md<TAB>tables dropped (Markdown has no merged cells)
  ```

  `<TAB>` is a real tab. `done` carries the input and the output path
  relative to `--work`; every field uses the manifest escapes
  (`\\ \t \n \r \=`, plus `\#` for a leading `#`; `=` is always escaped
  in event fields). An action writes its output to
  `<work>/<index>/<stem>.<ext>`, where `<index>` is the input's
  position in the batch. The contract is version 0 and unstable until a complex action has run on it. `--inputs-from` is one
  escaped path per line; inputs and `--work` must be absolute, and non-UTF-8
  paths are out of scope for v0.

  stderr is free-form; on failure its last lines go into the message.
- Exit codes: 0 done, 1 failed, 2 contract or invocation error (ours, not
  the file's), 3 unsupported input. For a batch: 0 if at least one input
  produced `done`; otherwise 1 if any input produced `fail`; 3 only when
  every input was unsupported. Exit 1 also covers a batch that cannot run
  at all (`--work` missing or not writable).
- Cancel with `SIGTERM` to the action's process group, `SIGKILL` 5 s
  later; the engine removes the temp dir either way.
- Limits, set in the child: `RLIMIT_FSIZE` caps **each file** an action
  writes at 16 × the total input size, clamped to 256 MiB – 16 GiB
  (archive bombs, with no plugin code; many files can still fill a disk);
  `RLIMIT_CORE` is 0.
- The supervisor caps what it accepts: at most 16 `warn` per input (then
  one "n more warnings"), and per action process at most 1024 `done`
  outputs and 1 MiB of `done` paths. An input that crosses a cap or causes
  a contract violation fails with nothing of it placed; inputs finished
  before keep their results. If any process of the action's group is
  still alive after it exits, the group is killed and every input of that
  process fails ("action left processes behind"), nothing placed.

### Sandbox

Every action runs sandboxed, set up in the child between `fork` and
`execve`, after the child's own `setsid`:

- **Landlock** at the best ABI the kernel offers, handling every right
  that ABI knows. Read and execute: `/usr`, `/lib`, `/lib64`,
  `/etc/ld.so.cache` (those that exist) and the action binary. Read: the
  inputs and the input list. Everything inside the step's temp dir.
  Nothing else: no `/tmp`, `/proc`, `/dev` or `/etc`. Also denied: TCP
  (ABI 4) and UDP (ABI 10) on existing sockets, pathname unix sockets
  outside the temp dir (ABI 9), abstract unix sockets and signals outside
  the sandbox (ABI 6). File names outside can still be probed (`stat`).
- **seccomp** (x86_64 only; other architectures get Landlock only), also
  applied without Landlock. Denied: sockets other than `AF_UNIX`;
  metadata changes (chmod, chown, utime, xattr, `file_setattr`); the
  ioctls `FS_IOC_SETFLAGS`, `FS_IOC32_SETFLAGS`, `FS_IOC_FSSETXATTR` and
  `FS_IOC_ENABLE_VERITY`; `io_uring`; `setsid` and `setpgid`, so the
  action and every descendant stay in one killable group. A syscall
  through another ABI (i386) kills the process; x32 gets `EPERM`.
- `no_new_privs`; every inherited fd ≥ 3 is closed on `execve`.
- Not covered: no `RLIMIT_NPROC` or memory limit (an action can fork-bomb
  or exhaust RAM until cancelled).
- Not covered: keyring syscalls (`keyctl`, `add_key`) are allowed.
- Without Landlock, actions from the user or plugin directory do not run
  ("sandbox unavailable"); built-in actions run with a "ran without
  sandbox" warning, and the history records `sandbox=no`.

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
  `batch=no` run per file, one after the other (parallel runs may come
  later). A two-step route runs both steps in the same temp dir; only the
  last step's outputs are placed. Step 1's outputs are checked again once
  its whole group is gone (a regular file, no symlink on the way), so an
  action cannot swap one for a link before step 2 reads it.
- The supervisor is a single-threaded `poll` loop over the action's
  pipes, a cancel `signalfd` and the child's pidfd; its only timers are an
  hourly refresh of the temp dir's mtime and the grace after `SIGTERM`.
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
- Every action ends in a notification, with **Undo** where the server
  lists the `actions` capability. Actions running longer than a second
  (`RUNNING_AFTER`) first get a "running" notification (label, `n / m
  files`, a progress hint from the engine's fraction, at most four updates
  a second, same id) with **Cancel**. The supervisor then waits for Undo
  for ten seconds after completion.
- The final text is fixed English. One input, one output: the output's file
  name (plus `Saved to Downloads (source folder is read-only)` when it went
  there). Several: `<n> done[, <m> skipped][, <k> failed]` and up to three
  problem lines `<input>: <reason>` (failures first), then `… and <r>
  more`. Nothing finished: `Nothing converted`. Cancelled: `Cancelled`,
  `<n> done, <r> not converted`. A lossy action that produced output starts
  the body with `Formatting was lost.`; a run without the sandbox ends it
  with `Ran without sandbox.`. Text is cut to 200 characters on a UTF-8
  boundary, and `&`, `<`, `>` are escaped when the server has `body-markup`.
- The bus connection is lazy: made at the one-second mark or at the end,
  so a quick run connects once. The address is `DBUS_SESSION_BUS_ADDRESS`
  (the first well-formed `unix:` path or abstract entry) or
  `$XDG_RUNTIME_DIR/bus`; the server is D-Bus activated if it is not
  running. At the end, connect, authenticate and `Hello` have 300 ms
  together and every call 500 ms (activation may start the server). While
  the run goes on the supervisor waits for each call, so the whole open has
  500 ms and a progress update 100 ms. One wake reads at most 256 frames.
  More than 64 events about our notification in one wake, or four wakes in
  a row that spend all 256 frames, mean a broken server, as does a
  malformed reply (a `Notify` id of 0 included) or an error reply. A
  timeout, I/O error or hangup during the run only drops the connection:
  no more updates, and the end opens once more (with the end's deadlines)
  so the result still replaces the "running" notification and its Cancel.
  A broken server, or any failure at the end, turns notifications off for
  the rest of the run with one line on stderr
  (`inacircle: notifications unavailable: …`); the result, the history and
  the exit code never depend on them. No update is sent once every input
  is done: the result follows.
- Cancel arrives on the supervisor's wake point, in the same `poll` as the
  signalfd and the pidfd: no thread. A click after the last action exited
  is ignored. Signals count only from the server's unique name, on its
  object path, and about our own notification id; everything else on the
  bus is ignored.
- After the Undo window, the result is sent again (same id) without the
  button, so no dead button stays in the notification centre. A closed
  notification ends the window at once, and so does SIGTERM or SIGINT
  received during the window (a cancel during the run is part of the run)
  (still blocked and read through the signalfd): the result is sent again
  without the button and `run` exits with its own code. An Undo click
  undoes the job and replaces the result with `Undone` and
  `<n> removed[, <k> kept (<reason>)]`, or `Undo failed` with the cause.
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
7. Undo deletes only files we created, and only if unchanged since
   placement (device, inode, size, mtime, ctime); otherwise it keeps them
   and says why. A folder made for several outputs is removed only if
   empty afterwards.

## Context sources

A source (drag now; selection, clipboard and the active window later)
yields one or more MIME types plus lazy access to the data, read only when
an action runs. Past that point nothing depends on where the input came
from; the hub shows it. With several sources, a fixed priority decides:
drag, selection, clipboard, active window, nothing. "Nothing" shows
running jobs and history.

## Jobs and history

No daemon:

- history: an append-only log in `$XDG_STATE_HOME/inacircle/history`
  (header `inacircle-history 0`), manifest-style lines, each one `write`
  on an `O_APPEND` descriptor. A job's `dir`, `out` (input, path and the
  identity taken at placement), `skip` and `fail` lines are appended as
  they happen, its `job` line (id, time, action, status, sandbox) when it
  ends, and `undo` once undone. A run that dies midway leaves a valid
  partial record of what it placed, which undo still finds. Nothing of a
  run is kept in memory per outcome;
- running jobs: each supervisor registers itself in
  `$XDG_RUNTIME_DIR/inacircle/jobs/<job-id>`, one `job` line (id, pid,
  action, inputs, done, progress, start time) rewritten by rename at most
  four times a second and removed at the end; a job id is
  `<unix-seconds>-<pid>-<8 random hex digits>`, and an entry whose
  process is gone is ignored;
- cancelling: `SIGTERM` to the supervisor.

A jobs view (name, paths, what into what, progress, estimated time,
cancel) is a reader of these files and is itself spawn-per-use: a CLI, a
window, or later a web page. A web UI must not be an always-on local HTTP
server: that is a resident process, and any web page can reach `localhost`.

## Headless CLI

The engine without the wheel, for scripts and tests:

```
inacircle run [--notify] <preset-id> <files…>          the menu path
inacircle run [--notify] --action <action-id> <files…> one action directly
inacircle offer <files…>                               what the wheel would offer
inacircle undo <job-id>
inacircle jobs
inacircle cancel <job-id>
```

- Output is manifest lines on stdout, every field with the manifest
  escapes (a non-UTF-8 path is printed lossily). `run` prints `job <id>`,
  then `done`/`skip`/`fail`/`warn` lines as they happen and `progress` of
  the whole run, and ends with `summary done=… skipped=… failed=…`,
  counted per input.
- `run` exits by the batch rule above. A cancel fails the inputs not yet
  finished (`cancelled`), then the same rule applies.
- The same file (by canonical path) given twice counts once. A non-UTF-8
  input path is skipped ("non-UTF-8 path").
- `undo` of a job that is still running is refused (exit 1): cancel it
  first.
- An invocation error (unknown command, preset or action, no files, an
  empty path) is a message on stderr and exit 2, with nothing on stdout.
- `--notify` adds the desktop notifications of [Feedback](#feedback). All
  stdout lines, `summary` included, are written before the Undo window
  starts; the exit code is the same as without it.
- `INACIRCLE_MEASURE=1` makes `run` print its CPU time, its children's,
  and its peak RSS on stderr (a measurement switch), before the
  notifications at the end; with `--notify` a further line
  `measure notify_cpu_us=<n>` gives our CPU spent on the final notice (open
  and `Notify`) before the Undo window.
- `INACIRCLE_UNDO_WAIT_MS=<n>` shortens the Undo window. Test-only.

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
crates/   wire, dbus, ui, engine, manifest, action, toml, doc
          (dbus: the notifications client, libc only; the shipped
          `inacircle` links it, never `doc`)
bins/     inacircle, inacircle-text, inacircle-testaction (test-only)
tools/    gen-entities (dev tool)
scripts/  checks and helpers
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
