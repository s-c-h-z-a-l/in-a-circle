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
- **`pages`** (`inacircle-pages`): page-image books to PDF and CBZ. Three
  actions. `pages.images-to-pdf` is a join action: any number of JPEG and
  PNG files (the same image rules as below) become one PDF, one page per
  file in the given order. An image that cannot be taken is left out and
  reported: unsupported ones (alpha, interlaced, 16-bit PNG, unusual JPEG)
  as `skip`, damaged or over-limit ones as `fail`; the rest is joined.
  When none can be taken nothing is written. If the PDF as a whole would
  pass the output size limit, nothing is written; the inputs already taken
  and those not yet examined fail with the size copy, while those already
  skipped or failed keep their own outcome. It is
  `lossy=yes` for the same reasons as the other action.
  `pages.images-to-cbz` is the other join action: the same JPEG and PNG
  files become one CBZ, each image stored byte for byte (ZIP method 0, a
  fixed header with no timestamp, extra field or ZIP64) under a numbered
  name (`001.jpg`, `002.png`, ... in the given order; at least three digits,
  more when there are more than 999 inputs). Only the header is checked, so
  every valid JPEG and PNG is taken (alpha, 16-bit, interlaced, CMYK,
  12-bit, arithmetic) and nothing is changed: it is `lossy=no`. An image
  that is not a JPEG or PNG, is damaged in its header or is over a limit
  (64 MiB, 180 MP) is left out and reported as `fail`; the rest is joined.
  If the archive as a whole would pass the output size limit or 4 GiB, or a
  write fails, nothing is written and every taken input fails.
  `pages.cbz-to-pdf` is a one-to-one action: a CBZ (a ZIP of PNG and JPEG pages,
  8-bit PNG without transparency, JPEG passed through untouched) becomes one PDF
  with one page per image, in natural order of the entry names; one CBZ, one
  PDF, all or nothing. It sits in the **Book** category. It is `lossy=yes`: it
  leaves out entries that are not images (such as `ComicInfo.xml`), ignores the
  Exif orientation and does not embed ICC profiles, and reports each as a
  warning, which the notification shows. A PNG is checked down to its scanlines;
  a JPEG is passed through without checking its scan data, so a JPEG truncated
  inside its entropy data is not detected and shows as a grey or partial page in
  the reader.

- **`subtitles`** (`inacircle-subtitles`): SRT, WebVTT, ASS/SSA and TTML to SRT
  or WebVTT. Six one-to-one actions, `batch=yes`, `lossy=yes`; the readers and
  writers are the `crates/subtitles` library (standard library, and the `xml`
  crate for TTML). The rules
  are under Manifest, after the text family's; the TTML element-by-element
  accounting is under [TTML accounting](#ttml-accounting). A TTML SMPTE time code
  (`ttp:timeBase="smpte"`) is read with the continuous-marker formula under
  either `ttp:markerMode`: an approximation for discontinuous markers, which
  carry no media time of their own.
  TTML styles and regions: a `style` attribute names styles that merge left to
  right (the later one wins), with the element's own inline values last; a
  style may reference styles, and a chain is read up to 32 entries (a loop or a
  longer reference is refused and counted as `Styling`). Italic, bold, underline,
  line-through and overline are inheritable and pass to the paragraph's text, and
  a region gives a paragraph only the values no ancestor gives. A paragraph is
  `top` only when its own region is placed in the upper third: its anchor (origin,
  plus the extent by `displayAlign`) at or within 33 % of the root container's
  height from the top, where a px placement needs the root's `tts:extent`. Every other declaration is
  `Styling` (colours, sizes, fonts) or `Position` (placement), counted once per
  cue. The style and region tables are charged to the item budget before they
  are stored.

### TTML accounting

What a TTML conversion keeps, counts, skips and refuses. The reader
(`crates/subtitles/src/ttml/mod.rs`) has this table in its doc comment, and the
test `every_ignored_construct_is_counted` checks one row per construct. Losses
are counted in cues, once per cue and kind, except `Elements`, which counts
items: each ignored element (with no visible text) and each image unit once,
however many cues it lies over, and also in a skipped paragraph (the skip counts
the paragraph, the item what it held). `skipped` counts items (elements,
paragraphs, text blocks) with the first one's line and reason. A count on every cue of the
document (`initial`, a `set` outside a paragraph) is the cue count, an upper
bound where the reader does not track which paragraphs the element reaches.
A paragraph with no visible text has nothing to lose, so it is not counted,
whatever else it holds (a hidden or animated one included).

| Construct | Goes to |
|---|---|
| XML that is not well-formed, or nested past `MAX_DEPTH` | `Malformed` (the whole file) |
| a DOCTYPE with entity declarations; a declared encoding other than UTF-8 or UTF-16 | `Unsupported` (the whole file) |
| a root that is not `tt` in the TTML or DFXP namespace | `NotTtml` (the whole file) |
| the DFXP 2006 namespaces of TTML 1 (`ttaf1`, and its `#styling`, `#parameter`, `#metadata`), on any element or attribute | read as their TTML counterparts: no loss of their own |
| `ttp:timeBase="clock"`, a `wallclock(...)` time, a drop-frame `ttp:dropMode` | `Unsupported` (the whole file) |
| `ttp:*` on the root (TTML or DFXP parameter namespace) | read as the time parameters, no loss; an unconverted time base or drop mode, or a bad value: `Unsupported` |
| other `ttp:*`, `ttm:*` attributes, `xml:lang`, `xml:id` on elements other than styles and regions | no content: nothing |
| `metadata` and the elements of the metadata namespaces, anywhere | silent: metadata is not content (14) |
| `head`: `styling` and `layout` are read (below); any other child (foreign, or a tt element not in the vocabulary) with visible text in it | `skipped`: `unsupported element`, with its line; with none, `Elements`, once (an item) |
| an attribute in another namespace (not TTML's, DFXP's, XML's, `xsi`, the `ttm` or `ttp` ones, or SMPTE's `backgroundImage`) on `tt`, `body`, `div`, `p`, `span`, `br`, `region`, `style` or `initial` | `Styling` on the element (every piece under it, once per cue), e.g. `xmlns:v="urn:v"` `v:x="1"`, `itts:forcedDisplay`, or a `tts:display` in a namespace that is not TTML's (not applied, so the text is kept). `ebutts:linePadding` and `ebutts:multiRowAlign` (namespace `urn:ebu:tt:style`, IMSC 1.2 9.5.14 and 9.5.15): `Position`, as `tts:padding` and `tts:textAlign` are |
| `styling`: each `style` | an entry of the style table. Its `xml:id` names it; without one it can never be referenced and has no effect |
| `styling`: `initial` (10.1.1) | the initial value of each property it names, for every paragraph: a paragraph takes it where no ancestor, region or style reference gives one (10.4.2), so it is the lowest layer. Carried: `tts:fontStyle`, `tts:fontWeight`, `tts:textDecoration`, and `tts:display`, `tts:visibility`, `tts:opacity` (the same rules as on the element). Each property takes the value of the last `initial` that names it. Any other property: `Styling` or `Position` on every cue. A conditional `initial` is not evaluated, so it is not applied: `Styling` on every cue | |
| `styling`: any other child | `Elements`, once (an item) |
| `layout`: each `region` | an entry of the region table, named by its `xml:id` |
| a duplicate `xml:id` among styles, or among regions | the first definition stands; the later one is dropped. Each reference to the name counts the dropped one's declarations (below). The `xml:id` error is not fatal (XML ID, section 6) |
| a dropped duplicate's declarations, referenced | `Styling` for each carried or unknown declaration, `Position` for a placement: on each cue of a paragraph or element that references the name |
| `style` inside `region` | a nested declaration of the region (10.4.1.4): an entry merged after the referenced styles, before the region's own attributes |
| a `style` attribute (`body`, `div`, `p`, `span`, `br`, `region`, `style`): each id | merged in order. An id not defined, a loop, or a chain past 32 entries sets `Styling` on the element |
| `tts:fontStyle`, `tts:fontWeight`, `tts:textDecoration` (underline, line-through, overline) on those elements | carried: italic (`oblique` too), bold (`normal` or `bold` only, 10.2.24) and the decorations of the text, inherited as 10.2 says |
| `tts:fontStyle`, `tts:fontWeight`, `tts:textDecoration` with a value outside the grammar | `Styling` |
| other `tts:*` declarations, and `tts:*` names the reader does not know | `Styling` on the element (every piece under it, once per cue) |
| `tts:display` `none`, `tts:visibility` `hidden`, `tts:opacity` `0`, on `body`, `div`, `p`, `region` or `span` (10.2.14, 10.2.49, 10.2.30) | the content is not shown: `none` and `0` hide the element and everything in it, `hidden` is inherited and a descendant with `visible` is shown. A hidden paragraph is `skipped`: `hidden by the document`; a paragraph with its own hidden text, or a hidden span in a shown paragraph, counts once (the outermost element holding the hidden text). Opacity counts the whole element; the other values are initial values and count nothing |
| `tts:opacity` with a value between 0 and 1, `tts:display="inlineBlock"`, a `tts:display`, `tts:visibility` or `tts:opacity` on `br` or `tt` (not applicable there) | `Styling`; the text is kept |
| `tts:origin`, `tts:extent`, `tts:displayAlign` on a region of a paragraph | the placement of `top`: a `top` paragraph keeps them; otherwise `Position` |
| `tts:origin`, `tts:extent`, `tts:displayAlign` on any other element | `Position` |
| `tts:padding`, `tts:position`, `tts:textAlign`, `tts:bpd`, `tts:ipd`, `tts:overflow`, `tts:zIndex`, `tts:disparity` | `Position` |
| `tts:extent` on `tt` (10.2.16) | the root container's height in px (`top` of a px placement); `auto`, `contain`, and two px lengths are read; anything else `Position` |
| a region's placement that cannot be computed (`em`, `c`, `rw`, `rh`, a sign, a `px` without a root height) | the paragraph is not `top`; its placement counts as `Position` |
| a paragraph with no region (no `layout`, or none named on it or its ancestors) | never `top`; no count (a region named and not defined counts `Position`, as below) |
| `region` on `body`, `div` or `p` naming no region | `Position` on the paragraphs it reaches |
| `region` on a `span` | `Position` on the span's text (the span is not placed in its region) |
| `body`, `div` | containers: timing. A bad time or `timeContainer` makes the paragraphs under it `skipped` |
| `p` | cues (`slice.rs`), or `skipped` with its line and reason |
| `span` | a container. A `span` with its own timing slices its paragraph |
| `br` | a line-break piece over the container's interval, open-ended when the container has no end (12.4): it does not decide the container's end. Metadata in it is ignored without a count; other content is counted as below |
| `ruby`, `rb`, `rbc`, `rt`, `rtc`, `rp` in a paragraph or span | a span whose text stays; `Styling` on each cue of it |
| text in `p` or `span`, visible | a piece. Visible text with no display time (zero long, or outside its container) makes its paragraph `skipped` once: `text with no display time` |
| text in `p` or `span`, white space only | spacing only: a piece over its container's interval; with no end of its own it lasts as long as the paragraph (8.2.10, 12.4), and the space is trimmed where a line starts or ends. Not counted |
| visible text directly in `tt`, `body`, `div`, `br`, `head`, `styling`, `layout`, `style` or `region` | `skipped`: `text outside any cue` |
| `begin`, `end`, `dur` with a time at or past 100 hours, in one expression or summed through the containers | a fault on the element: its paragraphs are `skipped`, `time is 100 hours or more` (the same reason wherever the time comes from)
| `begin`, `end`, `dur` with a field past its range (minutes or seconds of 60 or more, a frame past the frame rate) | a fault on the element: its paragraphs are `skipped`, `time out of range`
| `begin`, `end`, `dur` with a bad value, or `end` before `begin` | a fault on the element: its paragraphs are `skipped` with the reason |
| `xml:space` | white-space mode (8.2.10); another value is a fault |
| `condition` on `body`, `div`, `p`, `span`, `br` or `ruby` | the element and its content: `skipped`, `conditional element`. The condition is not evaluated, and the text may or may not be shown (8.2.1), so it is not an ignored element |
| `set` or `animate` on another target, in a paragraph or span | `Styling` on each cue of that paragraph |
| `set` or `animate` on another target, in `body`, `div` or the head | `Styling` on every cue of the document (an upper bound: the reader does not track which paragraphs they reach). One inside an `animation` is out-of-line: see the rows below |
| `set` or `animate` with a `tts:display`, `tts:visibility` or `tts:opacity` target (13.1.3, 13.1.1), in a paragraph or span, or in `body`, `div`, `region` (the paragraphs under it, or placed in it) | each paragraph it reaches is `skipped`: `visibility changes over time`. The timeline is not played, so the paragraph is neither shown nor hidden. In the head or on a `style`, the same element reaches no paragraph: `Styling` on every cue, as for another target |
| `animation` (13.1.2): a container of out-of-line `set` and `animate` | nothing counted itself. Each `set` or `animate` with an `xml:id` is counted where an element names it with an `animate` attribute (next row); one that nothing names has no effect, so nothing is counted |
| the `animate` attribute (13.2.1) on `body`, `div`, `p`, `span` or `region`, each IDREF it names | an IDREF costs one item. A visibility target refuses each paragraph under the element (or placed in the region): `skipped`, `visibility changes over time`. Another target, or an IDREF that names no such animation, is `Styling` on the element's cues. Each element named costs one item (its entry, charged before it is stored) |
| a foreign-namespace element (its whole subtree is pruned, 8.1) in a paragraph or span, with visible text in it | `skipped`: `unsupported element`, with its line; the paragraph's other text converts
| a foreign-namespace element in a paragraph or span with no visible text | `Elements`, once (an item), wherever it sits in the paragraph |
| a foreign or unknown tt element in `body` or `div` with visible text in it (its whole subtree is pruned) | `skipped`: `unsupported element`, with its line
| a foreign or unknown tt element in `body` or `div` with no visible text | `Elements`, once (an item) |
| an `image` element (tt namespace) in a paragraph or span | the paragraph is one image unit, counted once in `Elements`; its text is converted |
| an `image` element in `body` or `div` | an image unit: `Elements` once |
| `smpte:backgroundImage` on a `p` | the paragraph is one image unit (as above); its text is converted |
| `smpte:backgroundImage` on a `div` or `body` | the `div` or `body` is one image unit; the text under it is converted |
| `smpte:backgroundImage` on a `span` or `br` | its paragraph is one image unit (as above) |
| a `span`, `p`, `div` or ruby element in a place that holds no text (`body`, `div`, the head, or inside a `p` or `span` where a paragraph cannot nest) | `skipped`: `unsupported element`, the element and its content |
| the document has image units, no text cue and nothing skipped | `ReadError::ImageBased` (the whole file) |
| the document has no cue, no image unit and nothing skipped | `ReadError::NoCues` (the whole file) |
| the document has no cue, and skipped items (a stray image does not change this) | `ReadError::AllSkipped`: the count and the first skipped item (the whole file) |
| a `p` with no end time: its implicit duration is indefinite (12.4) and the document has no end | `skipped`: `no end time` |
| element frames, pieces, copies of pieces in slices, lines and cues, and the style and region entries, their references, and the duplicates | the item budget: past it `TooComplex` |
| each element outside a paragraph that holds a `set` or `animate` of `display`, `visibility` or `opacity` (the animated-visibility scope pass: a division, the body, a region, the root, or a head element, one item each); a `set` inside a paragraph or span is free | the item budget, charged in a pre-pass before the read (`animated_scopes`) |
| text copied into cues | `TextTooLarge` past `OUTPUT_BYTES` |

Past `ITEM_BUDGET` (2,000,000 items: every element, piece, copy, cue and table
entry) the whole file is refused as `TooComplex`; nothing is converted in part.

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
- A failed run is the binary's own fault, or not. Its own: it hung past
  its own 2 s deadline, crashed (died by a signal), exited non-zero, left
  processes behind (also ones still there when the cleanup gives up),
  kept its output open after its exit, or did not exit after SIGKILL. Then
  the binary backs off (the negative cache): its `bin` line keeps its
  current identity plus `fails=<n>` and `retry=<unix seconds>`, and its old
  actions, if any, stay on the wheel. Its manifest is not rerun before
  `retry`: 60 s after the first failure in a row, doubling (60 s x 2^(n-1)) up
  to 24 h, reached at the 12th; the count stops at 64. A success clears both
  fields; a changed binary (mtime or size) is tried at once and counts from
  zero. `retry` is wall-clock time (`CLOCK_REALTIME`, which survives a reboot);
  one more than 24 h ahead (the clock went back) is tried at once. Not its
  fault, so not penalized ("will retry" at the next start, as before): the
  refresh budget ran out, even when it cut a manifest short of its own 2 s; the
  sandbox or the supervision did not start; the spawn failed; no stdout; a
  `poll`, `wait`, `read` or child-list error. A broken manifest (not UTF-8, too
  large, malformed) stays cached without actions and without a backoff until the
  binary changes.
- A whole catalog refresh shares one 10 s budget: each manifest gets
  min(2 s, the rest of the budget), so a run of hanging binaries cannot
  hold a start for N x 2 s. A binary cut by it keeps its old cache entry
  (its family stays on the wheel) and is retried at the next start; one
  with no old entry stays out until then. The same budget applies when a
  refresh runs before an action starts on the choice path.
- The cache, `$XDG_CACHE_HOME/inacircle/catalog` (with an
  `INACIRCLE_ACTIONS_DIR` override: `catalog-<FNV-1a of the list>`, one file
  per set of directories), is a manifest-style file
  (header `inacircle-catalog 1`): per binary a `bin` line (path, mtime,
  size, built in or not, and `fails`/`retry` while it backs off) followed
  by its `action` lines; it is used as it is only while the set of
  binaries and their mtime and size match it exactly and no backoff is
  due; otherwise only the binaries that are new, changed or due rerun.
  `fails` (1 to 64) and `retry` (at most 20 digits, a `u64`) are plain
  decimals, both or neither; anything else, or another version, rejects
  the cache, which is rebuilt.
  The wheel never runs a manifest while its overlay is up: it reads the
  cache as it is and refreshes it after it closes (see the B path).
  The cache is read bounded (256 KiB, measured on the bytes read, not on
  the size `stat` shows): at most 128 binaries, 1024 actions, 1152
  records, 32 fields per record, 12288 fields and 32 KiB per line. A cache
  over a limit is rejected loudly and rebuilt. A rebuild obeys the same
  limits on what it has collected so far: a binary whose manifest would
  cross one (each manifest valid alone) is refused whole with a message,
  never trimmed silently. A manifest itself is limited to 256 records, 32
  fields per record, 4096 fields and 32 KiB per line, checked before it is
  parsed; past that its binary is refused. The scan of the action
  folders (on the wheel's path too) is bounded while it runs: at most 512
  entries per folder (more refuses that folder whole, loudly) and at most
  129 binaries held, the smallest paths in folder order.
- Per action: id and family; input and output as **MIME types** plus the
  output extension (`ext=`); one or many inputs (`batch=no` for one at a
  time); label and group; cost and whether it loses information
  (`lossy=` describes the text content, text and line breaks, not how it
  renders); required external tool (`requires=`, checked on `PATH` at
  start; missing means a greyed-out slice that says what to install).
- `ext=same` names no extension: the output keeps its input's
  as it is (the last one, its case kept: `Film.hu.srt` gives
  `Film.hu/Film.hu.srt`, `a.SRT` gives `a/a.SRT`, `a.tar.gz` gives
  `a.tar/a.tar.gz`; an input without one gives an output without one,
  and so does a trailing dot, an empty extension: `Film.` gives
  `Film/Film`). A name is written as UTF-8: bytes of the extension that
  are not UTF-8 become U+FFFD, as those of the stem already do in every
  output name (`a.b\xff` gives `a/a.b\u{FFFD}`). Only a plugin can meet
  this: the shipped `ext=same` actions take only listed ASCII extensions.
  It is valid on every one-to-one action, `batch=yes` and `batch=no`; a
  join's one output has no single input to take it from, so `join=yes`
  with `ext=same` rejects the line. Any other value is still the
  extension itself (`ext=SAME` is `.SAME`). A line without it means what
  it did before, and the catalog cache keeps its format and version. An
  engine older than this rule reads `same` as an extension and names the
  output `x.same`.
- A join action turns many inputs into one output (`join=yes`; absent
  means `no`). It may take a set of input types: `in=` is then a
  comma-separated MIME set of at most 16 members (the roadmap's image
  types and a document-with-assets role both stay far below; the menu
  matches a member at a time), none empty, none repeated. `min=<n>` is the
  fewest accepted inputs a run needs, 1 to 1024 (the invocation's input
  limit; absent means 1). `min` and a type set are valid only with
  `join=yes`; any other value or combination rejects the line
  ("invalid action line ignored"). For display the engine takes the set's
  first member as the action's input type. The catalog cache stores the
  manifest lines verbatim, so its format does not change.
- A join action may also take assets beside its documents
  (`assets=<MIME set>`, e.g. the images a set of Markdown files shows).
  `in=` is then the set of document types and `min=` counts documents
  only; assets alone never make a run. `assets=` has the shape of the
  `in=` set (1 to 16 members, none empty, none repeated) and shares no
  member with `in=`, so every input has one role. It is valid only with
  `join=yes`; absent means no assets. Any other value or combination
  rejects the line. A line without `assets=` means what it did before,
  and the catalog cache (stored lines, checked again on load) keeps its
  format and version.
- 8-bit text (`text/x-8bit`) is decoded one file to one copy, its output
  capped by that one file (see the Output policy), so like `ext=same` it
  is valid only on a one-to-one action: a join with it among its `in=`
  or `assets=` types rejects the line.
- `text.txt-to-md` is `lossy=yes`: runs of blank lines merge into one
  paragraph break (and runs of spaces may collapse when rendered), and
  the action reports it (blank lines at the start or end of the text and
  the spaces of whitespace-only lines count). The planner therefore never
  takes it as a lossless intermediate, and the notice says formatting was
  lost for a run whose output actually lost something.
- `text.txt-to-md` escapes every tilde of a run of two or more, so that
  plain text is never read back as strikethrough, and the `:` of a line
  made only of `:` and dashes (`:--`, `:-:`) under another line of its
  paragraph, which a GFM reader would take for a one-column table's
  delimiter row. CommonMark readers show neither escape. Every other
  tree the txt reader builds is written byte for byte as before.
- The Markdown writer (`doc::md`) has two modes. The CommonMark mode
  (`write`, `write_limited`) is what `text.txt-to-md` writes; a table in
  it is one line per row (reported) and strikethrough loses its marks
  (reported). A code block's info string gets a backslash before every
  `\` and `&`, since readers take escapes and entities off it. The GFM
  mode (`write_limited_with(.., Dialect::Gfm)`)
  writes everything else the same way and adds: a table as a pipe table
  (a header row, kept even when all its cells are empty, the delimiter
  row `---`, `:--`, `:-:` or `--:` per column, then the body rows, a row
  short of cells padded with empty ones; in a tight list item it
  follows a paragraph without a blank line, and a block after it needs
  one, so such an item reads back loose), every `|` in a cell written
  `\|`, a line break in a cell (a block in a book's cell) written as a
  space and reported as cell line breaks that became spaces only when
  one was there;
  strikethrough as `~~...~~` (adjacent ones joined, a nested one written
  as its outer one, an empty one left out), with a tilde next to a
  delimiter escaped; emphasis, strong emphasis and strikethrough with
  the white space at their edges moved outside the delimiters, a
  delimiter that would touch a run of its own character switched to
  `_`, and a mark whose delimiters still could not open or close by the
  flanking rules (`a<del>"b"</del>c`), or whose opener could close an
  enclosing mark, left out and reported as lost emphasis or
  strikethrough, so a GFM reader never shows a stray `~~` or `*`; a
  line-start `:` escaped, so a paragraph line never becomes a delimiter
  row. Task list items and extended autolinks are written as in the
  CommonMark mode. A `www.` or `://` in text whose run
  (to white space or `<`) would hold one of the writer's escapes, or
  runs into the next inline, gets its `.` or `:` as an entity, so a GFM
  reader does not make a link that holds the backslash; elsewhere a
  GFM reader links the text as it reads.
- `text.md-to-txt` reads a `.md` input as GitHub Flavored Markdown
  (CommonMark plus tables, strikethrough, task lists and extended
  autolinks). The action writes GFM tables as aligned columns in a full
  line of 80 characters (the prefix of an enclosing list or quote
  counts): two spaces between columns, a row of dashes under the header
  (a header whose cells are all empty is left out with its dashes and
  the empty rows right after it), the column alignment kept as padding.
  When the table is wider, the widest columns give up width one
  character at a time, none below 10 (a column that is
  narrower stays as it is) and a cell wraps inside its column, at spaces, a
  longer word at a character boundary. If the minimums and gaps do not fit the
  line, each row becomes `header: cell` lines (`Column <n>` for an empty header)
  with a blank line between rows, and the action reports that the table was
  listed. Widths count characters, so a wide or combining character (East Asian
  Wide and Fullwidth, emoji, combining marks) or a control character such as a
  TAB is reported as possible misalignment. Strikethrough loses its marks
  (reported); a task list item is written with `[x] ` or `[ ] ` after the list
  marker, before its text (no trailing space when the item is empty), and that
  loses nothing. An extended autolink (a bare `www.`, `http://`, `https://` or
  `ftp://` URL, or a bare email address) is written as its text, and that loses
  nothing; a `<...>` autolink is reported as a flattened link, as before.
  Padding and the repeated headers of the listed form count against the output
  budget like any other byte.
- The user-facing XHTML and HTML output goes through its own strict
  writer, `doc::xhtml`; the test writer `doc::html`, which reproduces the
  CommonMark and GFM reference renderers for the spec tests, is not that
  path and stays unchanged. The strict writer's output is well-formed XML:
  characters XML 1.0 forbids are left out (reported), raw HTML is dropped
  (reported), only the five XML entities are written, headings get
  GitHub-style ids (Unicode letters and digits kept, an id that is not an
  XML name gets the prefix `h-`, repeats are numbered `-1`, `-2`), and a
  `#id` link resolves across the chapters of a document to the file that
  holds the heading. `http(s):` and `mailto:` links stay; any other link,
  or one whose target is missing, keeps its text (reported). Every image
  becomes its alt text (reported), in a chapter and in a standalone page
  alike, so a page never loads anything from outside. The one exception
  is a chapter of a book of several documents: an image the book packages
  is an `<img>` of the packaged file with its alt text, and a link to
  another document of the book (`other.md`, `other.md#x`, its path taken
  relative to the linking document and normalized lexically) goes to that
  document's first chapter file or to the heading `x` in it, each
  document's heading ids being the ones it has on its own. Every document
  is named by its path relative to one folder that holds them all (their
  deepest common folder), so `../b/two.md` from `a/one.md` finds
  `b/two.md`, and a link that climbs out of that folder finds none. A task list
  item is written with a `[x] ` or `[ ] ` text marker and table alignment as a
  `left`, `center` or `right` class, so the output needs no form element and no
  inline style. Every piece counts against the output budget before it is
  appended.
- The book packer, `crates/epub` (doc, zip and xml), writes a
  document as an EPUB 3 book through the stored ZIP writer: `mimetype`
  first and stored, `META-INF/container.xml`, `OEBPS/content.opf`,
  `OEBPS/nav.xhtml` (the EPUB 3 table of contents; no NCX),
  `OEBPS/style.css` (tables and code only; typeface, size and measure
  stay the reader's) and one XHTML file per chapter,
  `OEBPS/ch0001.xhtml` onwards. Chapters split at the top-level H1s when
  there are at least two, else at the H2s when there are at least two,
  else the document is one chapter; content before the first split
  heading is its own chapter; at most 1,024 chapters
  (`book would have more than 1024 chapters`, nothing written). The table
  of contents lists the chapters and, one level below, their top-level
  headings of the next level. The metadata is deterministic: `dc:title`
  and `dc:language` (a tag that is not well-formed is `und`) from the
  caller, `dc:identifier` `urn:uuid:` plus a version 8 UUID made from a
  128-bit FNV-1a hash of the input, and `dcterms:modified` the input's
  modification time in UTC (`YYYY-MM-DDThh:mm:ssZ`), so the same input
  gives the same bytes. One chapter's XHTML is in memory at a time; the
  table of contents and the package document are built first from the
  headings. On a failure the packer cuts the file back to empty.
- A book of several documents (`epub::write_book`) splits each document
  the same way; the chapter files are numbered across the book and the
  1,024 limit counts them all. The table of contents lists each document
  under its title (its first H1, else its file stem, from the caller),
  with its chapters below it when it has more than one; a book of one
  document lists its chapters at the top, as above. Images come from the
  caller (the packer opens no file): it resolves each image destination
  of each document once, before anything is written, to one of the
  JPEG or PNG images it was given, already checked and in natural order,
  and copies each packaged image's bytes exactly, streamed, never held
  whole, from the file the caller checked; a short or long copy fails the book.
  An image is packaged once however many destinations refer to it, as
  `OEBPS/img/0001.<jpg|png>` onwards in the order the documents first refer to
  it, and written as an `<img>`; an image no document refers to goes, in the
  caller's order, to a last chapter `Images`, one figure per page with the file
  name as its alt text, and counts toward the chapter limit. Image bytes are not
  charged to the output budget (the caller bounds each image and the archive is
  bounded by the ZIP writer).
- The XML reader, `crates/xml` (no dependencies, no `unsafe`, independent of
  `doc` so other readers can use it), is a non-validating pull parser for
  XML 1.0 (fifth edition) with Namespaces in XML 1.0, for foreign input.
  `xml::decode` takes UTF-8 (with or without a byte order mark, borrowed)
  or UTF-16 with a byte order mark (copied); any other declared encoding
  is `unsupported encoding <name>` (the name cut to 40 characters, the
  IANA maximum, plus `...`), a declaration that contradicts the
  bytes, invalid UTF-8/16 or a second byte order mark is not well-formed. `Reader::next` returns
  start tags (namespace URIs resolved, declarations applied, not listed),
  end tags and text (CDATA, references and line-end normalization
  included); an empty element is a start and an end. It never loads
  anything: a DOCTYPE's external identifier is only checked for syntax,
  and the internal subset is skipped lexically (a `]>` in a literal,
  comment or PI does not end it). No entity is expanded from a DTD: a
  reference to one the internal subset declares, or that an external
  subset or parameter entity may declare (unless `standalone="yes"`),
  is `entity declarations are not supported`, so entity bombs and
  external entities cannot occur. Known references are the five XML
  entities, character references to characters XML allows, and, only
  with `Options::html_entities` (XHTML), the HTML5 named characters
  (`xml::entity`, the WHATWG table the Markdown reader also uses).
  Anything not well-formed or not namespace-well-formed is an error with
  its line; there is no recovery, and attribute defaults from the
  internal subset are not applied. A namespace name holding a reference
  or a line break is refused, so names and values borrow from the input
  and only text or a value that a reference or normalization changes is
  copied. Elements nest at most `Options::max_depth` (`MAX_DEPTH`, 256)
  deep, past it `TooDeep`; the open elements are the reader's own stack,
  not recursion, and the work is linear in the input (a step count of
  every byte examined, copied, hashed or compared, which the tests and the
  fuzz target bound per byte): a namespace name is hashed when its
  declaration comes into and goes out of scope, and attributes are told
  apart by its id, so a long one costs its length, not its length per
  attribute.
- The EPUB reader, `epub::read`, opens a book on disk that is foreign
  input. `open` reads the ZIP directory (the CBZ reader's limits: 12,000
  entries, ratio 1032, 512-byte names, no ZIP64), then
  `META-INF/container.xml` and the first rootfile of type
  `application/oebps-package+xml`, then that package document (EPUB 2 or
  3: the spine is the same in both): the first non-empty `dc:title` and
  every non-empty `dc:creator` in order (whitespace collapsed), the
  manifest and the spine, and `META-INF/encryption.xml` if there is one.
  The `mimetype` entry is not required. All three files are plain XML (no
  HTML entity names), at most 8 MiB and 2^19 XML nodes (elements,
  attributes and text runs) each. Every `itemref` is kept in spine order,
  `linear="no"` included, a repeated one as often as it appears, numbered
  from 1 (the `N` of `Chapter N`); the manifest item it names is resolved
  and kept once, however often the spine names it, its path at most 512
  bytes (the longest entry name: a longer one is `missing`) and its media
  type at most 255 (RFC 6838: a longer one is `unsupported type`). An
  `href` is relative to the package's folder: the `?query` and `#fragment`
  are cut, `%XX` decoded, the path normalized lexically
  (`doc::xhtml::normalize`), and the entry name matched exactly, case
  included; an href with a URL scheme, an absolute one, or one that climbs
  above the container root (also once decoded) is `outside the book` and
  never looked up. A resource `encryption.xml` lists under an
  `EncryptedData` with a cipher among its own `EncryptionMethod`s (in any
  order), or with none, is encrypted, and so is a ZIP entry with an
  encryption flag; font obfuscation (`http://www.idpf.org/2008/embedding`,
  `http://ns.adobe.com/pdf/enc#RC`) is not, and `rights.xml` decides
  nothing. When every XHTML spine item in the book is encrypted the book
  is `book is DRM-protected`; otherwise an encrypted item fails alone,
  `encrypted`. `Book::chapter` reads one spine item's raw bytes when asked
  and keeps nothing (media types are compared by `type/subtype`, ASCII
  case ignored, parameters cut): an XHTML item (`application/xhtml+xml`)
  up to 8 MiB, an SVG page as nothing (it has no text), anything else is
  `unsupported type <mime>` (`unsupported type (none)` when the item
  states none). A broken skeleton fails the whole book (`BookError`, the
  texts in `docs/resource-limits.md`); a spine item that
  is missing, outside the book, encrypted, too large, of another type or
  whose data is damaged fails only itself (`ChapterError`) and the rest of
  the book reads. `Book::resolve` resolves an `href` or `src` written in a
  chapter by the same rules, relative to the chapter's folder (not the
  package's); one that is empty once the query and fragment are cut
  (`#id`) names the chapter itself; a chapter outside the book, or a path
  over 512 bytes, resolves to nothing. `Book::resource` reads one file
  entry by its exact name under a cap the caller gives, checked on the
  declared size before anything is reserved or inflated and on the bytes
  produced (`too large`); no such file entry is `missing`, a ZIP
  encryption flag or a cipher in `encryption.xml` is `encrypted`, the ZIP
  reader's reason is `damaged`. Neither keeps anything.
- The XHTML chapter reader, `doc::xhtml_read`, maps one chapter (decoded by
  `xml::decode`, then read with the HTML named characters and the XML
  depth limit) to the document tree; text disappears only with a reported
  loss. Kept: `p`, `h1`-`h6`, `ul`/`ol` (`start`, at most 999,999,999),
  `li`, `blockquote`, `pre` (markup inside keeps its text, `br` is a line
  feed, a block inside is a line of its own, a cell a space), `hr`, `br`,
  tables, `em`/`i`, `strong`/`b`, `del`/`s`/`strike`, `code`/`kbd`/`samp`,
  `a href` (a link inside a link is its text), `img` (`src`, `alt`) and
  `ruby` (base text, each `rt` in parentheses; an `rp` holding only
  brackets and white space is dropped, any other `rp` text kept). The HTML
  block containers (`div`, `section`, `aside`, `figure`, `dl`, …) go and
  end a paragraph, `span` and unknown elements (MathML, `epub:` elements)
  go without ending one; their content stays. `head`, `script`, `style`,
  `template` and their content are dropped, inline SVG too (reported as
  images), and without a loss MathML `annotation`/`annotation-xml`
  (another encoding of the same formula; in XHTML or no namespace only
  inside a `math`) and `epub:case` (an `epub:switch` alternative; its
  `epub:default` is read). Elements in no namespace are read as XHTML
  (EPUB 2 chapters written without it), names ASCII case-insensitive; any
  other namespace is transparent. White space collapses as in HTML except
  in `pre` (whose leading line feed is dropped); a no-break space is text.
  Inline content at block level becomes a paragraph; a block where only
  inline content fits (a paragraph in a cell or in emphasis) keeps its text
  on lines of its own. A table's header is its `thead` row or a first row of
  `th` cells, else empty; `tfoot` rows come last; a cell spanning columns or
  rows (clamped as HTML does, 1000 and 65534) is kept once and the cells it
  covered are empty (`merged table cells split`); an element between rows
  that holds no text (an anchor, white space) makes no row, so it takes no
  row span; a caption is a paragraph
  before the table. Stylesheets and `style` attributes are never read: every
  chapter reports `book styles (fonts, colours, layout) not carried over`.
  Budgets: every element is a node, charged when it starts, and so is every
  node it builds besides its own (a text run, an implicit paragraph, row or
  cell, an empty cell a span adds); text is charged by its collapsed bytes;
  both before the allocation, so a chapter stops at `Exceeded("nodes")` or
  `Exceeded("owned text")`. The work is linear in the chapter plus the nodes
  charged (a step count the tests and the fuzz target bound), with the
  builder's own stacks, no recursion. `has_text` tells whether a document
  holds any text that is not white space (an image's alt text is not).
  `read_with_ids` reads the same tree and, beside it, the chapter's `id`
  attributes (`Ids`): an id on a heading the tree keeps is that heading's,
  by its ordinal (the headings counted from 0 in document order, nested
  ones included); so is the id of a block container (`div`, `section`
  and the other transparent block elements, `blockquote`, `li`) whose
  first block is that heading, so a link to a section lands on its title
  (outer containers first). Every other id (a footnote anchor, a
  paragraph, a span, a heading where only inline content fits, an empty
  heading, a container with something before its heading or none) is in
  `others`. Ids are kept as written, an empty one is none, an element
  dropped with its content keeps none; their text is owned text, charged
  before it is copied, so a chapter near the limit can fail with ids and
  still read without them (the text actions read without).
- `text.md-to-epub` and `text.txt-to-epub` (`group=book`, `lossy=yes`) write
  one book per input into the work directory, `<stem>.epub`. The title is
  the first non-empty H1 of a Markdown input, else the file's stem (always
  the stem for plain text); the language is the engine's `INACIRCLE_LANG`,
  `und` when it is missing or not a well-formed tag; the identifier is
  made from a digest of the input and the modification time is the
  input's. The output file is created new (never overwritten) and removed
  when the book fails, so no half book is left; losses of the reader and
  the writer (raw HTML, images, links) are reported once each.
- `text.md-join-epub` (`group=book`, `lossy=yes`, `join=yes`,
  `assets=image/jpeg,image/png`) joins Markdown files and the images they
  use to one book, `join/out.epub`. An input is an image when its header
  is a JPEG or PNG one (`probe::sniff`: any valid variant, bytes packaged
  as they are), else a document. Each document is one top-level chapter
  (split ones have sub-chapters), in the order given; the title is the
  result's name (`action::anchor_of`, the one rule the engine places the
  result by: the folder holding most documents) for two or more
  documents, else the document's own title as above. An image destination
  is resolved against the document's folder (percent-decoded, `.` and `..`
  lexically); the resolver only `stat`s the path it names (it never opens
  a file that is not an input) and compares device and inode with the
  given images (a symlink, `./a.png` and `a.png` are one image), or the
  alt text stays and the book reports the loss. A link to another
  given document goes to its chapter file and heading. A document that
  fails leaves the rest; a damaged image fails alone (its place keeps the
  alt text); no document left, no book. The book failing as a whole (the
  chapter limit, the output ceiling, a write error) fails every input it
  would have taken and leaves no file. `take` comes per taken input after
  the book is finished, `joined` once.
- `text.md-to-html` and `text.txt-to-html` (`group=document`, `lossy=yes`)
  write one self-contained page per input, `<stem>.html`, as a single
  chapter: `<!DOCTYPE html>`, `lang` (as for the book), `<meta charset>`,
  `<title>` (the same rule as the book's title), an embedded stylesheet
  of at most 1.5 KiB with a dark mode through `prefers-color-scheme`, no
  script and nothing loaded from outside. The losses are those of the
  book. The page is created new too (never overwritten) and checked
  against the output size ceiling before it is written.
- `text.epub-to-txt` (`in=application/epub+zip`, `group=document`,
  `lossy=yes`) writes one plain-text file per book, `<stem>.txt`. The book
  file is read entry by entry (`epub::read`), never whole, so it is not
  held to the text input budget. The file starts with the book's title
  and its authors (joined by `, `), one line each, and a blank line; what
  the book lacks is left out, and with neither, the header too. Then
  every spine item in spine order (`linear="no"` ones in their place),
  each with the plain-text writer's block rules, a blank line between
  chapters; a chapter's title is its own heading, none is added and the
  navigation document is read as any chapter. One chapter at a time:
  its bytes, `xml::decode`, `doc::xhtml_read` under fresh text-family
  budgets, the writer under what is left of the book's output budget,
  appended to the file; its tree is dropped before the next. A link to
  a place in the book (no URL scheme, such as `notes.xhtml#fn1` or
  `#x`) is written as its text alone; one with a scheme, or to another
  host (`//host/...`), as `text (destination)`; either is reported. An
  SVG page has no text and is reported as an image. A spine item that cannot be read
  is the paragraph `[Chapter N could not be read: <reason>]` in its
  place (the reasons in `docs/resource-limits.md`), and one `warn`,
  `N chapter(s) could not be read`, counts them; then each loss once
  (styles always: no stylesheet is read). A book that cannot be read
  fails with the reader's reason before any file exists; when no
  chapter could be read (`no chapter could be read`), none holds text
  (`book has no text`), or the text passes the 32 MiB output budget
  (`book is too large`) or the output ceiling (`Plain text would be
  larger than <N> MiB`), the file is cut back to empty and removed and
  the input fails.
- `text.epub-to-md` (`in=application/epub+zip`, `group=document`,
  `lossy=yes`, `batch=no`) writes one GitHub Flavored Markdown file per
  book, `<stem>.md`, and beside it the JPEG and PNG images its chapters
  show, `<stem>-0001.jpg`, `<stem>-0002.png`... (four digits from 1, in
  the order of their first reference; the names come only from the
  input's stem and the counter, never from the book). The file starts
  with `# <title>`, the authors (joined by `, `) as a paragraph and a
  blank line, what the book lacks left out; then every spine item as in
  `text.epub-to-txt`, written by the GFM writer (tables as pipe tables,
  strikethrough as `~~`), a blank line between chapters (a chapter's
  first list after a list of its kind takes the other marker, `*` or
  `)`, so a reader keeps the two lists apart and an ordered one its
  start), and an
  unreadable one as the paragraph `\[Chapter N could not be read:
  <reason>\]` (escaped, so it is never a link). The book is read twice:
  first every chapter for its headings only, which gives each heading
  its id as a viewer and the join number them over the whole md
  (`doc::xhtml::slug`, the title heading first, repeats `-1`, `-2`... in
  order) and records where each heading `id` attribute lands; then one
  chapter at a time to write it. A link into the book (no URL scheme)
  becomes `#<id>` when it lands on a heading (its fragment names a
  heading's `id`, an `id` inside the heading such as a table of
  contents anchor `<h2><a id="c2"/>II.</h2>`, or that of a container
  the heading leads; or with no fragment the chapter has a first
  heading), else it keeps only its text. The `<id>` is the anchor a viewer
  (GitHub) gives the heading: the slug without the `h-` an EPUB id needs
  before a digit (`#1984`, `#1984-1`), which the join still finds; a
  heading whose text gives no anchor (`!!!`) cannot be linked, and a
  heading text that is the prefix itself (`H 1` beside `1`) is numbered
  by our rule, not GitHub's; nor do decomposed accents (`e` + U+0301,
  left out of ours) and white space other than a plain space (a no-break
  space, `-` in ours) give GitHub's anchor, so such a heading's link may
  miss in a viewer, while the join resolves it. Further, `http:`,
  `https:`, `mailto:` and
  `//host/...` links stay as they are, any other scheme keeps only its
  text (so the md never carries a `javascript:` or `file:` link); each
  dropped link is reported. An image is resolved in the book like a
  link; the entry is read once however often it is named (two paths to
  one entry are one file) and written byte for byte when the image probe
  finds a JPEG or PNG of at most 180 megapixels (one the PDF writer
  cannot pass, such as a PNG with alpha, is kept too). The bytes are
  copied as they are. An image with a feature the PDF writer lacks is
  checked by its header only (type, size, pixel count); a PNG the PDF
  writer supports is also checked in structure (chunk CRCs, chunk order,
  the IDAT run), so a damaged one keeps its alt text. Compressed pixel
  data is never decoded, so damage inside it is not detected (a viewer
  shows such an image broken). Every other image (GIF, WebP, SVG, a damaged header, missing,
  outside the book, encrypted, over the limits in
  `docs/resource-limits.md`) keeps
  its alt text in its place, and one `warn`, `N image(s) replaced by alt
  text`, counts those places; it stands for the images loss, which then
  has no line of its own, so the `warn` lines stay within the
  supervisor's 16. The md links to an image by its file name
  (`![alt](<stem>-0001.jpg)`, in `<...>` when the name has a space; `%`,
  `#`, `?` and control characters percent-encoded). When the stem is so
  long that an image's name does not fit the file system (it is 6 bytes
  longer than the md's), the images keep their alt text and the md is
  still made when the book has text. A book with no text but at least
  one image written is made (a picture book); with neither, it fails:
  `image names too long for this folder` when an image could not be
  written for its name, else `book has no text`. Nothing is reported
  before the book is whole: then one `done`
  per image in order, the md last, and the `warn` lines. On any failure
  every file written for the book (the md and the images) is cut back to
  empty and removed and the input fails with no `done`, so nothing of it
  is published. The md and its images, joined again by
  `text.md-join-epub`, make a book again when it is within the EPUB
  writer's limits (1024 chapters, one document's budgets); a larger
  book's md is still written but cannot be joined back in one piece.
- `text.cp1250-to-utf8` and `text.iso88592-to-utf8` (`in=text/x-8bit`,
  `out=text/plain`, `ext=same`, `group=encoding`, `lossy=no`) decode a
  Windows-1250 or ISO-8859-2 file to a UTF-8 copy under the input's own
  name; the label is the code page the file is read as, the target is
  always UTF-8. The upper 128 bytes map by the Unicode Consortium's
  mapping files (`VENDORS/MICSFT/WINDOWS/CP1250.TXT`,
  `ISO8859/8859-2.TXT`), built into the binary and checked byte for byte
  against copies of the files and against glibc's `iconv` by the tests.
  The file is streamed in 64 KiB reads, so it has no size limit of its
  own (the engine's 3 x cap bounds the output). Strict, never `U+FFFD`:
  each error gives no output, with the byte's offset from 0. A byte
  Windows-1250 leaves undefined (`0x81 0x83 0x88 0x90 0x98`) is `byte
  0x81 at N is not Windows-1250`. ISO-8859-2 `0x80-0x9F` are C1 controls
  (in the mapping file and in glibc, but never in a text file), so they
  are refused: one Windows-1250 assigns (its quotes, dashes, `Š`...) is
  `byte 0x84 at N: this looks like Windows-1250`, one it does not is
  `byte 0x81 at N is not ISO-8859-2`. NUL or a C0 control other than
  TAB, LF, FF and CR is `not text: control byte at N`; one Ctrl-Z
  (`0x1A`) as the very last byte is allowed and kept. Line ends are kept
  byte for byte and no byte order mark is written, except for a `.srt`
  input (any case), whose copy starts with one (players on old TVs and
  Windows read a subtitle without it as the local code page). A file
  that is already UTF-8 (valid, with at least one multi-byte character)
  is skipped, `already UTF-8`; one that is mostly UTF-8 fails with
  `looks like UTF-8 with damaged bytes (first at N)` instead of turning
  every `é` into `Ă©`. Mostly UTF-8 is: over the whole file, as many
  valid UTF-8 sequences of Latin letters, punctuation or symbols
  (U+0080-U+024F, U+2000-U+206F, U+20A0-U+214F) as bytes outside valid
  UTF-8, or more; or a UTF-8 byte order mark first and any byte outside
  valid UTF-8 after it. Real 8-bit text almost never forms such a
  sequence: a lower-case accent is followed by ASCII, and most pairs
  capitals form by accident (`ŁÓŻKO` holds `D3 AF`) decode to Cyrillic,
  Hebrew, Arabic or Armenian letters, which do not count. The residue:
  a capital `Â Ă Ä Ĺ Ć Ç Č` before any byte `0x80-0xBF` (or `É` before
  `0x80-0x8F`) decodes into the Latin range, so one-line all-caps Slovak
  or Czech such as `DĹŽKA`, `PÄŤ` or `ČŠ` is refused; lower-case text or
  more lines outweigh it. These two verdicts win over a refused byte
  (`Á` in UTF-8 is `C3 81`) and need the whole file: the copy is
  written as the input streams and, on any failure, cut back to empty
  and removed. A pure ASCII file converts to itself. A converted input
  gets one `warn`, `converted from Windows-1250 to UTF-8` (or
  `ISO-8859-2`): the contract has no other line for a note on a
  converted input; a route without a lossy action does not count it as
  a loss.
- The text family stops a document at structure budgets, charged while it
  is built (before the allocation): input 8 MiB, 524,288 nodes (blocks
  and inlines, including link reference definitions), 32 MiB of owned
  text (link reference expansion included), 32 MiB of output. Past one,
  the input fails with the budget's own message and the batch goes on.
- The subtitle family (`inacircle-subtitles`, `family=subtitles`,
  `group=subtitles`) has six actions, each reading one format and writing one:
  `subtitles.srt-to-vtt`, `subtitles.vtt-to-srt`, `subtitles.ass-to-srt`,
  `subtitles.ass-to-vtt`, `subtitles.ttml-to-srt` and `subtitles.ttml-to-vtt`.
  Their MIME types are `application/x-subrip` (SRT), `text/vtt` (WebVTT),
  `text/x-ssa` (ASS and SSA) and `application/ttml+xml` (TTML). All are `cost=10`,
  `lossy=yes` (a drawing is dropped and a speaker's name becomes text) and
  `batch=yes`. The input is read whole and refused past 8 MiB (`subtitle file
  is larger than 8 MiB`) before any parsing. The readers store at most
  2,000,000 items (each cue, line and run is one; the TTML reader also charges
  each element, piece and copy of a piece, and each element that holds a
  visibility `set` or `animate` outside a paragraph), each charged before it is stored; past it the input fails with `too many lines and styled runs (more
  than 2,000,000)` and nothing is written. A WebVTT speaker's name is shared by
  the lines of its cue. A file that is not UTF-8 fails
  with `not UTF-8 text (byte at N): convert it with Encoding first`. A file
  with no usable cue fails with `no subtitles found` when nothing in it was
  skipped (empty, or no cue at all), and otherwise says how many items were
  skipped and the first one's line and reason, in every format
  (`no subtitles found (N items skipped, first at line L: <reason>)`). A TTML
  file fails with `not well-formed TTML (line L): <reason>`, `not a TTML
  document`, `unsupported TTML: <reason>` (a clock time base, a drop-frame time
  code, a declared encoding other than UTF-8 or UTF-16, entity declarations) or
  `no text subtitles found (image-based)`. Text copied into cues past 32 MiB
  fails with `subtitle text would be larger than 32 MiB`, not with the item
  budget's message. TTML's own limits are the element depth of the XML reader
  (256) and the style chain of 32 entries; see [TTML accounting](#ttml-accounting)
  and `docs/resource-limits.md`. The output is counted as
  it is written: past 32 MiB the input fails with `subtitle output would be
  larger than 32 MiB`. SIGXFSZ is ignored, so a write past the process's
  `RLIMIT_FSIZE` fails the same way and not the batch. A failed input leaves
  no output file, and an existing output is never overwritten (the file is
  created new). Cues are written in start order; cues that start together keep
  the file's order.
- The readers keep what a player shows and say what they drop.
  - **SRT** follows the players (a differential test pins it to ffmpeg): LF,
    CRLF or CR line ends; the cue number is ignored; `H+:MM:SS,mmm` (a `.` is
    accepted, and milliseconds are read as an integer); the coordinates after
    the end time are dropped, which is `Position` (once per cue). `<i> <b> <u>` and their
    `{\i1}`-style forms are kept (a bare `{\i}`, `{\b}` or `{\u}` switches off);
    `{\an7}` to `{\an9}` and `{\a5}` to `{\a7}` (the SSA top row) make a top
    cue. `<font>` keeps its text and is `Styling`; `<br>` (any case, `</br>`
    too, a `/` or a space allowed) is a line break, a normal line for the
    budget; other tags are dropped with their text kept. A `<` opens a tag only
    when its name (up to a space or `>`) holds letters, digits, `_` or `/` and
    starts with a letter, and no `<` comes before the `>`; any other `<` is
    text, so a URL or an address stays. `{\pos}` and `{\move}` are `Position`,
    `{\k}` and `{\K}` are `Karaoke`, a non-zero `{\p}` is `Drawing` (its text is
    dropped), and any other `{\...}` block is `Styling`.
  - **WebVTT** follows the W3C file parsing algorithm: the `WEBVTT` signature
    (a byte order mark is allowed) and the header are read, and `NOTE` blocks
    are not reported. A `STYLE` or `REGION` block makes every later cue
    `Settings`. A valid timing line inside a `NOTE`, `STYLE` or `REGION` block,
    or in a block already reported, starts a cue, as the standard does: a
    `-->` line ends such a block and is read again as the next block's first
    line. The one difference is a `-->` that is no timing line: the standard
    ends the block at it too, we keep it in the block (neither starts a cue).
    A `line:` setting in the top third makes a top cue; any other
    placement is `Position`. `<c.class>` and `<v.class>` are `Styling`, a
    timestamp tag is `Karaoke` (its text stays), `<v Name>` is the speaker (its
    classes are not part of the name; the name is decoded, then trimmed of
    ASCII white space only, so a no-break space stays), and other tags are dropped with their
    text kept. The
    entities `&amp; &lt; &gt; &quot; &apos; &nbsp; &lrm; &rlm;` and numeric
    references (`&#N;`, `&#xH;`) to any valid character but NUL and the line
    breaks are decoded; any other `&name;` stays as written.
  - **ASS and SSA** follow libass: each section's `Format:` line gives the
    columns, `Dialogue:` lines are cues and `Comment:` lines are dropped
    silently. The style's bold, italic and underline and its alignment (7 to 9
    in V4+, 5 to 7 in V4 styles, a top row) set the cue's base; the inline
    `\i`, `\b`, `\u`, `\an`, `\a` and `\r` change it. A bare `\i`, `\b` or
    `\u` returns that flag to the style of the last `\r`, as libass does, and
    so does any other value, except that a bold weight of 100 or more is a
    font weight (`\b400` is off, `\b700` on). `\N` breaks a line; `\n`
    breaks it only under `WrapStyle: 2` and is a space otherwise; `\h` is a
    no-break space. `\k` is `Karaoke`; `\p1` to `\p0` is `Drawing` and its
    text is dropped; `\pos`, `\move` and `\org` are `Position`; other tags are
    `Styling`.
  - A cue that cannot be used (a bad timestamp, an end before its start, 100
    hours or more) is skipped and the rest converts. Text outside any cue is
    one skipped block per run of lines, counted and never shown. A cue with no
    text left is not kept (a line of only white space is no text); a drawing
    removed from it is still `Drawing`, but styling, position and karaoke
    removed no text, so they are not counted.
- The writers. SRT is UTF-8 with a byte order mark and CRLF, numbered from 1;
  a speaker is a `Name: ` prefix (`VoiceNames`), `{\an8}` opens a top cue, and
  `<i> <b> <u>` are kept. The SRT reader has no escapes, so a `<` the reader
  would take as a tag (`<` or `</`, a letter, then letters, digits, `_` or `/`
  up to a space or `>`, and a `>` before any other `<`; the same test the
  reader uses) is written as U+FF1C, and a `{\` the reader would take as an
  override block (with a `}` after it) as U+FF5B, both look-alikes, counted
  once per cue as `Brackets`. A rewritten `<` no longer stops the reader's
  search for `>`, so the choice runs from the end of the line back; `<` with
  no `>` after it, `{\` with no `}`, stay text. The choice is made on the
  plain text, without the writer's own `<i>` and `</i>`, so at a style
  boundary a `<` or `{\` the reader would keep as text can be rewritten
  too: a counted look-alike, accepted. The guarantee is that the writer never
  leaves unrewritten anything its reader would take as markup. The `{\an8}`
  the writer puts before a top cue is not text and is kept. A text line that
  the reader would take for a timing line (`-->` with a start that parses as a time, the
  reader's own test on the line as written, marker and tags included) would
  start a cue on read back, so the `>` of each of its `-->` is written as
  U+FF1E, also counted as `Brackets`; the writer's own timing lines are not
  touched. WebVTT has no byte
  order mark, LF only, ` line:0` on a top cue, `<v Name>` for a speaker, and
  `<i> <b> <u>`; in its text `&`, `<` and `>` are escaped, and a speaker name's
  trailing dashes are written as `&#45;` (the `>` of its tag would make a `-->`).
  In both, a line
  break inside a cue becomes a space, a NUL becomes U+FFFD, a line of only
  white space and a cue with no text are dropped, and a blank line ends a cue.
- The subtitle `warn` lines, at most nine per input: first the skipped cues
  (`N items skipped (first at line L: <reason>)`, `L` 1-based, the count
  including text outside any cue), then one line per kind of loss in the order
  of `Loss` (colours, fonts and sizes; exact positions; karaoke effects;
  drawings; style and region blocks; speaker names written as prefixes;
  characters that look like markup written as look-alikes; unsupported elements
  ignored), each counted in cues as `(N cues)`, except unsupported elements,
  which count the elements and images they ignore, so `(N items)`. The
  reader's and the writer's counts of one kind are added together. At most
  nine lines (eight kinds and the skipped line), far under the engine's 16
  `warn` per input.
- A plugin may add text types by extension (`type text/x-typst ext=typ`).
- Format: line-based, tab-separated, versioned header. A small parser of
  our own, no JSON dependency.

```
inacircle-manifest 0
action	text.txt-to-md	family=text	in=text/plain	out=text/markdown	ext=md	label=Markdown	group=document	cost=10	lossy=yes	batch=yes
action	text.md-to-txt	family=text	in=text/markdown	out=text/plain	ext=txt	label=Plain text	group=document	cost=10	lossy=yes	batch=yes
action	text.md-to-epub	family=text	in=text/markdown	out=application/epub+zip	ext=epub	label=EPUB	group=book	cost=10	lossy=yes	batch=yes
action	text.txt-to-epub	family=text	in=text/plain	out=application/epub+zip	ext=epub	label=EPUB	group=book	cost=10	lossy=yes	batch=yes
action	text.md-to-html	family=text	in=text/markdown	out=text/html	ext=html	label=HTML	group=document	cost=10	lossy=yes	batch=yes
action	text.txt-to-html	family=text	in=text/plain	out=text/html	ext=html	label=HTML	group=document	cost=10	lossy=yes	batch=yes
action	text.md-join-epub	family=text	in=text/markdown	assets=image/jpeg,image/png	out=application/epub+zip	ext=epub	label=EPUB	group=book	cost=10	lossy=yes	batch=yes	join=yes	min=1
action	text.epub-to-txt	family=text	in=application/epub+zip	out=text/plain	ext=txt	label=Plain text	group=document	cost=10	lossy=yes	batch=yes
action	text.epub-to-md	family=text	in=application/epub+zip	out=text/markdown	ext=md	label=Markdown	group=document	cost=10	lossy=yes	batch=no
action	text.cp1250-to-utf8	family=text	in=text/x-8bit	out=text/plain	ext=same	label=Windows-1250	group=encoding	cost=10	lossy=no	batch=yes
action	text.iso88592-to-utf8	family=text	in=text/x-8bit	out=text/plain	ext=same	label=ISO-8859-2	group=encoding	cost=10	lossy=no	batch=yes
action	pages.cbz-to-pdf	family=pages	in=application/vnd.comicbook+zip	out=application/pdf	ext=pdf	label=PDF	group=book	cost=10	lossy=yes	batch=yes
action	pages.images-to-pdf	family=pages	in=image/jpeg,image/png	out=application/pdf	ext=pdf	label=PDF	group=book	cost=10	lossy=yes	batch=yes	join=yes	min=1
action	pages.images-to-cbz	family=pages	in=image/jpeg,image/png	out=application/vnd.comicbook+zip	ext=cbz	label=CBZ	group=book	cost=10	lossy=no	batch=yes	join=yes	min=1
action	subtitles.srt-to-vtt	family=subtitles	in=application/x-subrip	out=text/vtt	ext=vtt	label=VTT	group=subtitles	cost=10	lossy=yes	batch=yes
action	subtitles.vtt-to-srt	family=subtitles	in=text/vtt	out=application/x-subrip	ext=srt	label=SRT	group=subtitles	cost=10	lossy=yes	batch=yes
action	subtitles.ass-to-srt	family=subtitles	in=text/x-ssa	out=application/x-subrip	ext=srt	label=SRT	group=subtitles	cost=10	lossy=yes	batch=yes
action	subtitles.ass-to-vtt	family=subtitles	in=text/x-ssa	out=text/vtt	ext=vtt	label=VTT	group=subtitles	cost=10	lossy=yes	batch=yes
action	subtitles.ttml-to-srt	family=subtitles	in=application/ttml+xml	out=application/x-subrip	ext=srt	label=SRT	group=subtitles	cost=10	lossy=yes	batch=yes
action	subtitles.ttml-to-vtt	family=subtitles	in=application/ttml+xml	out=text/vtt	ext=vtt	label=VTT	group=subtitles	cost=10	lossy=yes	batch=yes
```

## Invocation

```
<family> run <action-id> --work <temp dir> [--opt key=value] [--inputs-from <file>] -- <absolute paths>
```

- argv only, never a shell. The engine decodes `file://` URIs and passes
  local files only. stdin is `/dev/null`, the working directory is the
  temp dir. Very long input lists go through `--inputs-from <file>`; the
  list (read bounded: 1 MiB, 8 KiB per encoded line) and argv together
  hold at most 1024 paths of at most 4096 bytes each and 1 MiB in all.
  The engine writes the list streamed and holds each input path once for
  the whole run (see `docs/resource-limits.md`).
- **An action writes only into the temp dir.** Once its whole process
  group is gone, the engine copies each input's results into fresh files
  of its own and publishes them with one atomic no-replace `rename` (see
  Output policy). So the output policy lives in one place, undo knows
  exactly what was created, no half-written file ever appears in the
  user's folder, and a broken plugin cannot overwrite anything.
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

  A join action (`join=yes`) reports per input `take<TAB><input>` (the
  input went into its one result), `skip`, `fail` or `warn`, and once, at
  the end, `joined<TAB>join/<file>`: its one result, written directly in
  `<work>/join/` (`<work>/join/out.<ext>` by convention). It never writes
  `done`; a one-to-one action never writes `take` or `joined`. Each of
  these is a contract violation, and so are a second `take` of an input,
  a `take` of an input it was not given, a `joined` before any `take`, a
  second `joined`, a `take`, `skip` or `fail` after `joined`, and a
  `joined` path that is not a regular file directly in `join/` (reached
  through no symlink).

  A join action with assets (`assets=`) gets one list: its documents
  first, in natural order, then its assets, in natural order. The engine
  does not mark the roles; the action tells them apart by content. It
  reports `take` for an asset as for a document.

  An action may open with `progress<TAB>0.00` before any input work
  (`inacircle-text run` always does), so a script that parses stdout must
  not take the first `progress` line for the end of a run.

  stderr is free-form; on failure its last lines go into the message.
- Exit codes: 0 done, 1 failed, 2 contract or invocation error (ours, not
  the file's), 3 unsupported input. For a batch: 0 if at least one input
  produced `done`; otherwise 1 if any input produced `fail`; 3 only when
  every input was unsupported. Exit 1 also covers a batch that cannot run
  at all (`--work` missing or not writable). A join action exits 0 if it
  reported `joined`; 3 if it took nothing and skipped every input;
  otherwise 1.
- A join action's result is published only if it reported `joined`, took
  at least `min` documents (assets never count), broke no rule of the
  contract, exited 0, and no cancel came before the commit. Otherwise nothing
  reaches the user's folder: every input it did not settle with its own `skip`
  or `fail` fails (with the violation, the exit status, "action reported fewer
  inputs than its minimum", or "cancelled"). An input it said nothing of fails
  ("no result reported") even when the result is published. Once published,
  every input it took is done, with the same one output.
- Cancel with `SIGTERM` to the action's process group, `SIGKILL` 5 s
  later; the engine removes the temp dir either way.
- If the supervisor itself dies abnormally (`SIGKILL`, an abort), nobody
  kills the group: the leader dies with it (`PR_SET_PDEATHSIG`, set
  before the sandbox, which then denies changing it), but the leader's
  own children live on under the next subreaper (e.g. `systemd --user`),
  still sandboxed, writing only into the work dir (a later run in that
  folder removes it after a day); nothing of them is published, and `jobs` and
  `cancel` no longer see them. A cgroup would bound that; it is not used (no
  systemd dependency).
- Limits, set in the child: `RLIMIT_FSIZE` caps **each file** an action
  writes at 16 × the total input size (all the inputs of that one
  process, a join's included), clamped to 256 MiB – 16 GiB
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
  accepted. A member that was killed with `SIGKILL` before the leader
  exited and that nobody waited for (the kernel's OOM killer, a hard
  `RLIMIT_CPU`, `kill -9 $job; exit` in a script) also counts as left
  behind: the engine cannot tell it from one its own kill caught, and
  fails closed.

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
- **Compatibility limits.** The policy denies by name, so a converter that
  needs a denied call fails with `EPERM`, `EACCES` or `ENOSYS`. Known
  costs: the ioctl default-deny (for example `cp --reflink`'s `FICLONE`),
  the scheduler, affinity and priority setters (OpenMP thread pinning,
  `nice`), System V IPC and POSIX queues, and `F_ADD_SEALS`. The shipped
  actions and the test fixtures run; converters from third parties have
  **not** been tried. A new action that needs one of these calls needs a
  decision, not a silent exception.
- Not covered: no `RLIMIT_NPROC` (an action can fork-bomb until cancelled).
  `RLIMIT_DATA` is per process, so each child of a forking action gets the
  cap again.
- **Clean environment**: the parent's is cleared; the child gets only
  `PATH=/usr/bin:/bin`, `LC_ALL` and `LANG` `C.UTF-8`, and `TMPDIR` (an
  action's private `<work>/tmp`, mode 0700). An action also gets
  `INACIRCLE_LANG`, the user's language code computed by the engine: the
  first non-empty of its own `LC_ALL`, `LC_MESSAGES`, `LANG`, cut at the
  first `_`, `.` or `@`, lowercased, if that is 2 or 3 ASCII letters; else
  `und` (so `hu_HU.UTF-8` gives `hu`, `LC_ALL=C` gives `und`). A manifest
  probe does not get it. It adds no path or syscall, so the ruleset and
  the filter are unchanged. No session socket, bus, display, token or
  loader variable is passed through.
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
- **Tested boundary, not proof.** The escape tests run under the same
  seccomp policy and Landlock rules the product uses. On a host that
  already refuses `unshare` to an unprivileged user (an AppArmor or
  `max_user_namespaces` policy, or a PID namespace without a user
  namespace), the `EPERM` is the host's, not the filter's; the filter's own
  denial of namespaces is covered by the policy-table tests. What the
  sandbox does not promise is in [sandbox-hardening.md](benchmarks/sandbox-hardening.md).

## Classifier

- **Our own, built in,** not the system's `mime.cache`: the same file must
  give the same wheel on every machine, and the system database differs
  between distributions and versions. Recognising a type we have no action
  for is useless anyway.
- Binary formats: magic bytes in the first few KB decide; content beats a
  wrong extension.
- **Comic books:** a file that starts with the ZIP signature and has the
  extension `cbz` (any case) is `application/vnd.comicbook+zip`, label
  `Comic book`. Only the head already read and the name decide; the engine
  does not open the archive. Any other ZIP stays a label ("ZIP") without a
  type.
- **EPUB books:** the same rule for the extension `epub` (any case):
  `application/epub+zip`, label `EPUB`. The `mimetype` entry is not read at
  classification; the action validates the book.
- **Images:** a PNG or JPEG signature gives `image/png` or `image/jpeg`
  (the join action `images-to-pdf` takes them). Whether a given file is one
  the action can place (a PNG with alpha, say) is the action's call, not the
  classifier's.
- **Planned, not built yet:** zip containers are looked into: ODF and
  EPUB declare their type in the `mimetype` entry; OOXML is told apart by
  the central directory at the end of the file (`word/document.xml`,
  `xl/workbook.xml`, …). Today the remaining binary signatures (PDF, GIF, ZIP,
  gzip) give a label but no type, so no action is offered for them.
- Text: no magic and valid UTF-8 without NUL bytes means text; the exact
  kind (Markdown, reStructuredText, Org, …) comes from the extension, since
  content cannot tell them apart. Today that is plain text, Markdown and
  the subtitle types below; recognising HTML and JSON by content is planned.
- **Subtitles:** a UTF-8 `.srt` whose 4 KiB head has a `-->` cue arrow is
  `application/x-subrip`, label `SRT subtitles`; a `.vtt` whose head (after
  an optional byte order mark) opens with `WEBVTT` followed by a space, a
  tab, a line end or the head's end (`WEBVTTX` is not a signature) is
  `text/vtt`, label `WebVTT subtitles`; a `.ass` or `.ssa` whose head has `[Script Info]` (any
  case) is `text/x-ssa`, label `ASS subtitles`. A `.ttml` or `.dfxp` whose
  head opens with `<` after an optional byte order mark and white space is
  `application/ttml+xml`, label `TTML subtitles`; a `.xml` is one only when
  its head's root element is `tt` or `prefix:tt` with a TTML namespace
  (`http://www.w3.org/ns/ttml`, or the 2006 DFXP draft's
  `http://www.w3.org/2006/10/ttaf1`) declared on that start tag, found by
  text search past any `<?…?>`, comments and `<!DOCTYPE …>` (its internal
  subset included). The root must end within the 4 KiB head; a root past
  it is not recognised. Whether the XML is well formed is the reader's
  to say. A UTF-16 TTML has NUL bytes, and a non-UTF-8 one is not valid
  UTF-8: neither is text, so neither is offered TTML or a code page.
  The name alone never makes a subtitle: without the head check the file
  keeps its text label (`.srt text`). An SRT, WebVTT or ASS file that is not valid UTF-8 is
  `text/x-8bit`, decided first as before, and goes to the code page actions.
- **8-bit text:** `text/x-8bit`, label `8-bit text`, is a candidate for
  the code page actions and taken by no other preset. A file is one when
  its extension is on the classifier's text list (`txt`, `text`, `md`,
  `markdown`, `mdown`, `mkd`, `srt`, `vtt`, `ass`, `ssa`, `sub`, `csv`,
  `tsv`, `log`, `ini`, `cfg`, `conf`, `me`, `1st`, `readme`, `diz`; XML
  names such as `ttml` and `dfxp` are not, since a code page action keeps
  the file's XML declaration; not
  `nfo`, usually CP437 art) or it has none, what is read has no NUL and no
  C0 control byte other than TAB, LF, CR and FF (one Ctrl-Z, `0x1A`, as the
  file's very last byte is allowed), and it is not valid UTF-8. A file with
  a listed extension whose 4 KiB head is all ASCII is read on, on the same
  handle, up to its first 256 KiB, stopping at the read that finds the
  first byte that is not valid UTF-8, or a control byte (valid UTF-8
  accents do not stop it); one wheel invocation reads on at most 1 MiB
  for all its files together, after which the head alone decides
  (`text/plain` for an ASCII head). A head that is valid UTF-8 with any
  non-ASCII byte, or a file without an extension, is decided by the head.
  Every valid UTF-8 file keeps the plain text, Markdown and subtitle rules above.
  The classifier only proposes: the action checks the whole file. A file
  the head decides as plain text or Markdown (a listed text or Markdown
  extension, or none) whose first byte that is not valid UTF-8 lies past
  the window, or past a spent budget, is offered the text actions; each
  one reads all of it and skips it, naming that byte and its offset from
  0 (`not UTF-8: byte 0xE9 at 266240`); nothing is published. Valid UTF-8
  with a NUL byte is still skipped as `unsupported` by the plain-text
  reader. Any other listed extension (`.csv`...), and a `.srt`, `.vtt`,
  `.ass` or `.ssa` without its subtitle mark, has no type with such a head,
  so no shipped action takes it.
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
  slice with the reason. A preset may instead name its action
  (`Preset.action`, an action id) where two actions share its input and
  output types and only the id tells them apart (two code pages to
  UTF-8): it then runs exactly that one-to-one action, never another
  route. Not installed, a join, or not of the preset's types, it greys
  the slice out ("no action installed"); a missing tool says what to
  install, as for a route. A preset without a name finds its route as
  before.
- **Eight fixed directions.** A target always sits in the same direction,
  whatever the input; unused directions are dimmed. Muscle memory is the
  point. The actual layout is a design decision.
- At most eight slices per ring and at most one sub-level, used only when
  the presets do not fit; then the first ring holds the target groups.
- **Mixed selections** offer the presets of the most common source (ties
  broken by the fixed preset order). The other files are skipped, and the
  hub says so ("15 / 20").
- **Join presets.** A preset may name a set of MIME types (`Preset.join`);
  the `pdf` and `cbz` presets name JPEG and PNG, the `epub` preset names
  Markdown. A preset's 1:1 input is
  optional: `cbz` has none (`Preset.input` is `None`), so `choose` only
  looks at the join. A run of a preset that matches no input is named
  (the history's `action=`) after the 1:1 route it would have taken, or the
  preset's id when it has none. `menu::choose` decides what a preset
  runs for a selection: the 1:1 route (for the preset's own input) or a join
  action, which is a catalog action whose type set covers the preset's set,
  with the preset's output, runnable, and with at least its `min`
  documents in the selection. A join's count is its documents plus, beside
  at least one document, its assets (`assets=`); assets alone count
  nothing. A join without assets and the 1:1 route compete on how many
  files each takes; the larger wins and a tie goes to the 1:1 route. A
  join with assets wins whenever it takes two or more documents, or a
  document and an asset (unless the 1:1 route takes more files): two
  Markdown files, or one with its images, make one book; one Markdown file
  alone keeps its 1:1 form, and images alone offer no EPUB. The count is
  the slice's `applicable` and the hub's N. The wheel's levels, the flat
  offer and the run all ask `choose`, so they cannot disagree. A join action is
  never a step of a chain: the planner skips it.

## Executor and supervisor

- After the choice the wheel closes and releases the lock; the same
  process stays as the **supervisor** while the action runs (a new session
  with `setsid`, started through `fork` and `execve` so the sandbox and
  limits can be applied in between), then exits. It frees the wheel's buffers
  before waiting.
- **One process per target folder** of a batch (each folder gets its own
  temp dir, so the final move stays an atomic `rename`); actions declaring
  `batch=no` run per file, one after the other (parallel runs may come
  later). A **join** action runs once over every accepted input, whatever
  folders they come from and whatever its `batch=` says, in natural order
  (`natural_cmp` on the file name's bytes, then on the whole path; a join
  with assets gets its documents in that order, then its assets in that
  order; assets without an admitted document are skipped, "no document to
  join", and the action does not start), in one
  temp dir in its result's target folder (its anchor, see
  [Output policy](#output-policy)), and its one result is published once,
  as one group, by the same staging and no-replace rename as any other
  output. A two-step route runs both steps in the same temp dir; only the
  last step's outputs are placed, named after the user's input (with
  `ext=same` its extension too, never that of step 1's output). Step 1's
  outputs are checked again once its whole group is gone (a regular
  file, no symlink on the way), so an action cannot swap one for a link
  before step 2 reads it. When step 1 makes several parts of one input,
  the input is still all or nothing: a part that either step fails,
  skips or leaves unreported settles the input (failed or skipped,
  once), and none of its other parts is handed on or placed.
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
  name, without its folder (plus `Saved to Downloads (source folder is read-only)` when it went
  there). Several: `<n> done[, <m> skipped][, <k> failed]` and up to three
  problem lines `<input>: <reason>` (failures first), then `… and <r>
  more`. Nothing finished: `Nothing converted`. An input converted at an
  unknown place adds `<input>: <cause> (op <id>)` and `<n> not confirmed
  where it was saved; Undo unavailable.`, and the Undo button is left out.
  Undo is offered only when the history holds every group of the run
  complete, durable and synced; otherwise the recording warning (`history
  not written: …; nothing was published; undo unavailable` when the error
  came before any output, `history not written: …; <n> output folder(s)
  kept; undo unavailable` when <n> were published before it, `… not
  confirmed on disk; undo unavailable`) is a body line instead, and the
  same text is the CLI's stderr warning. Cancelled: `Cancelled`,
  `<n> done, <r> not converted`. An output whose lossy action reported a
  loss (a `warn` for an input that has an output) starts the body with
  `Formatting was lost.`; a run that lost nothing does not say it. A run
  of an Encoding-group action that has an output adds the body line
  `converted from <code page> to UTF-8` (the action's label, for one input
  and for several; no line for a run without output or a cancelled one);
  it comes from the action's group, not from an event. A join
  run that published its result: the result's file name, then `<taken> /
  <total> files` (the inputs in the result, of every file of the run) and
  the same problem lines, plus the Downloads line when its anchor folder
  was read-only; a join that published nothing reads like any run that
  converted nothing. A run
  the sandbox gate refused ends it with `Sandbox unavailable.`. Text is cut to 200 characters on a UTF-8
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
  A cancel (signal or Cancel click) reads as "Cancelled" only if it stopped at
  least one input; one that came after the last result is reported as the finished
  run.
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
  undoes the job and replaces the result with what happened, per input
  group: `Undone` / `Moved to Trash` when every group went to the Trash;
  `Partially undone` with the counts (`2 moved to Trash, 1 kept because it
  changed`); `Nothing undone` only when every group was kept (one not found
  where it was saved says so in its line; the CLI exits 1 for it); `Undo uncertain` when a group's place or metadata could not be
  proved (never shown as a success); `Undo incomplete` otherwise (a group
  moved and came back, or stays in the Trash). Places that need attention
  and warnings follow, each line escaped and bounded; `Undo failed` with
  the cause when the job was refused before anything moved.
- Mistakes are handled by safety and undo, not by confirmation dialogs:
  people click through confirmations, and they tax every correct choice.

## Output policy

In the engine, in one place:

1. Next to the source. If that folder is not writable, the XDG download
   folder, and the notification says so.
2. Same base name, new extension. When the format stays the same, a fixed
   suffix, English and Latin-only by default (`-small`, `-compressed`),
   configurable. The exception is an `ext=same` action (a re-encoding,
   such as a subtitle to UTF-8): its output keeps the input's name
   exactly, with no suffix (`Film.hu.srt` gives `Film.hu/Film.hu.srt`;
   only a trailing dot is dropped, `Film.` gives `Film/Film`),
   because the wrapper folder already tells the two apart and players
   pair a subtitle with its film by the name.
3. Every input gets its own folder, a single output too (`a.txt` gives
   `a/a.md`; several outputs keep their names, `doc/doc-01.png`, …).
   Never overwrite: `name (1)/`, `name (2)/`, without asking.
4. Per input, all or nothing: the outputs are copied into fresh files in a
   private 0700 staging folder on the target's filesystem (`.inacircle-
   <random>/`), checked (regular files, unchanged while copied, text
   valid UTF-8 within its cap, validated on the stream that is written:
   see the text outputs below),
   given their final modes as `cp` would (0666 and 0777 minus the umask,
   but the owner always keeps rw on files and rwx on the folder, and a
   setgid target's S_ISGID is kept) while still inside the private folder,
   synced, and their identity recorded; then the folder is published by one
   `renameat2` `RENAME_NOREPLACE`. A filesystem without it is refused, with
   no fallback. Before that commit an error (or a cancel) removes only the
   engine's own staging; after it nothing is removed: a failed sync is
   reported as unconfirmed, and a result that cannot be proved right after
   the commit is reported as converted at an unknown place or state, with
   its operation id and anchor, never with a guessed path: either an
   ancestor was renamed meanwhile (`AncestorChanged`) or the folder's files
   changed since the baseline (`BaselineChanged`, "files changed while
   saved"). Other inputs' published results are never taken back.
   Text outputs: an action whose `out=` is `text/*` or `application/x-subrip`
   (the subtitle format, the one `application/*` type that is text) has every
   output named with its extension checked as UTF-8 on the whole stream,
   whatever that extension is (`.srt`, `.html`, none with `ext=same`),
   and so its single output always; its other outputs (a Markdown
   file's images) stay bytes. An output named `.txt` or `.md` is checked
   for any action, as before. A text output may be 32 MiB (the text
   output budget), except an 8-bit text decoder's (`in=text/x-8bit`):
   three times the bytes the engine handed it, plus 3 for a byte order
   mark, taken from the input's size when the action starts, never from
   anything the action reports (on a two-step route, the size of the
   intermediate file that made the output). Past its cap, or not UTF-8,
   the input fails ("text output is not UTF-8", "text output larger than
   ...") and nothing of it is published. An 8-bit text decoder makes one
   UTF-8 copy of one file, so it may publish exactly one output per
   input: a second one fails the input ("8-bit text action made more
   than one output") and nothing of it is published. The engine
   publishes text only as UTF-8: the contract has no 8-bit output type.
   `text/x-8bit` is an input type the classifier gives; an action that
   declares it as its `out=` (an encoder from UTF-8 to a code page) is
   checked like any `text/*`, so every output byte past ASCII fails its
   input as not UTF-8.
5. Extraction: the extracted files go, flattened, into one folder named
   after the archive; a single top-level folder inside the archive is not
   kept as such. (Every input gets its wrapper folder, also a single
   output.)
6. The temp dir is `.inacircle-<random>` inside the target folder, always
   removed at the end through the folder's fd (never by a path that may
   name something else by then); stale ones from a crash are cleaned by
   the next run: our pattern, owned by this user, older than a day, mode
   0700 and holding only our own names (`w`, `step<k>`, `step<k>.inputs`,
   at most four). A published group renamed to that pattern holds its
   outputs' names and is never removed. An output folder never takes that
   pattern.
7. A join action's one result has one place and one name, its anchor:
   the target (rule 1) of the folder that holds the most of its accepted
   documents (the inputs of its `in=` set; assets have no say), on a tie
   the folder of the document that comes first in its order. Its name is
   that folder's name for two or more documents
   (`Nyaralás/Nyaralás/Nyaralás.pdf`), the document's stem for one, however
   many assets come with it. A name that is missing or hidden (the root,
   a folder or stem starting with a dot, which would hide the result)
   gives the type's English name: `book` for an EPUB join, `images` for
   the image joins (PDF, CBZ). The wrapper and a taken name
   (`Nyaralás (1)/`) follow rule 3; inputs from other folders change
   nothing. It goes to the download folder only when the anchor folder is
   not writable.
8. Undo never deletes. It moves each input's output group (the wrapper
   folder, whole) into the freedesktop.org Trash with one
   `renameat2(RENAME_NOREPLACE)`: the home Trash
   (`$XDG_DATA_HOME/Trash`) when it is on the same mount and device, else
   the volume's (`$topdir/.Trash/$uid` under a sticky `.Trash` that root or this user owns, else
   `$topdir/.Trash-$uid`); every Trash folder (the root, `files`,
   `info`) is opened without following a symlink and must be a directory
   of this user. A folder we make is 0700. An existing home Trash folder
   is accepted when neither group nor others can write it and it has no
   setuid/setgid bit (KDE's KIO makes it 0755); a volume Trash folder must
   be exactly 0700. A home Trash accepted at 0755 can be listed (not
   written) by others if `$HOME` itself is traversable by them; the code
   does not check `$HOME`'s mode. Recovery re-checks `info` under the same rule. A
   folder failing its rule is refused, never `chmod`ed, and the warning
   names it and why; a remote filesystem or no safe Trash keeps the group.
   Nothing is copied across filesystems (`EXDEV` keeps it). Before the
   move the wrapper and every child must match their private baseline
   (device, inode, size, mtime, ctime, type, owner, mode, link count; the
   exact child count): one changed, missing or extra child keeps the whole
   group; other groups are not affected. The `.trashinfo` (`Path=`
   percent-encoded, `DeletionDate=` local time) is created with `O_EXCL`,
   read back and synced before the move, after a synced recovery intent.
   After the move the group and its info are checked again; on any
   difference the group is moved back (no-replace); if its way back is
   taken it stays in the Trash and its place is reported. Our info is
   authentic only while it names our moved group: otherwise it is
   neutralised through its fd and renamed off its `.trashinfo` name (even
   when the neutralisation failed). A place is reported only when proved
   at that moment: the known path, or the path resolved from the held fd
   (at most 4 tries, no symlink); otherwise it is unknown, with an anchor,
   and nothing is deleted. Later renames by other programs are not
   followed. A job with an output at an unknown place, not confirmed on
   disk, or with an incomplete or damaged record is refused before
   anything moves, and so is a job of the previous history format. We
   never empty or scan the Trash: the user manages it in the file
   manager.

## The wheel

`inacircle` without arguments. One process per hotkey press, gone when the
choice has run.

**Life of a run**

1. SIGALRM is reset to its default action and unblocked (an ignored
   disposition survives `execve`), then `alarm(5)` is armed, before the
   lock: a slow runtime dir is covered too. The 5 s are the startup's
   backstop: 2 s of startup plus 1 s of close is 3 s on the orderly path,
   and 2 s remain for scheduling, so it fires only where no deadline
   reaches. At the wheel's first commit the alarm becomes the user's stage,
   30 s (below); when the close begins it is replaced by a 2 s one (the
   close waits at most 1 s for the compositor), so a choice made in the
   last second of a stage is not cut off by the stage's own alarm. The
   default action kills the process, so this is the last guard if
   everything else hangs; the kernel then releases the lock and the
   connection.
2. One instance: `flock` on `$XDG_RUNTIME_DIR/inacircle/wheel.lock`, opened
   `O_NONBLOCK | O_NOFOLLOW` (a symlink or FIFO there is an error, exit 1).
   A held lock makes a second instance exit 0 at once, before touching
   Wayland.
3. The surface is shown (below). Its start has its own clock, 2 s from
   the connection to the wheel's first commit (`ui_startup_deadline`), apart
   from the user's wait. The wheel has two levels: categories, then the
   chosen category's targets. Each user wait (the drop, the category choice
   after a fallback drop, the target choice) has 30 s, counted from the
   commit; the drop and the category choice re-arm `alarm` (and the
   surface's own deadline, 500 ms shorter so a timeout normally ends in an
   orderly close). The deadline will become configurable.
4. Closed without a choice (Esc, a click off the wheel, the drag leaving
   without a drop, the deadline, the compositor closing the surface):
   surface torn down, lock released, exit 0. Nothing runs and nothing is
   notified. The teardown is ordered: the layer surface (the keyboard grab)
   and the surfaces are destroyed, the compositor's answer to a `sync` is
   awaited for at most 1 s (`ui_teardown_deadline`), the connection is
   closed, and only then is the lock released.
   A start that fails (no configure within 2 s, no position and a seat
   without a pointer, a seat that has no keyboard by then, a Wayland error before the
   first commit, a setup
   error) takes the same ordered close, then exit 1 and one desktop
   notification, "Could not open the circle." with the proven cause (an
   unproven one gets the general copy), through the bounded notification
   path (the open within 500 ms, the call within 500 ms, a 2 s alarm as the
   last guard). No retry and no second window; without a bus it is one
   line on stderr.
   The wheel is not shown before the seat has a keyboard (Esc could not
   close it); a keyboard that comes late, within the 2 s, is waited for,
   and one that never comes is the named cause at the deadline. A keyboard
   lost after the wheel is up closes it in order (exit 0, no notification).
   An error after the first commit is only logged (exit 1): the wheel did
   open, so it is never announced as "Could not open".
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
  `Document`, `book` = east, labelled `Book`, `encoding` = west, labelled
  `Encoding`, and `subtitles` = south, labelled `Subtitles`; an unknown
  `group` is not shown, one stderr line per run). A category is **lit** when any dragged
  file has a target in it, dim when it exists but nothing fits, and a direction
  without a category is empty. The hub shows the source (name or count).
  Level 2 shows the category's presets in their own directions (`markdown`
  (from plain text) and `markdown-epub` (from an EPUB) north, `plain-text`
  (from Markdown) and `plain-text-epub` (from an EPUB) south, so an EPUB
  lights only Document (Markdown north, Plain text south, HTML east dim)
  and nothing in Book; a file that is not valid UTF-8 text (`text/x-8bit`)
  lights only Encoding, whose wheel has `cp1250` (Windows-1250) west and
  `iso88592` (ISO-8859-2) east, each naming its action, and a UTF-8 text
  leaves Encoding dim; a subtitle lights only Subtitles (south): `vtt-srt`
  (VTT from SRT), `vtt-ass` (VTT from ASS) and `vtt-ttml` (VTT from TTML)
  share its north slot, `srt-vtt` (SRT from VTT), `srt-ass` (SRT from ASS)
  and `srt-ttml` (SRT from TTML) its south slot, so an SRT offers VTT north
  only, a VTT SRT south only, and an ASS or a TTML both; `pdf` north
  and `cbz` south in Book; `cbz` has no
  one-to-one form, it only joins images into one comic book; `html`
  and `html-txt` share Document's east slot, `epub` and `epub-txt` Book's:
  a direction is unique per group and input type, and when the dropped
  files fit both inputs, a preset that can run beats a dim one, then the one
  that takes more of the files holds the slot, then the earlier preset in
  the table, plain text before Markdown, which is also the flat offer's tie
  order; so one EPUB and one Markdown file leave `plain-text` (from Markdown)
  in the south slot, and more EPUBs than Markdown files give it to
  `plain-text-epub`; the same holds in the north slot between `markdown`
  (from plain text) and `markdown-epub`): lit if
  applicable, dim otherwise; the hub is the category's label, or
  `<n> / <total> files` when the most files any lit target of the category
  takes (`n`, the count the run would take) is below the number dropped.
  `inacircle offer --groups <files…>` prints level 1. The flat
  `inacircle offer <files…>` prints the presets of the most common input type,
  each `slice` line with its `group=`, since a direction repeats across groups.
- The **early read**: at `wl_data_device.enter`, if the offer's MIME list
  has `text/uri-list` (offers arrive before `enter`), the wheel `receive`s
  it into a pipe at once, read in the `poll` loop (500 ms, 1 MiB, 1024
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
  a 500 ms deadline, at most 1 MiB (the action invocation's path-list
  limit) and 1024 URIs (its input count). A list over either shows `too many
  files` in the hub and offers nothing (no truncated selection), early and
  after the drop. A source that never
  writes or never closes times out. A failed early read falls back; a
  failed read after the drop closes the wheel. The list is decoded from
  its end and its bytes are given back as they become paths, so the
  wheel never holds a whole list and all of its paths at once (see
  `docs/resource-limits.md`).
- Only `file://` URIs with an empty or `localhost` host count. `%XX`
  decodes to bytes; a non-`file` URI, another host, a bad escape or a
  `%00` drops that entry alone. A raw `?query` or `#fragment` is cut off.
  Paths are made what the command line makes them (`.` components and
  trailing slashes dropped; `..` and non-UTF-8 bytes stay), so the wheel
  classifies the same files the run it starts is given. CRLF and LF both
  end a line; lines starting with `#` are comments. If nothing is left,
  the hub says `Nothing to do`.
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
atlas exists. On level 2 the hub keeps the category's label when one file
was dropped or the category's lit targets take every dropped file;
otherwise it shows `<n> / <total> files` as above. When no category is
lit on level 1, the hub says `no action for <type label>` (the first file's
label, e.g. `no action for .cbr file`) if the hub can draw all of it
(108 px wide, every glyph in the atlas), else `no action`: with the 15 px
font `no action for ` alone takes about 85 px, so in practice the short
form shows and `inacircle offer` prints the long one.

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
  (header `inacircle-history 1`), manifest-style lines. A job's first
  record is `job-start`, written when it opens the history: its id and
  its owner (the supervisor's pid, start time in ticks since boot, boot
  id and pid namespace; without them when `/proc` cannot tell). Every
  block is appended after a newline check under the append lock: a last
  line another run left cut off gets its newline first, so it spoils only
  its own record.
  Before an output group's publishing rename, its whole manifest (`group-begin`
  with the wrapper's identity and the child count, one `child` per output
  with its private baseline, `group-end` with the count and the names'
  bytes) and a `publish-intent` (planned place) are appended as one block
  and synced; if that fails, the group is not published. A name taken at
  the rename adds another intent for the same group and operation. After
  the rename comes `publish-commit` (the proved place, durability) or
  `publish-uncertain` (the anchor, when the place could not be proved),
  synced; a failure there keeps the output and its success, and the job
  is then not undoable. A publication that fails after its intent gets
  `publish-abort`. `skip`, `fail` and `unknown` lines come as they
  happen, the `job` line (id, time, action, status, sandbox, `undo=no`
  when not undoable) at the end. Undo writes, per group, a synced
  `recovery-intent` (source, Trash target, identities, mount, deletion
  date) before anything is made in the Trash, and one synced ending:
  `recovery-commit` (in the Trash), `recovery-restored`,
  `recovery-kept`, `recovery-uncertain` or `recovery-abort` (nothing
  moved), each with the proved place or `-`, the anchor and the reason;
  then `undo` (unless every group was kept: nothing moved, so no `undo`
  record; the CLI exits 0 when each was kept at a place its line names, 1
  for `kept -`). An undo cut by a crash is resumed by the next one from
  these records: an intent without its ending is recovered by looking only
  at its target and our two info names (no Trash scan). Paths are
  byte-exact (`%XX` for bytes that are not printable ASCII, `%` and
  `\`). There is no digest: undo acts only on a job whose every group is
  complete (version, unique group and operation, begin and end, matching
  counts and bytes, a commit at a recorded intent's place, synced); a
  duplicate, missing, cut-off or unknown record refuses the whole job
  before anything is touched. A version 0 history is archived
  (`history.v0`, `history.v0.1`, …, never overwritten) under the
  history's lock by the first run that writes; its jobs are refused with
  the reason, their outputs kept. A run that dies midway leaves its
  complete groups, which undo still finds. A join run is one group like
  any other (its result's wrapper); the inputs it did not take have their
  usual `skip`/`fail` lines, and which inputs went into the result is not
  recorded (undo does not need it): undo moves that one wrapper to the
  Trash, the inputs are never touched. The format is the same as before
  join actions, and older jobs still undo. Nothing of a run is kept in
  memory per outcome;
- state files (history, registry entries, the catalog cache) are created
  exclusively under random names, never through a symlink, in a folder
  opened once and checked (a directory of this user, not writable by
  others: tightened if it is), and renamed or linked into place relative
  to that folder. The folder itself (`$XDG_STATE_HOME/inacircle`,
  `$XDG_RUNTIME_DIR/inacircle/jobs`, the catalog cache folder) must not be
  a symlink: that is refused on purpose (the folder is opened with
  `O_NOFOLLOW | O_DIRECTORY`; the operating system's bare `Not a
  directory (os error 20)` is replaced, after an `lstat` confirms a real
  symlink, by "the folder `<path>` is a symbolic link and is refused on
  purpose", with the same error kind, also for a dangling link; the
  history warning reads `history not written: …; nothing was published`,
  so nothing is published and nothing is offered for Undo; a cache folder that is a link means no
  cache is read or written, and a jobs folder that is a link means the
  running job is not registered; both print one line on stderr saying
  why). Its parents may be symlinks, and so may
  `user-dirs.dirs`, which is the user's own configuration. To keep a
  state folder elsewhere, make its parent the link or bind-mount it;
- running jobs: each supervisor registers itself in
  `$XDG_RUNTIME_DIR/inacircle/jobs/<job-id>`, one `job` line (id, pid,
  action, inputs, done, progress, start time) rewritten by rename at most
  four times a second and removed at the end; a job id is
  `<unix-seconds>-<pid>-<8 random hex digits>`, and an entry whose
  process is gone is ignored. A listing examines at most 128 entries and
  reads each entry bounded (4096 bytes, 16 fields); past a limit, or when
  the folder cannot be read, it is an error, never a shorter or empty list,
  and `undo`, `jobs` and `cancel` fail on it (undo moves nothing), as
  they do on an entry that cannot be opened (anything but one that is gone
  or is not a regular file of this user). The dead entries among those
  examined, and our own temporary files untouched for a minute, are
  removed first (each only while its name is still the file just opened),
  so a pile left by crashes heals. An entry records its process by pid,
  start time and boot id; one written by an older build has no identity
  and is judged by the pid alone, so after pid reuse it looks alive until
  removed by hand (delete the file in `$XDG_RUNTIME_DIR/inacircle/jobs/`;
  that folder is tmpfs and empties at the next boot). Where no file
  descriptor of an object is held across the check and the removal (a
  registry entry, a step's work folder), `(device, inode)` cannot prove
  identity against inode reuse. Fooling that takes a process of the same
  user that removes our private 0700 object and recreates the same random
  name on the freed inode; the worst result is one of our own bookkeeping
  files or an unpublished work folder removed, never a published output;
- the history is read in a window: the header by itself (256 bytes), then
  at most the last 8 MiB through a 1 KiB buffer, lines of at most 32 KiB
  with at most 32 fields, a job with at most 1024 outputs and groups and
  4096 publish intents, paths of at most 4096 bytes and 1 MiB in all. A job that is not provably inside
  the window (its `job-start` has to be there, before any other line of
  it; the append lock orders the records, no clock is trusted), or over a
  limit, is refused before anything moves;
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
  finished (`cancelled`), then the same rule applies. A join run exits 0
  once its result is published (each input in it gets its `done` line,
  with the one result's path); otherwise the same rule (1 if an input
  failed, 3 if every one was skipped).
- The same file (by canonical path) given twice counts once. A non-UTF-8
  input path is skipped ("non-UTF-8 path").
- `undo` of a job that is still running is refused (exit 1): cancel it
  first. The registry is not the only check (it may be missing, without
  `$XDG_RUNTIME_DIR`, or unwritten after an error): a job whose history
  has no closing `job` line is undone only once the owner its `job-start`
  names is provably gone (no process at the pid, or another start time or
  boot). An owner that cannot be checked refuses the undo: unknown means
  nothing moves. That is a job recorded without an owner (a record from
  before owners, or `/proc` could not name the process when the job
  started) and a job whose owner is in another pid namespace. Such a job
  stays refused after a crash too: only a job with an owner recorded
  here is undoable once its process is gone, and a crash recorded before
  owners existed is not.
  Undo prints one line per output group: `moved <Trash path>`,
  `kept <path|-> <reason>`, `restored <path|-> <reason>`,
  `recovery-kept <path|-> <reason> <errno|->` or
  `uncertain <op> <reason> <last path seen|->` (a hint, not a proved
  place), then `warning <text>` lines; exit 0 when every group went to the
  Trash or was kept at a place its line names, else 1.
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

**Planned, not built yet.** Today the built-in defaults always apply.

- `$XDG_CONFIG_HOME/inacircle/config.toml`, full **TOML 1.0** with a parser
  of our own (shared with the data family, tested against `toml-test`).
  Without a file, built-in defaults apply.
- No reload watcher is needed: every hotkey press reads the file afresh.
- Configurable: presets (a type's list replaces the built-in one as a
  whole), suffixes, notifications, look (colours, opacity, sizes). Fonts
  and font sizes are limited to the variants baked into the atlas.

## Repository layout

```
crates/   wire, dbus, ui, engine, manifest, action, doc, epub, flate,
          zip, probe, pdf, xml (planned: toml)
          (wire: the Wayland client, libc only; dbus: the notifications
          client, libc only; ui: the wheel's geometry, rasterizer, damage
          and text; flate, zip, probe, pdf: the CBZ-to-PDF pipeline of
          `inacircle-pages`, zip also writes archives with stored
          entries only; epub: the EPUB packer and reader of `inacircle-text`; xml:
          the non-validating XML reader; the shipped `inacircle` links
          wire, dbus, ui, engine, manifest and action, never `doc`,
          `epub`, `flate`, `zip`, `probe`, `pdf` or `xml`;
          `scripts/check-shipped-deps.sh` enforces it)
bins/     inacircle, inacircle-text, inacircle-pages,
          inacircle-testaction (test-only)
tools/    gen-entities, bake-atlas, testzip (dev tools; bake-atlas alone
          uses `fontdue`, testzip alone uses `miniz_oxide` to build test
          archives; both are kept out of the shipped binaries)
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
