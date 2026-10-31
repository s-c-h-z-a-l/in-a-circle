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

## Drop to stage 2

The classifier recognises ZIP content on the budgeted drop-to-stage-2
path. The metric is our CPU from the dropped data to the stage-2 wheel's
commit (`INACIRCLE_MEASURE=1` lines), p95 by nearest rank. Main has no CBZ
path, so the regression check is the existing text path (a text file
dropped on the Document slice) with the old and the new binary; the CBZ
(8 pages, stored) is then measured on the new binary alone. The drags were
made by hand, one series per condition; a sample of `-1` is a wheel that
received no drop and is excluded as not valid.

Builds: A is `main` 6e0873b (`inacircle` sha256 22ba2264...). B is the
branch that adds this family and contains the next plan's work (the join
action) as well, f981591f...; so B's text series also shows whether that
work cost the path anything. Machines: the reference desktop (Ryzen 7
5700X, CachyOS, kernel 7.2.9) and a ThinkCentre M910q (i5-7500T, Arch,
kernel 7.2.11), inventory as in
[hardening-performance.md](hardening-performance.md#machines). The second
machine is outside the reference timing gate by policy and is shown.

| Machine | Build | Drop | Valid | p50 (us) | p95 (us) | max (us) |
|---|---|---|---|---|---|---|
| desktop | A | txt to Document | 58 of 66 | 158 | 164 | 168 |
| desktop | B | txt to Document | 65 | 160 | 171 | 175 |
| desktop | B | CBZ to Book | 64 | 151 | 166 | 188 |
| ThinkCentre | A | txt to Document | 65 of 69 | 561 | 617 | 671 |
| ThinkCentre | B | txt to Document | 52 | 554 | 618 | 667 |
| ThinkCentre | B | CBZ to Book | 57 | 517 | 571 | 609 |

The text path moved +4% (p95) on the desktop, under the 10%
investigation threshold, and +0.2% on the ThinkCentre. The CBZ path costs
the same as the text path or slightly less (it has no text sniffing).
The boundary between B's text and CBZ series on the desktop was inferred
from the gaps in time between the drags, not recorded.

## Reproduce

```
cargo test --release -p inacircle-pages many_pages_stay_within_memory -- --nocapture
cargo test --release -p inacircle-pages large_page_timing -- --ignored --nocapture
scripts/check-sizes.sh
scripts/fuzz.sh close inflate zip_reader
```
