# Text family: reading an EPUB

What `text.epub-to-txt` costs on the largest accepted book, on a dense
chapter, and on hostile shapes: peak memory and time per byte. Gates and
rules: [acceptance.md](acceptance.md). Limits are in
[`resource-limits.md`](../resource-limits.md); the contract is in
[`architecture.md`](../architecture.md).

## Method

- **Memory.** The release `inacircle-text` runs
  `text.epub-to-txt --work <dir> -- <book>` on a generated book (stored ZIP,
  one chapter file per spine item). The peak RSS of the child (`ru_maxrss`
  from `wait4`) and the wall time are read after it exits. 7 runs per
  shape; median and maximum. Actions are not held to the 5 MB wheel
  budget; the figure is checked against the sandbox's `RLIMIT_DATA` net
  (RAM/8, at least 1 GiB).
- **Shapes.** (a) the largest accepted book: 10,000 chapters of about
  6.6 KB, 66,590,000 B of XHTML in all (limit 64 MiB = 67,108,864 B);
  (b) the densest chapter that still passes: 240,000 `<p>a</p>` inside 30
  nested `<blockquote>`; (c) 300,000 such paragraphs, past the node limit;
  (d) three chapters of exactly 8 MiB (the per-chapter limit) of plain
  paragraphs, 100 words each, 25,165,824 B; (e) one chapter of 262,000
  `<b>x</b>` in one paragraph; (f) 200,000 `<p>a</p>` inside 250 nested
  `<blockquote>`, whose text grows past the output budget; (g) the three
  chapters of (d) followed by the chapter of (b), which gets only the
  output budget the first three left.
- **Time per byte.** The same harness on single chapters of hostile shape
  (the shapes the unit tests check for linearity with step counts), wall
  time of the whole run divided by the chapter bytes.

## Machine

The reference desktop (see the [README](README.md#reference-machine)):
Ryzen 7 5700X, 96 GB, CachyOS, kernel 7.2.9, release build with the
repository profile (fat LTO, `panic=abort`, strip).

## Numbers

| Shape | XHTML (B) | Peak RSS median / max (KiB) | Wall median (s) | Result |
|---|---|---|---|---|
| (a) 10,000 chapters | 66,590,000 | 23,692 / 23,692 | 0.779 | book read |
| (b) 240k `<p>` in 30 blockquotes | 1,920,881 | 179,716 / 180,016 | 0.441 | book read |
| (c) 300k `<p>` in 30 blockquotes | 2,400,881 | 118,008 / 118,020 | 0.089 | chapter refused, no chapter could be read |
| (d) three 8 MiB chapters | 25,165,824 | 33,212 / 33,364 | 0.133 | book read |
| (e) 262k `<b>` | 2,096,138 | 125,872 / 126,000 | 0.093 | book read |
| (f) 200k `<p>` in 250 blockquotes | 1,606,381 | 177,788 / 177,972 | 0.518 | `book is too large` |
| (g) (d) then (b) | 27,086,705 | 137,576 / 137,656 | 0.281 | `book is too large` |

Time per byte on hostile single chapters (7 runs, median):

| Chapter shape | Bytes | ns per byte | Peak RSS (KiB) |
|---|---|---|---|
| a `<span>` around every word | 4,194,431 | 10.5 | 28,816 |
| 150,000 attributes on one element | 1,689,029 | 12.3 | 34,188 |
| 100,000 namespace declarations | 2,577,919 | 19.2 | 39,432 |
| 1,200,000 entity references | 7,200,138 | 12.7 | 46,320 |
| 200,000 nested `<rp>` | 2,200,144 | 19.0 | 46,320 |
| 4 MiB namespace name | 4,194,454 | 3.4 | 46,320 |
| 255 nested `<div>` (refused at the depth limit, tiny input) | 2,937 | not meaningful | 34,188 |

The dense chapter (b) is the memory worst case: many small nodes cost far
more per input byte than text does, which is why the node limit exists next
to the byte limit. All runs are 7 samples, so these are preliminary
diagnosis, not a 50-sample acceptance.

## Verdict

Memory: the worst measured peak is about 176 MiB (the dense chapter), far
below the smallest `RLIMIT_DATA` net (1 GiB). The largest book stays near 23
MiB because only one chapter is held at a time. Time: every hostile shape
runs at 3-20 ns per byte with no sign of super-linear growth. Preliminary
until 50 samples per shape are taken.

## Reproduce

Build with `cargo build --release -p inacircle-text`, generate stored-ZIP
EPUBs of the shapes above (container, a package document listing every
chapter in the spine, one XHTML file per chapter), run
`inacircle-text run text.epub-to-txt --work <dir> -- <book>` and read the
child's maximum resident set size.
