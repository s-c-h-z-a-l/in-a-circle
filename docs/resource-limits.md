# Resource limits

Every sandboxed action gets `RLIMIT_CORE` 0, `RLIMIT_FSIZE` and the memory
net `RLIMIT_DATA`. Every number has its reason here and in the code
(`crates/engine/src/sandbox.rs`).

## RLIMIT_FSIZE

16 x the total input size, at least 256 MiB, at most 16 GiB. Per file, not
the sum. The input sizes are taken once, before the action starts; an
8-bit text decoder's output cap (below) uses the same sizes.

## RLIMIT_DATA: the memory net

`RLIMIT_DATA` = RAM / 8, clamped to 1 GiB ..= 4 GiB. RAM comes from
`sysinfo(2)` (`totalram * mem_unit`), read once per action start.
`totalram` is slightly below the nominal RAM (the kernel and firmware keep
a part), so a 16 GB machine gets about 1.94 GiB, not 2. Soft and hard limit
are equal, so the action cannot raise it. It applies with or without
Landlock. A lower inherited hard limit (`ulimit -d`) wins: the cap is
clamped to it, and the message names the clamped value.

It is the final net against a faulty or missing per-family limit (each
family documents its own, for example the pixel limit of the image
actions). Over the cap, an allocation fails; a Rust action aborts with
`memory allocation of N bytes failed`, and the supervisor reports the input
as `out of memory (limit N MiB)`.

### Why DATA, not AS

`RLIMIT_AS` caps the virtual address space. glibc reserves a 64 MB malloc
arena per thread virtually, so a multi-threaded action would fail before
using real memory. `RLIMIT_DATA` (Linux 4.7 and later) counts the heap and
private writable anonymous mappings: what we want to bound. File mmaps and
the code of shared libraries do not count.

### Why a fixed, machine-dependent cap

Memory use does not follow the input size: a 3 MB JPEG decodes to 48 MB, a
50 KB PNG can be gigabytes of pixels. So the cap is not derived from the
input; the real protection is the per-family documented limit.

### What users need

Memory is the largest unit held at once (RGBA8 = 4 B/px, RGBA16 = 8 B/px).

| Who | Source | Pixels | RGBA8 | 16 bit |
|---|---|---|---|---|
| Basic | phone, normal mode (4000x3000) | 12 MP | 48 MB | - |
| Basic+ | phone, 50 MP mode | 50 MP | 200 MB | - |
| Advanced | DSLR (Sony A7R V: 9504x6336) | 61 MP | 244 MB | 488 MB |
| Advanced | A4 scan at 1200 dpi (9920x14032) | 139 MP | 557 MB | 1.1 GB |
| Power | medium format (Fuji GFX100: 11648x8736) | 102 MP | 407 MB | 814 MB |
| Power | 200 MP phone (16320x12240) | 200 MP | 800 MB | - |
| Power | stitched panorama | 300-1000 MP | 1.2-4 GB | 2.4-8 GB |

Operation factors: a format change with row-wise encoding is about 1x;
progressive JPEG (coefficients kept until the last scan, 3 B/px at 4:2:0)
about 1.75x; a resize in two passes 1.5-2x, in bands about 1x.

Reference: Pillow warns above 89 MP and rejects above 179 MP ("decompression
bomb"). The planned pixel limit of the image actions (180 MP) follows it.

The other families stream and stay small: cbz to pdf one page plus the
entry list (< 80 MB, see the pages family below); md/epub one chapter tree
(< 100 MB); archives a 32 KB window plus about 300 B per file (about 60 MB at 200 000 files); office
streamed XML (< 100 MB); audio (< 10 MB). json <-> toml needs the whole
tree (about 5-10x the file), so those actions get a file size limit.

The pages family (cbz to pdf) holds one page file whole plus the entry
list. The images are passed through, so memory follows the page file, not
its pixels:

| Limit | Value | Why, and what happens past it |
|---|---|---|
| entries | 12,000 | twice the 5,000-page case measured, with room for folder entries; about 200 B per record, 2.4 MB at the limit; `CBZ has more than 12000 entries` |
| page file | 64 MiB | a 180 MP scan is 20-40 MB as JPEG; it is the one buffer held whole and the inflate output limit (the `inflate` fuzz target runs with small limits for speed; the 64 MiB boundary is covered by unit tests); `page <name> is larger than 64 MiB` |
| ratio | 1032 | the most a deflate stream can reach (258 bytes per two bits), so every valid stream passes, a blank page stored as an uncompressed PNG (about 1000:1) included; a header claiming more is refused before inflating; a bomb is bounded by the page file limit, not by the ratio |
| name | 512 B | twice the 255-byte file name limit; at most 6 MB for 12,000 names; `damaged CBZ: an entry name is too long` |
| pages | 10,000 | below the entry limit so the message is reachable; an omnibus is a few thousand pages; about 100 B per page for the order and the PDF offsets; `CBZ has more than 10000 pages` |
| pixels | 180 MP | the image pixel limit above; `page <name> is larger than 180 megapixels` |

Measured (release build, reference machine): 5,000 small pages peak at
3.7-4.0 MB `VmHWM` (about 1.7 MB process baseline plus 2.2 MB for the
entry list, order and offsets); one 42 MB page peaks at 43.6 MB. The
derived worst case is the page file limit (64 MiB, 67.1 MB) plus the
entry list at its limits (12,000 records of about 200 B including a
typical name, plus up to 512 B per name for long ones: at most 8.5 MB)
plus the 1.7 MB baseline: about 77 MB. That is not measured, because no
real CBZ comes near it, and it is far below the smallest `RLIMIT_DATA`
net (1 GiB). Reading and checking a 42 MB page into a PDF takes about
0.18 s.

The PDF is about the sum of the page files plus, typically, about 130 B
per page; the bound is 2,560 B per page (below). So the output is bounded
by the engine's `RLIMIT_FSIZE` (16 x the input, 256 MiB to 16 GiB), and
the action checks it itself first. It reads its own `RLIMIT_FSIZE` (a
failure to read it fails the input), and before the output is created it
sums the declared page sizes plus 2,560 B of PDF structure per page and
1,024 B per file (the object text, a 256-entry palette, the xref entries
and the Kids list, an upper bound derived from the writer). Over the cap
the input fails with `PDF would be larger than <N> MiB` (N rounded down),
nothing is created and no page is inflated. A running count of the bytes
written is the second check; the half-written PDF is removed. So a CBZ of
deflated flat pages cannot expand past the limit.

The join action `pages.images-to-pdf` reads one image file at a time
(bounded read, at most 64 MiB, `image is larger than 64 MiB`), checks it
like a CBZ page (180 MP, PNG data walked once) and writes its page; the
buffer is dropped before the next one. It keeps only the list of accepted
inputs (a path each, at most 1,024 by the invocation limit) and the PDF
writer's page offsets (about 100 B per page). 1,000 small images peak at
3.3 MB `VmHWM` (debug build; the process baseline is about 1.7 MB); the
test ceiling is 8 MB. The output ceiling is the same estimate as above,
kept as a running sum over the accepted images (image size + 2,560 B per
page + 1,024 B per file) and checked before each page is written, with the
count of bytes actually written as the second check. Past it no input is
taken: the inputs already taken and those not yet examined fail with
`PDF would be larger than <N> MiB` (inputs already skipped or failed keep
their own outcome), nothing is written, and the half-written PDF is removed.

The join action `pages.images-to-cbz` stores the images unchanged in a
CBZ, so it never holds an image: each file is opened without waiting
(a FIFO fails), measured, header-checked, and copied through a 64 KiB
buffer into the archive. The header check (`probe::sniff`) reads only up
to the end of the PNG IHDR chunk or the JPEG start-of-frame segment, and
checks what a valid image must have there: the PNG signature, the IHDR
length and CRC, the JPEG marker structure, the frame's precision, sampling
factors and table selectors, and the pixel count (180 MP). It accepts
every valid variant (alpha, 16-bit and interlaced PNG; CMYK, 12-bit,
arithmetic, progressive and lossless JPEG) because the bytes are not
changed. It does not decode anything: the image body is neither inflated
nor walked, so an image damaged inside its data passes, as the file is
stored as it is. It holds a 4 KiB stack buffer and reads at most 64 MiB
(it is bounded by `take`), and the walk over JPEG markers is bounded too.

| Limit | Value | Why, and what happens past it |
|---|---|---|
| image file | 64 MiB | the page limit above, so what a CBZ holds is what `pages.cbz-to-pdf` reads back; `image is larger than 64 MiB` for that input only |
| entries | 65,534 | 0xFFFF in the 16-bit count is the ZIP64 marker; the invocation takes at most 1,024 inputs, so it is a format guard, not one a user reaches |
| archive | 4 GiB - 1 | every offset and size is 32 bits (no ZIP64); `CBZ would be larger than 4 GiB`, the whole join fails |
| name | 3+ digits plus 3 | `001.jpg`; the digits are those of the input count, at least 3, so the names sort as text; far below the 255 B name limit |
| output ceiling | `RLIMIT_FSIZE` | the exact size of the finished archive (headers, directory, end record) is projected before each image is written; past the ceiling nothing more is taken, every taken input and every one not yet examined fails with `CBZ would be larger than <N> MiB` and the half-written file is removed |

An image that grows or shrinks while it is copied (it is read for exactly
its measured size) or whose read fails is rolled back and fails alone
(`image changed while it was read` for a size change);
the CBZ keeps the rest. A failed write (disk full) leaves the archive in
an unknown state, so the whole join fails and `join/out.cbz` is removed:
image data can contain a ZIP end record, so a leftover prefix is never
kept. Memory is the 64 KiB copy buffer, the accepted-input list and the
name set (about 100 B per entry). 1,024 sparse 128 KiB images (a 128 MiB
archive) peak at about 3.5 MB `VmHWM` (debug build; the baseline is about
1.7 MB), not the sum of the images; the test ceiling is 6 MB, far below the
archive size, so memory that grew with the archive would fail it.

The join action `text.md-join-epub` parses every Markdown file of the
book and holds them until the book is packed (links across files need all
of them), so memory follows their sum, not one file. What a parsed file
holds follows its nodes and owned text, not its input bytes (24 files of
660 KB of `- a` lines, 15.8 MiB, each under the node budget, peaked at
2.9 GB held side by side). So the files of one book share ONE document
budget: the 524,288 nodes and 32 MiB of owned text of `Limits::DEFAULT`
are charged across all of them, each file read under what the files
before it left (each file still has its own 8 MiB input limit). The
largest book holds what the largest accepted single file holds. Past the
shared budget no book is made: every input the book would have taken
(the files read, the one that passed it, those not yet read, the images)
fails with `book is too large (documents exceed the document budget
together)`. A file that passes the budget on its own fails alone when it
is the first that spends any of it; after others it fails the book.
Measured peak RSS (release `inacircle-text`, no sandbox, `getrusage`
of the child, `ru_maxrss` in KiB): the 24 dense files above stop at the
second file, 185,140 KiB (190 MB), the same as one dense file of 495,000
nodes joined alone (185,112 KiB); two dense files of 261,000 nodes each
join at 161,420 KiB; two prose files of 8 MiB join at 88,276 KiB; three of
7 MiB pass the shared owned-text budget and fail at 74,500 KiB. All are
under a fifth of the sandbox's `RLIMIT_DATA` net (>= 1 GiB).
The largest input count (1,024: 24 Markdown files and 1,000 PNG images of
130,808 B, a 131 MB book) peaks at 15,640 KiB in the same method. Images
are copied in chunks, so their count does not grow the peak.
Images are never held by the book: each is header-checked
(`probe::sniff`, at most 64 MiB, 180 MP, as for the CBZ), digested in 8 KiB
reads for the book's identifier, and later streamed into the archive from
the file it was checked from (identity and size must still match). The
output ceiling is `RLIMIT_FSIZE`, checked by the write itself (the size of
a book is known only once it is packed): past it every input the book would
have taken fails with `EPUB would be larger than <N> MiB` and the file is
removed. The book has at most 1,024 chapters (`book would have more than
1024 chapters`). The header check does not read the image body: an image
damaged inside its data passes, as for the CBZ.

A PNG page is inflated once into a counter to prove it holds exactly the
scanlines its header promises. A JPEG page is passed through without
checking its scan data: the probe reads the markers, not the entropy
stream, so a JPEG truncated or damaged inside its entropy data is not
detected and shows as a grey or partial page in the reader.

Entries under `__MACOSX/` and files whose name starts with `.` (Finder
resource forks such as `._1.jpg`) are not pages; they count in the
`non-image entries left out` warning.

The code page decoders (Windows-1250 and ISO-8859-2 to UTF-8) have no
input limit: they stream the file in 64 KiB reads and hold that buffer
and an output buffer of three times it (one byte is at most three UTF-8
bytes), about 256 KiB whatever the size; the UTF-8 verdict
(`docs/architecture.md`) is a handful of counters kept as the bytes
pass. The output is bounded by the engine's cap on an 8-bit text
decoder (3 x the input + 3 B, below) and
by `RLIMIT_FSIZE`; SIGXFSZ is ignored, so a write past the ceiling fails
only that input (`The text would be larger than <N> MiB`), and the copy
is cut back to empty and removed. Measured peak (`VmHWM`, release
`inacircle-text`, no sandbox, 2026-11-04): 3,020-3,068 KiB for a 16 MiB
file, 3,020-3,036 KiB for 200 MiB of Hungarian text (281 MiB out),
3,212-3,288 KiB for 64 MiB of `0x84` only (`„`, the densest output,
192 MiB); the debug test build measures 4.3-4.4 MiB and the test's
ceiling is 8 MiB. Time is linear: each byte is looked at once (200 MiB
in 0.82 s).

The text family (txt <-> md) is held by structure budgets charged while it
builds, not after: 8 MiB input, 524,288 nodes, 32 MiB of owned text, 32 MiB
of output. The node budget was 131,072 at first; the densest real text
measured (a registry file, 15.9 B/node) reached it at about 2 MiB, so it
is 2^19, which holds an 8 MiB input at that density for roughly 100-200 MB
peak, a fifth of the smallest RLIMIT_DATA net.

A book (EPUB) is written from that document under the same budgets; the
output budget (32 MiB) covers the whole book's XHTML, chapters, table of
contents and package document together, not each file alone.

| Limit | Value | Why, and what happens past it |
|---|---|---|
| chapters | 1,024 | the engine's input limit: a book made of that many inputs is the largest one accepted, and the split counts toward it; `book would have more than 1024 chapters`, nothing is written |
| XHTML | 32 MiB | the text family's output budget, for the whole book; `document output limit exceeded`, the file is cut back to empty |
| entries | 65,534 | the ZIP writer's format guard; 1,024 chapters plus five fixed entries is far below it |
| archive | 4 GiB - 1 | every offset and size is 32 bits (no ZIP64); unreachable under the XHTML budget |
| language tag | 35 B | RFC 5646 asks for at least 35 characters; a longer or malformed tag is written as `und` |
| output ceiling | `RLIMIT_FSIZE` | an HTML page is whole in memory, so its exact size is checked against the action's own `RLIMIT_FSIZE` before the file is created; a book is packed straight to the file, so `SIGXFSZ` is ignored and the first write past the ceiling fails (`EFBIG`) and the file is cut back to empty. Either way only that input fails, with `EPUB would be larger than <N> MiB` or `HTML would be larger than <N> MiB` (N rounded down), and the rest of the batch still converts. A ceiling that cannot be read fails the input |

Memory is one chapter's XHTML at a time (its string may hold twice its
length while it grows), the ZIP writer's 64 KiB copy buffer, and the
table of contents and heading ids, bounded by the headings' text. A book
of 256 chapters of 32 KiB each (8.5 MB of XHTML) peaks at about 0.27 MB of
heap in the packer (debug build, measured by the test's counting
allocator), not the sum of the chapters.

A book is read back (`epub::read`) under limits of its own, each checked
before the bytes are read (on the declared size, which the ZIP reader
never lets the data exceed) and named when it is hit.

| Limit | Value | Why, and what happens past it |
|---|---|---|
| book file | 4 GiB - 1 | every offset and size is 32 bits (no ZIP64); `damaged EPUB: ZIP64 archives are not supported` |
| entries | 12,000 | the CBZ reader's value, so both archive readers accept the same archives; about 200 B per record, 2.4 MB at the limit; `damaged EPUB: too many entries` |
| ratio | 1032 | the most a deflate stream can reach, the CBZ reader's value; a header claiming more is refused before anything is inflated; `damaged EPUB: an entry's compression ratio is too high` |
| name | 512 B | the CBZ reader's value; `damaged EPUB: an entry name is too long` |
| skeleton file | 8 MiB, 524,288 XML nodes | `container.xml`, the package document and `encryption.xml` each, at most the text family's input and node budgets (an element, each attribute and each text run is a node); a real package is tens of KiB; `not an EPUB (package document too large)` |
| XML depth | 256 | the XML reader's `MAX_DEPTH`; past it `not an EPUB (elements nested too deeply (line L) in <path>)` |
| chapters | 10,000 | spine items, the CBZ reader's page limit (below the entry limit); a few dozen bytes each, and each manifest item they name is resolved and kept once, at most about 1 KB (the two caps below), however often the spine names it; `book has more than 10000 chapters` |
| item path | 512 B | the ZIP reader's name limit: a longer resolved path names no entry, the item is `missing`, and only its first 512 B are kept |
| media type | 255 B | RFC 6838: type and subtype names at most 127 characters each, and the `/` (parameters are cut before the check); a longer one is `unsupported type <its first 255 B>...` |
| chapter | 8 MiB | uncompressed, the text family's input limit; only that chapter fails, `too large`, the rest of the book reads |
| book | 64 MiB | the sum of the chapters to read, each as often as the spine has it (a chapter too large alone does not count): twice the 32 MiB text output, since XHTML markup is at least as large as its text; `book is too large`, nothing is read |

These fail the whole book (nothing is written for it), each with its text:

- an archive the ZIP reader refuses at `open`: `damaged EPUB: <reason>`
  (not a ZIP, truncated, ZIP64, an entry outside the file, overlapping
  entries, two entries of the same name, the limits above). The whole
  directory is checked, so any entry fails the book, a font or an image
  as much as a chapter (a font whose compression ratio is past 1032:
  `damaged EPUB: an entry's compression ratio is too high`);
- no `META-INF/container.xml`: `not an EPUB (no META-INF/container.xml)`;
- no package rootfile, or no package at its path: `not an EPUB (no
  package document)`;
- a skeleton file over its size or node limit: `not an EPUB (package
  document too large)`;
- a skeleton file that is not well-formed (or nested too deeply): `not an
  EPUB (<XML error> in <path>)`;
- a skeleton file the ZIP reader cannot read: `damaged EPUB: <reason>`;
- no spine, or an empty one: `book has no spine`;
- more than 10,000 spine items: `book has more than 10000 chapters`;
- chapters over 64 MiB in all: `book is too large`;
- every XHTML chapter encrypted: `book is DRM-protected`.

Everything else fails only the chapter it concerns.

`text.epub-to-txt` reads such a book one spine item at a time and holds
one chapter: its bytes (at most 8 MiB, up to 1.5 times that once UTF-16 is
decoded), its tree and its text. The heaviest chapter pairs a near-limit
tree with nearly the whole output budget (240,000 `<p>` inside 30 nested
`<blockquote>`, 1.9 MB of XHTML, about 29 MiB of text); a chapter after
others gets only the output budget they left. The measured peaks of these
and the other shapes (the largest book, three 8 MiB chapters, hostile
chapters) are on the benchmark page,
[`epub-read.md`](benchmarks/epub-read.md); all of them are far below the
`RLIMIT_DATA` net.

| Limit | Value | Why, and what happens past it |
|---|---|---|
| chapter budgets | the text family's, fresh per chapter | each chapter is read under its own input (8 MiB), node (524,288) and text (32 MiB) budgets, so a book costs one chapter at a time; past one only that chapter fails: `too large`, `too large (nodes limit)`, `too large (text limit)` |
| XML depth | 256 | the XML reader's `MAX_DEPTH`; that chapter fails, `too large (nesting limit)` |
| encoding name | 40 characters | the longest name the IANA charset registry allows (RFC 2978); the XML reader (`xml::MAX_ENCODING_NAME`) cuts a longer declared name with `...`, so `unsupported encoding <name>` stays short in a chapter's reason and in a book's `not an EPUB (... in <path>)` for `container.xml` or the package document, and the rest of the batch still converts |
| text output | 32 MiB | the text family's output budget, for the whole book (header, chapters and their reason lines), checked before each write; each chapter's writer gets only what is left of it, so a late chapter cannot build more text than can still be written; what is left also pays the writer's working memory (a table's cell grid, 24 B per cell), so a book just under 32 MiB whose late chapter holds a large table can fail although its text would fit; `book is too large`, the file is cut back to empty and removed |
| output ceiling | `RLIMIT_FSIZE` | checked before each write (and `SIGXFSZ` ignored, so a write past it fails with `EFBIG`, not the process); `Plain text would be larger than <N> MiB` (N rounded down, KiB below 1 MiB), the file is cut back to empty and removed, the rest of the batch still converts |

`text.epub-to-md` reads the book under the same limits and adds its
own for the heading index and the images:

| Limit | Value | Why, and what happens past it |
|---|---|---|
| heading index | 524,288 entries, 8 MiB of text | the first pass keeps every heading's id and where each heading `id` attribute and chapter lands, for the links of the second; the text family's node and input budgets, so a book's headings are held to what one document holds (each id about three times: the numbering and the lookup, so about 24 MiB at most); `book is too large`, nothing is written |
| Markdown output | 32 MiB | the text output budget, for the whole md, as for plain text; `book is too large` |
| output ceiling | `RLIMIT_FSIZE` | per file: the md is checked before each write (`Markdown would be larger than <N> MiB`, the book fails); an image larger than the ceiling keeps its alt text |
| image name | the file system's (`ENAMETOOLONG`) | `<stem>-NNNN.<ext>` is 6 bytes longer than the md's `<stem>.md`: an input stem the md fits but its images do not makes every image keep its alt text, and the book is made; a book with no text then fails, `image names too long for this folder` |
| images | 1023 | the supervisor takes at most 1024 `done` lines from one process (the md is the last) and a join at most 1024 inputs, so the md and its images can be joined back; past it an image keeps its alt text |
| one image | 64 MiB, 180 megapixels | the join's and the CBZ and PDF actions' image limits, so what this action writes the join takes; checked on the entry's declared size before anything is inflated, and on the bytes produced; past it that image keeps its alt text. The bytes are copied as they are: an image with a feature the PDF writer lacks is checked by its header only (type, size, pixel count); a PNG the PDF writer supports is also checked in structure (chunk CRCs, chunk order, the IDAT run), so a damaged one keeps its alt text; compressed pixel data is never decoded |
| images in all | twice the book file | every image read is charged its entry's declared size before it is made (kept, refused or damaged: the ZIP reader inflates a damaged entry up to its declared size before it can tell), and an entry larger than what is left is not read, so a book cannot make more than twice its own size be inflated for images (real images barely compress in the ZIP, so a book's own fit); past it the further images keep their alt text and the book is made |

Each image entry is read at most once, however often the chapters name
it; the md holds one chapter's tree and one image's bytes at a time.
When the book fails, every file written for it is cut back to empty and
removed.

A chapter fails alone, in its place, with one of: `too large`, `too large
(nodes limit)`, `too large (text limit)`, `too large (nesting limit)`,
`not well-formed XML (line L)`, `unsupported encoding <name>`, `entity
declarations are not supported`, `encrypted`, `outside the book`,
`missing`, `unsupported type <mime>`, `damaged (<ZIP reason>)`. The book
leaves no file when no chapter could be read (`no chapter could be
read`) or none holds text (`book has no text`; an image's alt text is
not text).

### The machine side

A basic user has 8 GB RAM, a power user 32-96 GB. Spawn-per-use lets several
actions run at once, so one action must not take a large share of the RAM.
1/8 is 1 GiB on 8 GB (12.5%), 2 GiB on 16 GB, and reaches 4 GiB at 32 GB.

| Machine | Cap | 180 MP RGBA8 (720 MB) | 180 MP progressive JPEG (about 1.26 GB) |
|---|---|---|---|
| 8 GB | 1 GiB | fits | does not fit: clean `fail` |
| 16 GB | 2 GiB | fits | fits |
| 32 GB+ | 4 GiB | fits | fits |

Accepted: on an 8 GB machine a very large progressive JPEG and some 16-bit
scans do not go through, with a clear message. A bomb file must not take
the basic user's machine down.

**Why 1 GiB at the bottom:** at the 180 MP pixel limit (720 MB RGBA8) the
simplest operation (a format change) must fit on the smallest supported
machine. **Why 4 GiB at the top:** a need beyond it means a bug in our own
limits, not legitimate use; the net must not let a bug grow to most of a
big machine's RAM.

### Not covered

`RLIMIT_DATA` is per process, not per process tree: each child of a
forking action gets the cap again, so the sum over a tree is not bounded
(no cgroup, no memory quota; a cgroup or systemd dependency would be a new
decision). File mmaps and shared memory are not counted. `RLIMIT_NPROC` is
per user, not per process tree, and setting it would hit the user's other
processes, so it stays unset: an action can fork-bomb until it is
cancelled. What does hold is the group: every action and manifest probe
runs in one process group it cannot leave (`setsid` and `setpgid` are
denied), the whole group is killed on cancel, timeout and exit, and a
member still alive or killed at the leader's exit fails the input. If the
supervisor itself is killed, only the leader dies with it
(`PR_SET_PDEATHSIG`); its descendants live on, sandboxed and unpublished,
until they end (see [architecture.md](architecture.md), Invocation)
([sandbox-hardening.md](benchmarks/sandbox-hardening.md)). That limits how
long a runaway tree lives, not how much memory it takes meanwhile.

## Subtitles

The subtitle family (SRT, WebVTT, ASS) reads one file into cues and writes
one file. Its size limits are the text family's, so one number bounds both
families (`crates/subtitles`, `INPUT_BYTES`, `OUTPUT_BYTES`, `MAX_MS`):

| Limit | Value | Why, and what happens past it |
|---|---|---|
| input | 8 MiB | the text family's input budget; checked before any parsing, so `subtitle file is larger than 8 MiB` and nothing is read |
| output | 32 MiB | the text family's output budget, so a converted subtitle is never larger than a converted text file. The action counts the bytes as it writes them and refuses the write that would pass 32 MiB (`subtitle output would be larger than 32 MiB`); a write past the process's `RLIMIT_FSIZE` fails the same way, since SIGXFSZ is ignored. Either way the file is removed and nothing is left behind |
| time | 99:59:59.999 | both formats write the hour as two digits, so a later cue has no time code to be written with; the reader skips that cue (with a warning) and converts the rest |
| items | 2,000,000 (`ITEM_BUDGET`) | each cue, each line and each run is one item, charged before it is stored; the readers' memory is per item, not per byte (a 2-byte line is a line and a run, about 110 B together). Past it the input fails with `too many lines and styled runs (more than 2,000,000)` and nothing is written: nothing is truncated. 2,000,000 items peak at about 220 MiB, a quarter of the 1 GiB `RLIMIT_DATA` floor. A real 8 MiB film subtitle is about 10,000 items (2,000 cues of two lines of two runs), 200 times under the budget; the densest real file, the smallest SRT cues (27 B each, about 310,000 cues of three items), is about 930,000 and still fits |
| speaker name | shared, one copy | a WebVTT cue's speaker name is one `Rc<str>` shared by the lines of its cue, not a copy per line: one 1 MiB name over 400 lines held 406 MiB as copies, 12 MiB now |
| cues | none of their own | the item budget bounds the count (a cue is one item) |
| warnings | at most 7 lines | one per kind of loss (six kinds) and one for skipped cues, far below the engine's cap of 16 per input |

Measured peak RSS (`ru_maxrss` of the child, the test build, the development
machine; the `dense_8_mib_input_stays_within_the_data_limit` test prints the
same figures). The five densest 8 MiB shapes, before and after the item budget:

| shape (8 MiB input) | before | after |
|---|---|---|
| SRT, about 310,000 cues of 27 B | 138.4 MiB, 1.57 s, kept | 129.0 MiB, 1.73 s, kept |
| SRT, one cue of 2-byte lines (`x\n`) | 906.3 MiB, 2.58 s, kept | 216.5 MiB, 0.58 s, refused (`TooComplex`) |
| WebVTT, about 310,000 cues of 27 B | 138.4 MiB, 1.49 s, kept | 129.0 MiB, 1.50 s, kept |
| ASS, about 180,000 cues of 46 B | 87.2 MiB, 1.04 s, kept | 81.6 MiB, 1.04 s, kept |
| ASS, one cue of `x\N` lines (3 B each) | 607.8 MiB, 1.56 s, kept | 216.7 MiB, 0.77 s, refused (`TooComplex`) |

The speaker-name probe (`vtt-to-srt`, one 1 MiB name in a cue of 400 lines):
406.4 MiB before, 12.1 MiB after. Its output is past the 32 MiB ceiling, so it
still fails, after parsing.

## The engine's own memory at the input limits

The input limits (1024 paths, 1 MiB of paths) are also what the engine
holds for a run, inside the `inacircle` budget (5,000,000 B PSS: wheel,
engine and supervisor). It holds each input path once: the steps' inputs,
the groups and the join's anchor borrow from the run's one list; the
`--inputs-from` file is streamed to disk, not built in memory; the
history's per-input tally keys an input by two hashes of its path under
one random key of std's keyed default hasher (128 bits), not by a copy.
Two inputs share both with a probability below n^2 / 2^129 (about 2^-109
at 1024 inputs); they would then count as one, which can change the
tally's counts and what is derived from them, the history job line's
`status` (`partial` read as `done`) and the notice's summary. What is
published, the per-input history lines and the refusal to undo a run
with an unknown publication do not depend on the tally's keys.
`inacircle run` takes its arguments over, so its argv on the heap becomes
the list (the kernel's copy on the stack stays). The wheel decodes a
dropped list from its end and gives its bytes back as they become paths
(in 64 KiB steps), so the list's bytes and its paths overlap by at most
64 KiB.

Measured (release, reference machine, 1024 inputs with ~1 KiB paths, the
list at its 1 MiB limit, one batch process, PSS sampled every 200 us
until exit): `inacircle run` 3.4-3.9 MB (8.9 MB while the engine held the
paths about seven times over). The wheel peaks at 4.2 MB (4095 kB, i.e.
4,193,280 B, at most over three runs), within the 5,000,000 B budget with 16% to spare, at
every phase: level 1 while the list becomes paths, stage 2, and the run
after the click; its stage 2 holds the paths once. Before the list was
given back as it was decoded, level 1 held the bytes and the paths
together and peaked at 4.9-5.1 MB, over the budget. Tests:
`many_long_paths_stay_within_the_pss_budget` (CLI),
`the_largest_drop_through_the_choice_stays_within_the_budget` and
`the_largest_drop_stays_within_the_memory_budget` (wheel, early and after
the drop), all gated in the release build on every phase.

## Limits added by the hardening

Every wait and every read of foreign or on-disk state is bounded while it
runs, not only by a size check beforehand (a file can grow after `stat`):
the reader asks for at most the limit plus one byte, and a longer input is
an error, never a truncation. A value past a limit is refused whole with
the message named in the code, and other inputs' finished results stay.
Where the unit says `wall` the limit is time, otherwise it is a count or
bytes of what was actually read or produced. Values are the constants in
the source; the reason column is why that number.

| Limit | Value | Reason |
|---|---|---|
| Text input (`txt` <-> `md`) | 8 MiB | A book-length text is 1-3 MiB; the longest real text measured was 0.65 MiB. Past it the input fails with its own message. |
| Text structure nodes | 524,288 | Densest real text 15.9 B/node would hit 131,072 at about 2 MiB, contradicting the input limit; 2^19 holds 8 MiB at that density for about 100-200 MB peak, a fifth of the smallest `RLIMIT_DATA`. |
| Text owned text, text output | 32 MiB each | Worst-case escape expansion is 5x (one space becomes `&#32;`, 1 -> 5 bytes, from the escaping rule in `crates/doc`). Markdown to text tables expand more and by shape, not by a constant factor: padding to 80 columns and the listed form's repeated headers. Either way a hostile 8 MiB input fails the output budget loudly, never truncated (the node budget usually stops a padded table first). |
| Published text output | 32 MiB; an 8-bit text decoder's (`in=text/x-8bit`): 3 x its input + 3 B | Checked by the engine on every text output (`out=text/*`, and any `.txt`/`.md`) while it is copied, before anything is published. 32 MiB is the text output budget above. A decoder's output is bounded by its input instead: one byte of a single-byte code page is one character of the Basic Multilingual Plane, at most 3 UTF-8 bytes, plus the 3-byte byte order mark a `.srt` gets, so the input itself needs no limit. The input's size is taken when the action starts (on a two-step route, the intermediate file's). A decoder publishes one output per input; a second one fails the input, since the cap bounds one copy. Past the cap the input fails and nothing of it is published. |
| XML element nesting | 256 elements open at once (`xml::MAX_DEPTH`) | The Markdown reader's nesting limit, so a document converts between the formats without one refusing a depth the other accepts; real XHTML chapters nest a few dozen levels. Past it the document is refused (`TooDeep`), never cut. The reader keeps its own stack (no recursion) and its work is linear in the input, so the depth bounds memory, not time. |
| Catalog cache file | 256 KiB; 128 binaries, 1024 actions, 1152 records, 32 fields per record, 12,288 fields, 32 KiB per line | A realistic cache is about 160 KB at the maximum shape. Over a limit the cache is rejected loudly and rebuilt. |
| Action folder scan | 512 entries per folder, 129 binaries held | `cache_bins` x 4 spare; past it the folder is refused whole, loudly, never trimmed. |
| Manifest | 64 KiB stdout, 256 records, 32 fields per record, 4096 fields, 32 KiB per line | 2.4x the 256-record maximum shape; checked before parsing. |
| Input list | 1024 paths, 4096 B per path, 8192 B per encoded line, 1 MiB list and total | 1024 = the outputs one action process may report; more inputs could not run in one process anyway. |
| Action events | 64 KiB per line, 1024 `done`, 1 MiB of `done` paths, 16 `warn` per input, 64 KiB read per wake-up | The supervisor never holds more than the limit; the read quota yields to cancel and exit between wake-ups. |
| History reader | header 256 B; last 8 MiB in 1 KiB chunks; 32 KiB per line, 32 fields; per job 1024 outputs and groups, 4096 publish intents, 4096 B per path, 1 MiB of paths | The window is I/O, not heap (about 700x today's history). A job not provably inside it is refused before anything moves. 1 MiB of paths is load-bearing: Undo PSS was about 4.56 MB at 1 MiB and 6.4 MB at 2 MiB in one preflight measurement. |
| Running-jobs registry | 128 entries examined, 4096 B and 16 fields per entry | A real machine has a few. Past a limit it is an error, never a shorter list. |
| Trash | 64 name attempts, 16,384 B `.trashinfo`, 32,768 B recovery intent, 4 path-resolve attempts, 4096 B path | 64 attempts cost about 18 us; 8 would refuse the 9th Undo of an equal name. The byte limits are the percent-encoded 4096 B path plus fixed fields, with 1.1-1.3x room. |
| `user-dirs.dirs` | 64 KiB | A normal file is under 1 KiB. Followed if it is a symlink (the user's own configuration), but opened non-blocking and size-checked. |
| Wheel start | 2000 ms wall (own clock, from the connection to the first commit) | 2 s is 400 times the normal start on the fake compositor (first commit 5.3 ms p50, see [wheel-startup-deadlines.md](benchmarks/wheel-startup-deadlines.md)); a policy bound, not a confidence bound. |
| Wheel teardown | 1000 ms wall | Its own bound; the old observer's 5 s and the historic 30 s anomaly say nothing about a 1 s distribution. |
| Notification | 300 ms connect and `Hello` together, 500 ms per call, 100 ms per progress update; the wheel's start-failure notice: 500 ms to open plus 500 ms for its one call, under a 2 s `alarm` | One shared frame for the whole open; the wait never holds the desktop. |
| Drop list (wheel) | 1 MiB read, 1024 URIs | The invocation's own limits (`INPUT_LIST_BYTES`, `INPUT_COUNT`): a longer list could not run anyway. Past either, the wheel shows "too many files" with nothing lit, accepts no drop and sends the source a cancel, never a truncated selection. The read is bounded to the limit plus one 4 KiB chunk. Memory at the limit: see the measurement above. |
| Classifier read-on | 256 KiB per file (the head included), 1 MiB per wheel invocation, read in 16 KiB stack chunks | Telling an 8-bit text from UTF-8 when the 4 KiB head is all ASCII: 256 KiB holds a whole film subtitle (a feature-length `.srt` is 50-150 KiB). The wheel classifies every dropped file (up to 1024) before it shows, so the read-on is a budget shared by the drop: 1 MiB is four whole windows, and 1024 x 256 KiB could not be read without stalling the wheel. Past the window or the budget only the head decides: a plain text or Markdown file goes to its text actions, which read all of it and skip it with its first non-UTF-8 byte and that byte's offset (`not UTF-8: byte 0xE9 at 266240`); another listed extension gets no type and no action. |
| User waits | 30 s per stage, 500 ms drop pipe, 500 ms soft margin | Unchanged from before; separate from the start clock. |
