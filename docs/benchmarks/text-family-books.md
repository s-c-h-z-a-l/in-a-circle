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
| not recorded | 1,000 | 40,000 | 92,076 KiB |
| 8,232,707 B | 1,000 | 10,000 | 46,276 KiB |

The 107,712 KiB run produced an 11 MB book. In these runs the peak grows with the
number of table rows (46,276 KiB at 10,000 rows, 92,076 KiB at 40,000);
the runs do not isolate the chapter count. A first attempt with many inline nodes hit the 524,288-node
limit and failed as designed (`too many text elements`), so the table
shape above is the largest that is accepted.

| Binary | Arch | File (B) | `.text` (B) |
|---|---|---|---|
| `inacircle` | x86_64 | 1,166,392 (+6,368) | 896,760 (+4,880) |
| `inacircle-text` | x86_64 | 861,520 (+104,648) | 539,672 (+88,806) |
| `inacircle-pages` | x86_64 | 504,208 | 403,763 (+496) |

`inacircle-text` grew by more than 100 KB (the closing fixes of the
link URLs and the page write account for 1,552 B of it; the itemised
parts below are from the build before them): the XHTML writer is about
39 KB, the EPUB packer about 20 KB, the ZIP writer about 3 KB, the book and
action code about 30 KB, and the rest is `std` formatting, hashing and
allocation. Its `.text` stays under the 1 MB budget. `inacircle` links
none of the `doc`, `epub` or `zip` crates
(`scripts/check-shipped-deps.sh`). The aarch64 baseline was not refreshed.

### Drop to stage 2

| Machine | Series | p95 |
|---|---|---|
| Reference desktop | `main` (plan 12, feccfcb0), Markdown to Document, N=13 | p50 157, p95 190, max 190 us |
| Reference desktop | this branch, Markdown to Document, N=12 | p50 158, p95 178, max 178 us |
| ThinkCentre | Markdown to Document | not measured (not a gate) |

Hand-made drags on the reference desktop, `INACIRCLE_MEASURE=1`, the two
builds alternating, one log per series. The small-sample rule of
[acceptance.md](acceptance.md) applies: the largest p95 (190 us) is 19% of
the 1,000 us budget (limit 25%), the standard deviations are 13 us (8% of
the p50) and 11 us (7%), and the branch's p95 is 6.3% below the baseline's,
which comes from a single 190 us sample (the means are both 158 us). There
is no regression. Eleven drags of a `.epub` on the baseline build, which
has no action for one, are in its log as failed attempts, not valid
observations.

### Drop to stage 2, Markdown joins

Many Markdown files to one EPUB, and one Markdown file with its images to
one EPUB (the Book group), on the reference desktop: hand-made drags,
`INACIRCLE_MEASURE=1`, drop on the Book slice, Esc on the second wheel.
`main` (plan 13, 0438fc78) is the baseline where it offers the same slice;
for the Markdown plus 31 images it offers no EPUB, so there is no baseline.

| Series | Valid | p50 | p95 | max | sd |
|---|---|---|---|---|---|
| `main`, 30 Markdown files | 23 | 156 us | 164 us | 173 us | 6 us |
| this branch, 30 Markdown files | 25 | 153 us | 164 us | 164 us | 6 us |
| this branch, 1 Markdown + 30 JPEG | 23 | 158 us | 169 us | 169 us | 6 us |

The p95 is at most 17% of the 1,000 us budget and the spread 4% of the p50,
so the small-sample rule of [acceptance.md](acceptance.md) applies and the
series are well above its 10 observations. The two 30-file series have the
same p95, so there is no regression. The second machine was not measured.
By hand: the 30 files made one EPUB of 30 chapters and the Markdown with
its images made one of 3 documents and 30 images, both with 0 errors and
0 warnings from `epubcheck`.

### Drop to stage 2, EPUB and 8-bit text

An EPUB on the Document slice and a Windows-1250 `.srt` on the Encoding
slice, on the reference desktop: hand-made drags, `INACIRCLE_MEASURE=1`,
Esc on the second wheel. `main` at the end of plan 14 offers nothing for
either, so there is no baseline; the figures are checked against the
budget and the small-sample rule of [acceptance.md](acceptance.md).

| Series | Valid | p50 | p95 | max | sd |
|---|---|---|---|---|---|
| `.epub` (1.2 MB) to Document | 10 | 162 us | 165 us | 165 us | 3 us |
| `.srt`, first accent at the start | 13 | 166 us | 174 us | 174 us | 3 us |
| `.srt`, first accent after 4 KiB | 15 | 166 us | 176 us | 176 us | 4 us |

The largest p95 is 18% of the 1,000 us budget (limit 25%) and the spread
at most 2% of the p50. The second machine was not measured.

By hand: an EPUB 3 novel became plain text with its title and author
header, six chapters in order and the three warnings (links flattened,
images replaced by alt text, styles dropped); an EPUB 2 novel became
Markdown with its three images beside it, and that Markdown with the
images became an EPUB again (ten documents, three images, `epubcheck`
clean); both subtitles came out as UTF-8 with a byte order mark and
correct accents. A UTF-8 subtitle gets no Encoding slice. An EPUB 2 to
plain text run was not made by hand; the suite covers it.

## Verdict

Memory: the largest accepted input peaks at about 105 MiB of RSS, far
below the smallest `RLIMIT_DATA` net (1 GiB), so the limits and the net
agree. Size: inside the budgets, with the growth explained. The drop to
stage 2 holds its budget on the reference desktop (p95 165-190 us against
1,000 us across the Markdown, EPUB and
subtitle drops); the second machine was not measured.

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
