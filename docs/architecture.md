# Architecture

How In.A.Circle is put together, and why. Measurements behind these
decisions are in [`benchmarks/`](benchmarks); the [spikes](../spikes)
proved the parts. `inacircle` without arguments is the wheel (see
[The wheel](#the-wheel)); the engine also runs headless (see
[Headless CLI](#headless-cli)), with desktop notifications on request.

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
- `--manifest` runs in the sandbox of `run` (see Sandbox), narrower: no
  input, no writable directory (Landlock grants only reading and executing
  the system directories and the binary), and `RLIMIT_FSIZE` 0 (a manifest
  writes only to its stdout pipe, which `FSIZE` does not cover, and may
  write no file). Its deadline, 64 KiB output limit and group kill are
  unchanged. It gets the same clean environment as `run`, with
  `TMPDIR=/nonexistent`. Without the whole sandbox no manifest runs,
  built-in or not ("sandbox unavailable"; not cached, so a later start
  retries).
- A whole catalog refresh shares one 10 s budget: each manifest gets
  min(2 s, the rest of the budget), so a run of hanging binaries cannot
  hold a start for N x 2 s. A binary cut by it keeps its old cache entry
  (its family stays on the wheel) and is retried at the next start; one
  with no old entry stays out until then. The same budget applies when a
  refresh runs before an action starts on the choice path.
- The cache, `$XDG_CACHE_HOME/inacircle/catalog` (with an
  `INACIRCLE_ACTIONS_DIR` override: `catalog-<FNV-1a of the list>`, one file
  per set of directories), is a manifest-style file
  (header `inacircle-catalog 0`): per binary a `bin` line (path, mtime,
  size, built in or not) followed by its `action` lines; it is valid only
  while the set of binaries and their mtime and size match it exactly.
  The wheel never runs a manifest while its overlay is up: it reads the
  cache as it is and refreshes it after it closes (see the B path).
- Per action: id and family; input and output as **MIME types** plus the
  output extension (`ext=`); one or many inputs (`batch=no` for one at a
  time); label and group; cost and whether it loses information
  (`lossy=` describes the text content, text and line breaks, not how it
  renders); required external tool (`requires=`, checked on `PATH` at
  start; missing means a greyed-out slice that says what to install).
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
  process fails ("action left processes behind"), nothing placed; an input
  the process already settled with its own `skip` or `fail` keeps that
  outcome (each input has one), as it placed nothing. After a contract
  violation the reason stays the violation. The engine is the child
  subreaper of what it starts, so the kernel's child list, not `/proc`,
  tells whether a member still runs; if it cannot be read, nothing is
  accepted.

### Sandbox

Every action runs sandboxed, set up in the child between `fork` and
`execve`, after the child's own `setsid`:

- **Landlock** at the best ABI the kernel offers, at least 6, handling
  every right that ABI knows. Read and execute: `/usr`, `/lib`, `/lib64`,
  `/etc/ld.so.cache` (those that exist) and the action binary. Read: the
  inputs and the input list. Everything inside the step's work dir.
  Nothing else: no `/tmp`, `/proc`, `/dev` or `/etc`. Also denied: TCP
  (ABI 4) and UDP (ABI 10) on existing sockets, pathname unix sockets
  outside the temp dir (ABI 9), abstract unix sockets and signals outside
  the sandbox (ABI 6). File names outside can still be probed (`stat`).
- **seccomp**, one policy for x86_64 and aarch64 (each with its own
  syscall numbers), always installed; another architecture runs no
  action. Denied: `socket` and `socketpair` for every
  family, `AF_UNIX` included (`EACCES`); metadata changes (chmod, chown,
  utime, xattr, `file_setattr`); every ioctl but a few queries
  (`FIONREAD`, `TCGETS`, `TIOCGWINSZ`) and the ioctl forms of allowed
  fcntls; async fcntls (owner, signal, lease, notify, seals, `O_ASYNC`);
  other processes (ptrace, `process_vm_*`, the pidfd calls, `kcmp`);
  keyrings; namespaces; clone flags beyond fork, vfork and threads
  (`clone3` gets `ENOSYS`, so libc falls back to `clone`); scheduler and
  priority setters; `prlimit64` on another pid; `migrate_pages` and
  `move_pages`; the parent-death, subreaper and ptracer prctls; mounts
  (`open_tree_attr` too), the mount queries `statmount` and `listmount`
  (Landlock hides `/proc`'s mount table), and file handles; inotify and
  fanotify groups (Landlock does not mediate their marks); io_uring, bpf, perf, userfaultfd; System V IPC and
  POSIX queues; `setsid` and `setpgid`, so the action and every
  descendant stay in one killable group. A syscall through another ABI
  (i386, AArch32) kills the process; x32 gets `EPERM`.
- `no_new_privs`; every inherited fd ≥ 3 is closed on `execve`.
- **Memory net**: `RLIMIT_DATA` (soft = hard) of RAM/8 clamped to 1-4 GiB.
  It is the last net against a faulty or missing
  per-family limit, not the protection itself; over it an allocation fails
  and the input fails: a Rust action (allocation-failure line, then
  `SIGABRT`) with `out of memory (limit N MiB)`, any other action with its
  own error or the signal. The numbers and why `DATA` and not `AS` are in [resource-limits.md](resource-limits.md).
- Not covered: no `RLIMIT_NPROC` (an action can fork-bomb until cancelled).
  `RLIMIT_DATA` is per process, so each child of a forking action gets the
  cap again.
- **Clean environment**: the parent's is cleared; the child gets only
  `PATH=/usr/bin:/bin`, `LC_ALL` and `LANG` `C.UTF-8`, and `TMPDIR` (an
  action's private `<work>/tmp`, mode 0700). No session socket, bus,
  display, token or loader variable is passed through.
- **Mandatory**: nothing runs without the whole sandbox, built-in actions
  and manifests included, and an action the catalog knows from its cache
  meets the same gate when it runs. The gate needs Landlock ABI >= 6 (the
  signal and abstract unix socket scopes) and a parent that is not root,
  has one real/effective/saved/filesystem uid and one gid, and holds no
  permitted, effective or ambient capability (an inheritable one alone is
  harmless under `no_new_privs` and is what systemd gives every session;
  the bounding set does not count). Nothing is dropped instead. Otherwise
  every input fails with "sandbox unavailable: <why>", and the history
  records `sandbox=no`.

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
  the body with `Formatting was lost.`; a run the sandbox gate refused ends
  it with `Sandbox unavailable.`. Text is cut to 200 characters on a UTF-8
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
- After the Undo window the result is closed (`CloseNotification`), so no
  dead button stays in the notification centre. Sending it again without
  the button would pop it up a second time on common servers (noctalia). A
  closed notification ends the window at once, and so does SIGTERM or
  SIGINT received during the window (a cancel during the run is part of the
  run; the signals stay blocked and are read through the signalfd): the
  result is closed and `run` exits with its own code. An Undo click
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

## The wheel

`inacircle` without arguments. One process per hotkey press, gone when the
choice has run.

**Life of a run**

1. SIGALRM is reset to its default action and unblocked (an ignored
   disposition survives `execve`), then `alarm(30)` is armed, before the
   lock: a slow runtime dir is covered too. The default action kills the
   process, so this is the last guard if everything else hangs.
2. One instance: `flock` on `$XDG_RUNTIME_DIR/inacircle/wheel.lock`, opened
   `O_NONBLOCK | O_NOFOLLOW` (a symlink or FIFO there is an error, exit 1).
   A held lock makes a second instance exit 0 at once, before touching
   Wayland.
3. The surface is shown (below). The wheel has two levels: categories,
   then the chosen category's targets. Each wait (the drop, the category
   choice after a fallback drop, the target choice) has 30 s. The drop and
   the category choice re-arm `alarm` (and the
   surface's own deadline, 500 ms shorter so a timeout normally ends in an
   orderly close). The deadline will become configurable.
4. Closed without a choice (Esc, a click off the wheel, the drag leaving
   without a drop, the deadline, a Wayland error, the compositor closing
   the connection): surface torn down, lock released, exit 0 (1 on an
   error). Nothing runs and nothing is notified.
5. A choice: the Wayland connection is closed (the surface disappears), the
   lock released, `alarm(0)`; then the same run path as
   `inacircle run --notify <preset> <files…>` (executor, journal,
   notifications), with its exit code. The hotkey is usable again at once.

The logic is a pure state machine (inputs in, effects out, no syscalls)
driven by the Wayland layer, so every exit path is tested without a
compositor.

**Surface**

- A full-screen `zwlr_layer_shell` surface on the `overlay` layer,
  anchored to all four edges, with exclusive keyboard focus, so Esc always
  reaches us. Its buffer is 1x1 and transparent, stretched over the output
  with `wp_viewporter`.
- The wheel lives in a subsurface with an **empty input region**: every
  pointer and drag event lands on the parent. The wheel is painted while
  the cursor position is still on its way; only the subsurface position
  depends on it. It is placed once, clamped to stay on the output; hit
  testing uses the shifted center.
- Two `wl_shm` buffers; damage is computed against what is on screen, not
  against the buffer being drawn.
- Required globals (minimum version): `wl_compositor` 4, `wl_shm` 1,
  `wl_seat` 1, `wl_subcompositor` 1, `wp_viewporter` 1,
  `zwlr_layer_shell_v1` 1; missing or older is an error, exit 1.
  `wl_data_device_manager` 3 is optional (`set_actions` and `finish` need
  it): without it there is no drop target and the hub says `Nothing to do`.
- Caps against a hostile or broken server, each an error past the limit:
  256 globals, 16 live data offers, 64 MIME types per offer.

**The B path (drag, then drop)**

- Two levels. Level 1 shows **categories** (a preset's `group`, mapped by the
  `GROUPS` table in `engine::menu`: today `document` = north, labelled
  `Document`; an unknown `group` is not shown, one stderr line per run). A category is **lit** when any dragged file has a
  target in it, dim when it exists but nothing fits, and a direction
  without a category is empty. The hub shows the source (name or count).
  Level 2 shows the category's presets in their own directions (`markdown`
  north, `plain-text` south): lit if applicable, dim otherwise; the hub is
  the category's label. `inacircle offer --groups <files…>` prints level 1.
- The **early read**: at `wl_data_device.enter`, if the offer's MIME list
  has `text/uri-list` (offers arrive before `enter`), the wheel `receive`s
  it into a pipe at once, read in the `poll` loop (500 ms, 64 KiB, 1024
  URIs). The result carries the tag (seq) of the `enter` that started it; a
  new `enter` (another offer, or out and back in) closes the pipe and the
  machine ignores an older tag, so an old list never shows. When the list
  is in, level 1 replaces the stage and the drag is over live categories.
  The early read neither `finish`es nor `destroy`s the offer: it lives on
  until the drop. At the drop, with the list in, there is no second read:
  the machine emits `EndDrop` (`finish` with the negotiated copy, then
  `destroy`) and level 2 is shown for the category under the pointer. If
  the list is not in yet, or the early read failed or timed out, the
  existing read after the drop runs.
- **Fallback (`Drop here`)**: with no `text/uri-list` on offer, or when the
  early read fails or times out, the hub says `Drop here`, every slice is
  empty and the whole wheel is a drop target (as before). After the drop
  the list is read, level 1 appears, and a **click** on a lit category
  shows its targets (that click is not a drop).
- A swap guard of 200 ms: a click right after the wheel was replaced is
  ignored (it was aimed at the old wheel).
- With no drag (a plain hotkey press) the hub says `Nothing to do` and any
  click or Esc closes.
- Drag rules: copy only, never a `DELETE` target. Over the wheel with
  `text/uri-list` on offer, `accept` and `set_actions(copy)` where a drop
  would do something; elsewhere, and for any other drag, `accept(null)` and
  `set_actions(none)`. With level 1 showing during the drag that means
  only a lit category (not a dim one, an empty direction, or the hub); in
  the fallback every slice and the hub accept. `finish` is
  sent only if copy was negotiated at the drop. During a drag the position
  comes from `wl_data_device.enter`/`motion`, not `wl_pointer.enter`. A
  `leave` without a drop closes the wheel (and closes the early pipe). Events that carry an fd take it
  even when ignored.
- **Hit** semantics: a point on a slice is `Slice(k)`; inside the wheel's
  outer radius but off every slice (the hub and the gaps) is `Hub`;
  beyond it, `Outside`. `Hub` is a drop target and ignores clicks;
  `Outside` closes on click.
- A `text/uri-list` read (early or after the drop) goes through a pipe with
  a 500 ms deadline, at most 64 KiB and 1024 URIs. A source that never
  writes or never closes times out. A failed early read falls back; a
  failed read after the drop closes the wheel.
- Only `file://` URIs with an empty or `localhost` host count. `%XX`
  decodes to bytes; a non-`file` URI, another host, a bad escape or a
  `%00` drops that entry alone. A raw `?query` or `#fragment` is cut off.
  Paths are kept as given (`..` and non-UTF-8 bytes stay, as on the
  command line). CRLF and LF both end a line; lines starting with `#` are
  comments. If nothing is left, the hub says `Nothing to do`.
- Choosing: only an applicable target is clickable (left button); that
  is the choice. The URI bytes are freed before classification, and the
  files are tallied per type (count and the first name), not kept per file.
  Level 2 uses the catalog snapshot level 1 was built with (loaded once),
  so the drop does no catalog I/O and both levels agree.
- No action binary runs while the overlay is up: level 1 takes the
  catalog from the cache alone. A cached binary still present is used as
  cached, even if it changed; one the cache does not know is not offered
  this time; with no cache at all the hub says `No actions yet`. Once the
  wheel is gone (surface torn down, lock released), the cache is
  refreshed in the same process within the refresh budget (10 s in all,
  min(2 s, the rest) per manifest, see Manifest); a choice's run does that
  refresh itself, before the action starts.
- A drag that arrives once the drop was taken is refused.

**The hub text**

The hub shows the source: the file name for one file; `<n> / <total>
files` for several (`n` is what the wheel offers on, `total` what was
dropped). Text wider than the hub is cut with `…`. If a character of the
name is missing from the atlas (or the name is empty), the name is
replaced by `1 file · <type label>` (e.g. `1 file · text`), until a larger
atlas exists.

**Text**

One font (Noto Sans Medium, OFL), one size (15 px), scale 1: ASCII plus
the Hungarian accented letters, 115 glyphs. `tools/bake-atlas` bakes it
offline (the only place `fontdue` is used); the alpha atlas
(`crates/ui/src/atlas.bin`, ~12 KB) is embedded with `include_bytes!`,
advances in 26.6 fixed point. No font parsing at run time; the TTF is not
in the repository. A test compares the committed atlas with the bake's
output when the font is available.

**Look and geometry**

Box 480, outer radius 220, inner 80, hub 64, gap 2 px, eight slices
centred on the compass directions (0 = north, clockwise; borders at
22.5° + k·45°), so a preset's direction is its slice; the label sits at
a slice's middle. States: normal,
hover, dim. Polish comes later.

**Measuring**

`INACIRCLE_MEASURE=1` prints one line on stderr when the wheel closes:
`measure wheel main_to_commit_cpu_us=… main_to_commit_wall_us=…
hover_events=… hover_us_p50=… hover_us_max=… idle_wakeups=… … list_to_level1_cpu_us=… …`
(`list_to_level1_cpu_us`: from the early list's arrival (counted only when
the machine accepts it) to the lit level-1 wheel's first commit; -1 when there was none).
Unset, it costs one `getenv`. Numbers:
[benchmarks/wheel.md](benchmarks/wheel.md).

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
  and its peak memory (PSS, and RSS) on stderr (a measurement switch), before the
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
          (wire: the Wayland client, libc only; dbus: the notifications
          client, libc only; ui: the wheel's geometry, rasterizer, damage
          and text; the shipped `inacircle` links them, never `doc`)
bins/     inacircle, inacircle-text, inacircle-testaction (test-only)
tools/    gen-entities, bake-atlas (dev tools; bake-atlas alone uses
          `fontdue`, kept out of the shipped binaries)
scripts/  checks and helpers
spikes/   measurements, not product code
```

Product code is written from these decisions, not copied from the spikes.

## File types before the drop

On drag enter, files offer only `text/uri-list`, which says nothing about
the type; the URI list itself is guaranteed only after the drop.

- **The base path (built, [B path](#the-wheel)):** drop on the wheel hands
  the files over, then the second stage offers the type-specific targets.
- **A shortcut on top of it, not a second path (not built):** if the URI
  list can be read during the drag (`wl_data_offer.receive` before the
  drop) within a deadline, the wheel shows the type-specific slices
  already, and the drop is the final choice. If it does not arrive, the
  same wheel shows a single "drop here" target and the second step
  follows. Other applications may never serve the list early, which is why
  the shortcut cannot be the only path.
