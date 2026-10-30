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

## Drop to stage 2 (pending)

The classifier now types PNG and JPEG, and the preset choice runs per
preset on the budgeted drop-to-stage-2 path. The 50-sample p95 of our CPU
from the drop's data to the stage-2 wheel's commit, for 1, 30 and 1,000
images, on the reference environment and on a second, untuned machine, is
**pending**: it needs the user at the machine. Until it is recorded here
the budget claim for that path is open.

## Verdict

Within every measured bound: 1,000 images join in under 3 MB of peak
memory, and the binaries are inside their budgets on both architectures.
The drop-to-stage-2 timing is open.

## Reproduce

```
cargo test --release -p inacircle-pages many_images_stay_within_memory -- --nocapture
cargo test --release -p inacircle-pages many_pages_stay_within_memory -- --nocapture
scripts/check-sizes.sh
scripts/fuzz.sh close manifest catalog_cache history
```
