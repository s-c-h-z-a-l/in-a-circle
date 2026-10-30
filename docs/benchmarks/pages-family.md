# Pages family (CBZ to PDF)

What `inacircle-pages` costs in memory, time and binary size, and the
checks that bound its input. Gates and rules: [acceptance.md](acceptance.md).
The limits and their reasons are in
[`resource-limits.md`](../resource-limits.md); the action contract in
[`architecture.md`](../architecture.md).

## Method

- **Memory.** The release `inacircle-pages` converts a CBZ and the test
  samples `/proc/<pid>/status` `VmHWM` (the kernel-kept peak RSS) until
  the child exits. Two workloads: 5,000 one-pixel PNG pages (the entry
  list, order and PDF offsets at scale), and one 42 MB page (the one buffer
  held whole).
- **Time.** The same large-page run, wall time of the child from spawn to
  exit, release build. A single run, a diagnosis, not a p95 gate.
- **Size.** `scripts/check-sizes.sh` (stripped release builds): file size
  and `.text`. Budgets: `inacircle` file <= 1.5 MB, a family binary
  <= 1 MB of `.text`.
- **Robustness.** `scripts/fuzz.sh close` (600 s per target, libFuzzer) on
  the three parsers' targets; `scripts/mutants.sh` on the changed code;
  `cargo llvm-cov` per crate.

## Machine

The reference desktop (see the [README](README.md#reference-machine)):
Ryzen 7 5700X, 96 GB, CachyOS, kernel 7.2.9, release build with the
repository profile (fat LTO, `panic=abort`, strip). The aarch64 sizes come
from the native arm64 GitHub runner (ubuntu-26.04-arm), CI run
41017837937, job "Native (ubuntu-26.04-arm)", step "Binary sizes", commit
af93036.

## Numbers

| Measurement | Result |
|---|---|
| 5,000 small pages, peak `VmHWM` | 3,660 kB |
| one 42 MB page, wall time | 174 ms |
| one 42 MB page, peak `VmHWM` | 43,468 kB |

The memory follows the largest page file plus the entry list, not the
pixels (the images are passed through). The derived worst case is the 64 MiB
page limit plus the entry list at its limits plus the baseline, about
77 MB; it is not measured because no real CBZ comes near it, and it is far
below the smallest `RLIMIT_DATA` net.

| Binary | Arch | File (B) | `.text` (B) |
|---|---|---|---|
| `inacircle` | x86_64 | 1,106,648 | 841,832 |
| `inacircle-pages` | x86_64 | 451,240 | 359,779 |
| `inacircle` | aarch64 | 987,912 | 705,124 |
| `inacircle-pages` | aarch64 | 463,568 | 306,776 |

`inacircle` grew by 1,112 B for the new action (classifier entry, preset
and group); it links none of the `zip`, `flate`, `probe` or `pdf` crates,
which `scripts/check-shipped-deps.sh` enforces. `inacircle-pages` is
about a third of its 1 MB code budget.

| Check | Result |
|---|---|
| fuzz, 600 s per target | 0 findings |
| fuzz `inflate` (limit 64 KiB) | 1,600-3,000 exec/s per worker, coverage 274 to 279 |
| fuzz `zip_reader` (entry limit 256 KiB) | about 8,300 exec/s per worker, coverage 530 to 593 |
| mutation testing | 874 mutants; every survivor killed or justified |
| line coverage | flate 94.68%, zip 88.16%, probe 92.52%, pdf 92.93%, pages 93.20% |

The fuzz limits are small for speed: with a 256 KiB limit the `inflate`
target ran at about 440 exec/s per worker, with 64 KiB at 1,600-3,000; the
64 MiB page limit is covered by unit tests. A ~0.7 MB PNG that declares
180 MP costs the PNG data check about 0.5 GB of inflate work into a
counter (8.3 s under ASan); the work is linear, reports progress per page
and can be cancelled.

## Drop to stage 2 (pending)

The classifier now recognises ZIP content on the budgeted drop-to-stage-2
path. The 50-sample p95 of our CPU from the drop's data to the stage-2
wheel's commit, on the reference environment and on a second, untuned
machine, is **pending**: it needs the user at the machine. Until it is
recorded here the budget claim for that path is open.

## Verdict

Within every measured bound: the memory is the page file plus a few MB, a
42 MB page converts in under 0.2 s, and the binaries are inside their
budgets on both architectures. The drop-to-stage-2 timing is open.

## Reproduce

```
cargo test --release -p inacircle-pages many_pages_stay_within_memory -- --nocapture
cargo test --release -p inacircle-pages large_page_timing -- --ignored --nocapture
scripts/check-sizes.sh
scripts/fuzz.sh close inflate zip_reader
```
