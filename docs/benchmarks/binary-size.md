# Binary size and run cost of `inacircle`

**Question:** the headless engine took `inacircle` past the old 500 KB
binary budget. Where do the bytes go, does size cost anything at run
time, and what does a run cost?

**Verdict:** the budget was wrong, not the binary. Stable `std` alone is
~350 KiB of it, and size has no effect on startup or RSS: pages that are
never touched are never faulted in. The budget is now `inacircle`
≤ 1.5 MB and each family binary ≤ 1 MB of code; a plan that grows a
binary by more than 100 KB explains why. A small run stays under 1 ms of
our CPU (pre-`main` included) and 3 MB peak RSS; the worst case tested
stays under 3.4 MB.

## Method

- Size: `cargo build --release` (opt-level 3, fat LTO, one codegen unit,
  `panic=abort`, `strip`). The breakdown comes from an unstripped build
  of the same profile (`CARGO_PROFILE_RELEASE_STRIP=false`): every
  symbol's size from `nm -S`, grouped by its demangled path.
- Run cost: `INACIRCLE_MEASURE=1 inacircle run …` prints
  `getrusage(RUSAGE_SELF)` (our CPU, from exec, so pre-`main` included),
  `getrusage(RUSAGE_CHILDREN)` (the actions) and `VmHWM` on stderr. Each
  case runs from a shell with private XDG dirs and `INACIRCLE_ACTIONS_DIR`
  pointing at the release action binaries, warm page cache, fresh input
  files per run; 30 runs (5 for the largest case), p50.
- Rust 1.98.1, x86_64, the [reference machine](README.md#reference-machine).

## Binary size

| binary | bytes | KiB |
|---|---|---|
| `inacircle` (release) | 650,472 | 635 |
| `inacircle`, opt-level `s` | 582,296 | 569 |
| `inacircle`, opt-level `z` | 561,824 | 549 |
| `inacircle-text` (release) | 670,960 | 655 |

Where the bytes of `inacircle` go (unstripped release build, symbol
sizes):

| part | KiB |
|---|---|
| `std`, other (I/O, process, collections, alloc, panic) | 176 |
| `std` backtrace and symbolization (gimli, addr2line, miniz_oxide, demangling) | 173 |
| engine (`crates/engine`) | 136 |
| formatting (`core::fmt`) | 40 |
| ours, other (`inacircle`, `manifest`, `action`) | 32 |
| **named symbols** | **556** |

The remaining ~80 KiB of the stripped file is unwind tables, dynamic
symbol and relocation tables and the ELF headers.

- **Half of the binary is `std`, and half of that is the backtrace
  symbolizer.** With `panic=abort` and a stripped binary it can never
  produce a useful backtrace, but stable Rust links it anyway; removing
  it takes `build-std` (nightly) or `no_std`.
- **opt-level `s`/`z` saves 68-89 KiB** (10-14 %) at the cost of speed in
  the hot paths (hover repaint, event parsing). Not worth it, see below.
- **Size does not cost at run time.** The binary is mapped, not read:
  only the pages a run touches are faulted in, and the backtrace code,
  most of `fmt` and the paths a run does not take are never touched. A
  run's peak RSS (below) is under 3 MB for a 635 KiB binary; startup and
  RSS keep their own budgets.

## Run cost

| case | our CPU (p50 / max) | actions' CPU | peak RSS |
|---|---|---|---|
| one `.txt` to Markdown | **690 / 762 µs** | 457 µs | **2.85 MB** |
| one unsupported file (no action started) | 510 / 557 µs | 0 | 2.83 MB |
| 64 folders, one `.txt` each (64 processes) | 11.5 ms | 28.8 ms | 2.98 MB |
| one input, 1024 outputs | 9.7 ms | 3.6 ms | 3.26 MB |
| 64 folders × 1024 outputs (65,536 files) | 568 ms | 230 ms | 3.39 MB |

- **A small run is within budget:** ~0.7 ms of our CPU from exec to exit,
  pre-`main` included, for reading the input, classifying it, running
  the action under the sandbox and placing the result.
- **Peak RSS does not grow with the number of outputs.** Before the
  engine streamed outcomes to the observer and the history (keeping only
  counters per input), the 65,536-file case peaked at **42.5 MB**; now it
  is 3.39 MB, against 2.85 MB for a single file.
- The per-output cost (~8 µs of our CPU per placed file) is the
  `rename`, the identity `stat` and one history line each.

## Reproduce

```sh
cargo build --release
R=$PWD/target/release
M=$(mktemp -d); mkdir -p $M/{actions,cache,state,home,config,files}
mkdir -m 700 $M/runtime
ln -s $R/inacircle-text $R/inacircle-testaction $M/actions/
export INACIRCLE_ACTIONS_DIR=$M/actions XDG_CACHE_HOME=$M/cache \
  XDG_STATE_HOME=$M/state XDG_RUNTIME_DIR=$M/runtime \
  XDG_CONFIG_HOME=$M/config HOME=$M/home
cd $M/files

# one .txt (repeat with a fresh a.txt each run)
printf 'hello world\n' > a.txt
INACIRCLE_MEASURE=1 $R/inacircle run markdown a.txt >/dev/null

# 64 folders x 1024 outputs
for i in $(seq 0 63); do mkdir d$i
  printf 'INACIRCLE-TEST\noutputs=1024\n' > d$i/o.test; done
INACIRCLE_MEASURE=1 $R/inacircle run --action test.script d*/o.test >/dev/null

# size breakdown
CARGO_PROFILE_RELEASE_STRIP=false cargo build --release -p inacircle --target-dir /tmp/nostrip
nm -S --size-sort -C /tmp/nostrip/release/inacircle
```
