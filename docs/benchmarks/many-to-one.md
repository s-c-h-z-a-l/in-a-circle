# Many to one (images to one PDF)

What the join action `pages.images-to-pdf` costs in memory and binary
size, and the checks behind it. Gates and rules:
[acceptance.md](acceptance.md). The action contract is in
[`architecture.md`](../architecture.md), the limits in
[`resource-limits.md`](../resource-limits.md); the CBZ side of the family
is in [pages-family.md](pages-family.md).

## Method

- **Memory.** The release `inacircle-pages` runs the join and the test
  samples `/proc/<pid>/status` `VmHWM` (the kernel-kept peak RSS) until
  the child exits. Two workloads: 1,000 images joined into one PDF, and a
  5,000-page CBZ (the one-to-one action, for comparison with the
  previous page).
- **Size.** `scripts/check-sizes.sh` (stripped release builds): file size
  and `.text`. Budgets: `inacircle` file <= 1.5 MB, a family binary
  <= 1 MB of `.text`.
- **Robustness.** `scripts/fuzz.sh close` (600 s per target, libFuzzer) on
  the manifest, catalog cache and history targets; `scripts/mutants.sh` on
  the changed code; `cargo llvm-cov` per crate.
- **Compatibility.** A history job written by the previous plan's binary
  was undone by this plan's binary.

## Machine

The reference desktop (see the [README](README.md#reference-machine)):
Ryzen 7 5700X, 96 GB, CachyOS, kernel 7.2.9, release build with the
repository profile (fat LTO, `panic=abort`, strip). The aarch64 sizes come
from the native arm64 GitHub runner (ubuntu-26.04-arm), CI run
41046473864, job "Native (ubuntu-26.04-arm)", step "Binary sizes", commit
c31147e.

## Numbers

| Measurement | Result |
|---|---|
| 1,000 images to one PDF, peak `VmHWM` | 2,916 kB |
| 5,000-page CBZ, peak `VmHWM` | 3,564 kB |

| Binary | Arch | File (B) | `.text` (B) |
|---|---|---|---|
| `inacircle` | x86_64 | 1,147,120 (+40,472 over the previous plan) | |
| `inacircle-pages` | x86_64 | | 372,211 (+12,432) |
| `inacircle` | aarch64 | 1,053,448 | 737,700 |
| `inacircle-pages` | aarch64 | 463,568 | 316,776 |

`inacircle` still links none of the `zip`, `flate`, `probe` or `pdf`
crates (`scripts/check-shipped-deps.sh`). Both binaries are inside their
budgets on both architectures.

| Check | Result |
|---|---|
| test suite | 1,395 passed, 0 failed, 4 ignored |
| fuzz `close`: manifest, catalog cache, history, 600 s each | 0 findings |
| mutation testing | 264 mutants tested; every survivor killed but one, judged equivalent (the second outcome of a finished input is dropped, so the result cannot be observed) |
| line coverage | engine 91.77%, action 82.03%, `inacircle-pages` 89.44% |
| old history, new binary | a job written by the previous plan was undone by this plan's binary (pass) |

The sandboxed children write corrupt `profraw` files, so the coverage
profiles were merged with `failure-mode=warn`.

## Drop to stage 2

The classifier types PNG and JPEG, and the preset choice runs per preset on
the budgeted drop-to-stage-2 path. The metric is our CPU from the dropped
data to the stage-2 wheel's commit (`INACIRCLE_MEASURE=1`), p95 by nearest
rank for the series; the image drops are hand-made runs, so they are shown
as ranges. Builds, machines and the text-path series (the regression check
against `main`) are in [pages-family.md](pages-family.md#drop-to-stage-2):
the text path moved +4% (desktop) and +0.2% (ThinkCentre) p95, p95 171 us
and 618 us, against the 1,000 us budget. Build B is the branch with the
join; B2 adds the drop-list fix below.

| Machine | Build | Images | Runs | Drop to stage 2 (us) | Early list to level 1 (us) |
|---|---|---|---|---|---|
| desktop | B | 30 | 5 | 141-167 | not recorded |
| desktop | B | 1,000 (~57 KB list) | 3 | 165-173 | 3,680-3,923 |
| ThinkCentre | B | 30 | 5 | 454-529 | 1,475-1,899 |
| ThinkCentre | B | 1,000 (~71 KB list, 62-byte paths) | 9 attempts, none worked | - | - |
| ThinkCentre | B2 | 1,000 | 3 | 468-507 | 14,800-15,900 |

The early list to level 1 grows with the number of inputs (per-input
work, reported and not budgeted); the drop to stage 2 does not.

**Finding.** With build B, every one of the 9 attempts on the ThinkCentre
refused the 1,000-image drag silently (the wheel showed "Drop here"): the
list of dropped paths (~71 KB with 62-byte paths) was over the 64 KiB drop
list limit. The desktop list (~57 KB, shorter paths) fit, which hid it
there. The limit was raised to 1 MiB (B2), still a fixed, bounded limit; with B2
the same drag works and the wheel's PSS stayed about 2.8 MB (gate 5 MB).

**End to end.** One 1,000-image drag on the ThinkCentre with B2 ran the
whole join: a PDF of 1,000 pages, 4,191,043 B, PDF 1.4, `qpdf --check`
clean; wheel PSS 2,796 kB; drop to stage 2 357 us.

The second machine is outside the reference timing gate by policy and is
shown for the absolute overrun check; nothing is over.

## Images to CBZ

The join action `pages.images-to-cbz` writes a stored (method 0) ZIP: each
image is copied byte for byte into one archive named `001.jpg`, `002.png`
and so on, with no recompression.

**Method.** The release `inacircle-pages` joins 1,024 sparse 128 KiB
images (a 128 MiB archive) and the test samples `/proc/<pid>/status`
`VmHWM` until the child exits (limit in the test: 6 MiB).
`scripts/check-sizes.sh` gives the stripped release sizes. The drop to
stage 2 uses `INACIRCLE_MEASURE=1`, as in the section above.

**Machine.** The reference desktop (Ryzen 7 5700X, 96 GB, CachyOS, kernel
7.2.9, repository release profile), x86_64.

| Measurement | Result |
|---|---|
| 1,024 images x 128 KiB to one CBZ, peak `VmHWM` | 3,232 kB |
| `inacircle` file (x86_64) | 1,160,024 B |
| `inacircle-pages` file / `.text` (x86_64) | 503,712 B / 403,267 B |
| Drop to stage 2 with PNG/JPEG, p95, reference desktop | pending (manual run) |
| Drop to stage 2 with PNG/JPEG, p95, second machine | pending (manual run) |
| `main` to exit CPU, warm cache, p95 | pending (two-machine run) |

The two timing rows are the user's manual drag runs (50 or more valid
observations per condition, nearest-rank p95); nothing is recorded for
them yet and no value is implied.

**Verdict.** Memory and sizes are inside their bounds (peak memory 3.2 MB
for the largest accepted workload; `inacircle` under 1.5 MB, `.text` of
`inacircle-pages` under 1 MB). The timing verdict stays open until the
two rows above are filled.

**Reproduce.**

```
cargo test --release -p inacircle-pages many_large_images_stay_within_memory -- --nocapture
scripts/check-sizes.sh
```

## Verdict

Within every measured bound: 1,000 images join in under 3 MB of peak
memory, and the binaries are inside their budgets on both architectures.
The drop to stage 2 is 141-173 us per run on the desktop and
357-529 us on the ThinkCentre, far under the 1,000 us budget.

## Reproduce

```
cargo test --release -p inacircle-pages many_images_stay_within_memory -- --nocapture
cargo test --release -p inacircle-pages many_pages_stay_within_memory -- --nocapture
scripts/check-sizes.sh
scripts/fuzz.sh close manifest catalog_cache history
```
