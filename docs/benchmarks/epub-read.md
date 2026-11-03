# Text family: reading an EPUB

What `text.epub-to-txt` costs on the largest accepted book, on a dense
chapter, and on hostile shapes: peak memory and time per byte. Gates and
rules: [acceptance.md](acceptance.md). Limits are in
[`resource-limits.md`](../resource-limits.md); the contract is in
[`architecture.md`](../architecture.md).

## Method

- **Memory.** The release `inacircle-text` runs
  `text.epub-to-txt --work <dir> -- <book>` on a generated book (stored ZIP,
  one chapter file per spine item). Each run is its own process, forked
  and executed by a small C parent that reads its peak RSS (`ru_maxrss`
  from `wait4`, that process only); the wall time is taken around it. A
  Python parent is not used: its own high-water mark carries over the
  `exec` and adds about 9 MB (a trivial one-chapter book peaks at
  3,084 KiB this way, 12,088 KiB from Python). 7 runs per shape; median
  and maximum. Actions are not held to the 5 MB wheel
  budget; the figure is checked against the sandbox's `RLIMIT_DATA` net
  (RAM/8, at least 1 GiB).
- **Book.** `mimetype`, a `container.xml` naming `OEBPS/c.opf`, an EPUB 3
  package with a title, one manifest item and one itemref per chapter
  (`OEBPS/cN.xhtml`). Every chapter is
  `<?xml version="1.0" encoding="UTF-8"?>`, a newline,
  `<html xmlns="http://www.w3.org/1999/xhtml"><head><title>t</title></head><body>`,
  the body below, `</body></html>`; "padded to N B" means spaces before
  `</body>`.
- **Shapes.** (a) the largest accepted book: 10,000 identical chapters of
  6,659 B, each as many paragraphs of 20 `<span class="w">lorem</span>`
  (space separated, `<p>…</p>` and a newline) as fit, padded, 66,590,000 B
  of XHTML in all (limit 64 MiB = 67,108,864 B; plain-text chapters of
  that size would pass the 32 MiB output budget); (b) the densest chapter
  that still passes: `<blockquote>` 30 times, 240,000 `<p>a</p>`,
  `</blockquote>` 30 times; (c) the same with 300,000 paragraphs, past the
  node limit; (d) three chapters of exactly 8 MiB (the per-chapter limit),
  each as many paragraphs of 99 `lorem` and one `ipsum.` (space separated,
  `<p>…</p>` and a newline) as fit, padded, 25,165,824 B; (e) one chapter
  of `<p>`, 262,000 `<b>x</b>`, `</p>`; (f) as (b) with 250 blockquotes and
  200,000 paragraphs, whose text grows past the output budget; (g) the
  three chapters of (d) followed by the chapter of (b), which gets only
  the output budget the first three left.
- **Time per byte.** The same harness on single chapters of hostile shape
  (the shapes the unit tests check for linearity with step counts), wall
  time of the whole run divided by the chapter bytes. Each body below is
  padded to the byte count in the table: one `<p>` of
  `<span class="w">word</span>` and a space as many times as fit; `<p`
  with 150,000 attributes ` aN="x"` (N = 0, 1, …), then `>x</p>`; the
  same with 100,000 declarations ` xmlns:pN="u:N"`; `<p>x`, 1,199,999
  `&nbsp;`, `&amp;</p>`; `<p><ruby>a`, 200,000 `<rp>(</rp>` side by side,
  `</ruby></p>`; `<p xmlns:x="u:` and 4,194,302 `a`, then `">x</p>`;
  `<div>` 255 times, `x`, `</div>` 255 times.

## Machine

The reference desktop (see the [README](README.md#reference-machine)):
Ryzen 7 5700X, 96 GB, CachyOS, kernel 7.2.9, release build with the
repository profile (fat LTO, `panic=abort`, strip).

## Numbers

| Shape | XHTML (B) | Peak RSS median / max (KiB) | Wall median (s) | Result |
|---|---|---|---|---|
| (a) 10,000 chapters | 66,590,000 | 9,476 / 9,564 | 0.678 | book read |
| (b) 240k `<p>` in 30 blockquotes | 1,920,881 | 180,024 / 180,044 | 0.443 | book read |
| (c) 300k `<p>` in 30 blockquotes | 2,400,881 | 118,016 / 118,112 | 0.089 | chapter refused, no chapter could be read |
| (d) three 8 MiB chapters | 25,165,824 | 33,292 / 33,448 | 0.132 | book read |
| (e) 262k `<b>` | 2,096,138 | 125,836 / 126,168 | 0.091 | book read |
| (f) 200k `<p>` in 250 blockquotes | 1,606,381 | 177,896 / 178,072 | 0.515 | `book is too large` |
| (g) (d) then (b) | 27,086,705 | 137,636 / 137,652 | 0.279 | `book is too large` |

Time per byte on hostile single chapters (7 runs, median; peak RSS
measured as above):

| Chapter shape | Bytes | ns per byte | Peak RSS (KiB) |
|---|---|---|---|
| a `<span>` around every word | 4,194,431 | 10.5 | 9,452 |
| 150,000 attributes on one element | 1,689,029 | 12.3 | 29,340 |
| 100,000 namespace declarations | 2,577,919 | 19.2 | 39,480 |
| 1,200,000 entity references | 7,200,138 | 12.7 | 19,500 |
| 200,000 `<rp>` side by side in one `<ruby>` | 2,200,144 | 19.0 | 5,224 |
| 4 MiB namespace name | 4,194,454 | 3.4 | 7,300 |
| 255 nested `<div>` (refused at the depth limit, tiny input) | 2,937 | not meaningful | 3,144 |

The dense chapter (b) is the memory worst case: many small nodes cost far
more per input byte than text does, which is why the node limit exists next
to the byte limit. Truly nested `<rp>` stops at the depth limit (256)
like the nested `<div>`. All runs are 7 samples, so these are preliminary
diagnosis, not a 50-sample acceptance.

## Verdict

Memory: the worst measured peak is about 176 MiB (the dense chapter), far
below the smallest `RLIMIT_DATA` net (1 GiB). The largest book stays under 10
MiB because only one chapter is held at a time. Time: every hostile shape
runs at 3-20 ns per byte with no sign of super-linear growth. Preliminary
until 50 samples per shape are taken.

## Reproduce

Build with `cargo build --release -p inacircle-text`, generate stored-ZIP
EPUBs of the shapes above (container, a package document listing every
chapter in the spine, one XHTML file per chapter), run
`inacircle-text run text.epub-to-txt --work <dir> -- <book>` from a small
C parent (`fork`, `execv`, `wait4`) and read that child's `ru_maxrss`.

## EPUB to Markdown

What `text.epub-to-md` costs on the same kinds of books, plus books with
images. Same machine, same release build, same C parent and the same 7 runs
per shape (median and maximum of `ru_maxrss`; wall time around the child).
The book layout is the one above, with a title, an author and a language in
the package, and every image a stored JPEG (SOI, a baseline SOF segment, an
SOS segment, zero padding, EOI) listed in the manifest and referenced by a
`<p><img src="iN.jpg" alt="aN"/></p>`. The work directory is emptied between
runs.

- (a) the largest accepted book: 10,000 identical chapters of 6,659 B (the
  shape of (a) above), 68,645,381 B file.
- (b) the densest chapter that passes: 30 `<blockquote>`, 240,000
  `<p>a</p>`, 30 `</blockquote>` (the shape of (b) above), 1,922,003 B file.
- (c) the image count limit: one chapter with a paragraph of text and 1,023
  images of 64 KiB, 67,248,670 B file.
- (d) the image budget: an image chapter referencing 20 JPEGs of 8 MiB
  (deflated in the ZIP, so the book is small), then three text chapters of
  8 MiB; 25,333,812 B file, so the written images may total at most twice
  that.

| Shape | Peak RSS median / max (KiB) | Wall median (s) | Result |
|---|---|---|---|
| (a) 10,000 chapters | 10,280 / 10,412 | 1.514 | Markdown written |
| (b) 240k `<p>` in 30 blockquotes | 181,556 / 181,684 | 0.486 | Markdown written |
| (c) 1,023 images of 64 KiB | 4,468 / 4,676 | 0.141 | Markdown and 1,023 images written |
| (d) images reach twice the book size | 46,160 / 46,372 | 0.601 | Markdown and 6 images written, `14 image(s) replaced by alt text` |

Images are held one at a time (at most 64 MiB each) and written before the
next is read, so the peak grows with the largest image and not with their
count or total: (c) peaks at the level of a book without images and (d)
holds one 8 MiB image beside the text chapters, one of which is held at a
time. As with the plain-text action the
dense chapter (b) is the memory worst case, about 177 MiB, far below the
smallest `RLIMIT_DATA` net (1 GiB). The Markdown action reads the chapters
twice (a pass for the headings, then the writing pass), so (a) takes about
twice the time of the plain-text action on the same book, for the same
memory. These are 7 samples per shape, preliminary diagnosis and not a
50-sample acceptance.
