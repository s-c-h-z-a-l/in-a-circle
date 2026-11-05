# Text family: 8-bit text

What recognising an 8-bit text costs on the wheel path, and what the
Windows-1250 and ISO-8859-2 to UTF-8 actions cost in memory. Gates and rules:
[acceptance.md](acceptance.md); limits: [`resource-limits.md`](../resource-limits.md).
**Preliminary:** 9 and 7 samples per shape, not the 50 an acceptance run takes.

## Machine

AMD/x86_64 desktop, Linux 7.2.9-1-cachyos, release builds (stock profile), a
warm page cache, the compositor not involved. The "before" build is commit
3328e7b9 (the classifier without the 8-bit rule), built in its own target
directory; the "after" build is this branch.

## Classifier on the wheel path

`inacircle offer <files>` classifies every file of a drop, as the wheel does
before it shows. Each file is a 307,200 B (300 KiB) `.srt` of ASCII only, so
every file takes the read-on (256 KiB per file, 1 MiB per invocation) and is
then `text/plain`. Each run is its own process, forked and executed by a small
C parent (`fork`, `execv`, `wait4`); the figure is that process's user plus
system CPU time. 9 runs per shape; median and maximum, in ms.

| Drop | before median / max | after median / max | difference (median) |
|---|---|---|---|
| 1 file | 0.370 / 0.426 | 0.404 / 0.463 | +0.034 |
| 50 files | 0.538 / 0.688 | 0.722 / 0.798 | +0.184 |
| 1,024 files | 4.236 / 5.012 | 4.419 / 4.745 | +0.183 |

The read-on costs about 0.18 ms per drop however many files it holds: the
1 MiB budget is spent on the first four files and the rest are decided by their
head, as before. The process total includes the exec and the loader, which the
budget does not count.

## Code page actions, peak memory

`inacircle-text run text.cp1250-to-utf8 --work <dir> -- <file>` from the same C
parent; peak RSS (`ru_maxrss`, that process only), 7 runs, median and maximum.

| Input | median / max KiB |
|---|---|
| 16 MiB, mixed ASCII and accented bytes | 3,024 / 3,164 |
| 200 MiB, the same mix | 2,964 / 3,032 |
| 200 MiB, densest (every byte becomes 3 UTF-8 bytes: 600 MiB out) | 3,096 / 3,108 |

Memory does not depend on the input size: the decoder streams through a 64 KiB
buffer and a 192 KiB output buffer. Wall time of a median-RSS run: 84 ms
(16 MiB), 1.03 s (200 MiB), 1.71 s (densest, bound by writing 600 MiB).
These runs are without the sandbox; its `RLIMIT_DATA` net is far above 3 MB.

## Verdict

The classifier stays inside the wheel budget: the added CPU is about 0.2 ms
for a drop of any size, against the 1 ms of the wheel's work. The actions hold
at about 3 MB for 16 MiB, 200 MiB and the densest input.

## Reproduce

Build both commits (`cargo build --release -p inacircle -p inacircle-text`, the
old one from a `git archive` of 3328e7b9 with its own `CARGO_TARGET_DIR`),
generate the `.srt` files, and run the binaries from a C parent that calls
`wait4` and reads `ru_utime + ru_stime` and `ru_maxrss`.
