# Resource limits

Every sandboxed action gets `RLIMIT_CORE` 0, `RLIMIT_FSIZE` and the memory
net `RLIMIT_DATA`. Every number has its reason here and in the code
(`crates/engine/src/sandbox.rs`).

## RLIMIT_FSIZE

16 x the total input size, at least 256 MiB, at most 16 GiB. Per file, not
the sum.

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
taken, every input fails with `PDF would be larger than <N> MiB`, and the
half-written PDF is removed.

A PNG page is inflated once into a counter to prove it holds exactly the
scanlines its header promises. A JPEG page is passed through without
checking its scan data: the probe reads the markers, not the entropy
stream, so a JPEG truncated or damaged inside its entropy data is not
detected and shows as a grey or partial page in the reader.

Entries under `__MACOSX/` and files whose name starts with `.` (Finder
resource forks such as `._1.jpg`) are not pages; they count in the
`non-image entries left out` warning.

The text family (txt <-> md) is held by structure budgets charged while it
builds, not after: 8 MiB input, 524,288 nodes, 32 MiB of owned text, 32 MiB
of output. The node budget was 131,072 at first; the densest real text
measured (a registry file, 15.9 B/node) reached it at about 2 MiB, so it
is 2^19, which holds an 8 MiB input at that density for roughly 100-200 MB
peak, a fifth of the smallest RLIMIT_DATA net.

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
| Text owned text, text output | 32 MiB each | Worst-case escape expansion is 5x (one space becomes `&#32;`, 1 -> 5 bytes, from the escaping rule in `crates/doc`), so a hostile 8 MiB input fails the output budget cleanly. |
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
| User waits | 30 s per stage, 500 ms drop pipe, 500 ms soft margin | Unchanged from before; separate from the start clock. |
