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
entry list (< 60 MB); md/epub one chapter tree (< 100 MB); archives a 32 KB
window plus about 300 B per file (about 60 MB at 200 000 files); office
streamed XML (< 100 MB); audio (< 10 MB). json <-> toml needs the whole
tree (about 5-10x the file), so those actions get a file size limit.

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

`RLIMIT_DATA` is per process: each child of a forking action gets the cap
again. `RLIMIT_NPROC` is per user, not per process tree, and setting it
would hit the user's other processes, so it stays unset.
