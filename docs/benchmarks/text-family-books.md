# Text family: EPUB and HTML

What `inacircle-text` costs when it writes a book (`text.md-to-epub`,
`text.txt-to-epub`) or a single page (`text.md-to-html`,
`text.txt-to-html`): memory on the largest accepted input, binary size,
and the budgeted drop-to-stage-2 path. Gates and rules:
[acceptance.md](acceptance.md). The action contract is in
[`architecture.md`](../architecture.md), the limits in
[`resource-limits.md`](../resource-limits.md).

## Method

- **Memory.** The release `inacircle-text` runs `text.md-to-epub` on the
  largest input the family accepts and the peak RSS of the child
  (`ru_maxrss`) is read after it exits. The input is a generated Markdown
  file just under the 8 MiB input limit with many H1 chapters and one very
  large table (the shape that holds the most structure at once). Actions
  are not held to the 5 MB wheel budget; the figure is checked against the
  sandbox's `RLIMIT_DATA` net (RAM/8, at least 1 GiB).
- **Size.** `scripts/check-sizes.sh` (stripped release builds): file size
  and `.text`. Budgets: `inacircle` file <= 1.5 MB, a family binary
  <= 1 MB of `.text`.
- **Drop to stage 2.** Our CPU from the dropped data to the stage-2
  wheel's commit (`INACIRCLE_MEASURE=1`), p95 by nearest rank over at
  least 50 observations, as for the other families (see
  [many-to-one.md](many-to-one.md#drop-to-stage-2)). The menu change of
  this family (four presets that share slots per group) is on this path.

## Machine

The reference desktop (see the [README](README.md#reference-machine)):
Ryzen 7 5700X, 96 GB, CachyOS, kernel 7.2.9, release build with the
repository profile (fat LTO, `panic=abort`, strip).

## Numbers

| Input (Markdown to EPUB) | Chapters | Table rows | Peak RSS |
|---|---|---|---|
| 8,387,584 B (limit 8,388,608) | 879 | 50,000 | 107,712 KiB |
| 8,232,707 B | 1,000 | 40,000 | 92,076 KiB |
| 8,232,707 B | 1,000 | 10,000 | 46,276 KiB |

The 107,712 KiB run produced an 11 MB book. In these runs the peak grows with the
number of table rows (46,276 KiB at 10,000 rows, 92,076 KiB at 40,000);
the runs do not isolate the chapter count. A first attempt with many inline nodes hit the 524,288-node
limit and failed as designed (`too many text elements`), so the table
shape above is the largest that is accepted.

| Binary | Arch | File (B) | `.text` (B) |
|---|---|---|---|
| `inacircle` | x86_64 | 1,166,392 (+6,368) | 896,760 (+4,880) |
| `inacircle-text` | x86_64 | 859,968 (+103,096) | 538,472 (+87,606) |
| `inacircle-pages` | x86_64 | 504,208 | 403,763 (+496) |

`inacircle-text` grew by more than 100 KB: the XHTML writer is about
39 KB, the EPUB packer about 20 KB, the ZIP writer about 3 KB, the book and
action code about 30 KB, and the rest is `std` formatting, hashing and
allocation. Its `.text` stays under the 1 MB budget. `inacircle` links
none of the `doc`, `epub` or `zip` crates
(`scripts/check-shipped-deps.sh`). The aarch64 baseline was not refreshed.

### Drop to stage 2

| Machine | Series | p95 |
|---|---|---|
| Reference desktop | text drop to stage-2 commit | pending (manual run) |
| ThinkCentre | text drop to stage-2 commit | pending (manual run) |

## Verdict

Memory: the largest accepted input peaks at about 105 MiB of RSS, far
below the smallest `RLIMIT_DATA` net (1 GiB), so the limits and the net
agree. Size: inside the budgets, with the growth explained. The drop to
stage 2 is open until the two series above are recorded.

## Reproduce

- Memory: build with `cargo build --release -p inacircle-text`, generate a
  Markdown file of the shape above (about 879 `# Heading` chapters, one
  table of 50,000 rows, total just under 8,388,608 B), then run
  `inacircle-text run text.md-to-epub --work <dir> -- <file>` and read the
  child's maximum resident set size (for example `/usr/bin/time -v`).
- Size: `scripts/check-sizes.sh`.
- Drop to stage 2: the procedure of
  [many-to-one.md](many-to-one.md#drop-to-stage-2), with a Markdown file
  and a plain-text file dropped on the wheel.
