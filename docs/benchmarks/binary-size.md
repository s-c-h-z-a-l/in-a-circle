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

## Hardening growth since Plan 7

`CLAUDE.md` asks a plan that grows a binary by more than 100 KB to explain
why. Over the hardening branch `inacircle` went from 900,352 B to
1,082,216 B (**+181,864 B**; `.text` 671,864 to 822,056 B, **+150,192 B**).
It stays under the 1,572,864 B limit with 490,648 B to spare. No single
area is over 100 KB; the sum is.

Method: the stripped release build (`cargo build --release`, rustc 1.98.1,
fat LTO, `codegen-units=1`, `panic=abort`) of the last commit of each area,
sizes from `stat` and `size -A`. The build at the Plan 7 commit `6e5604c`
reproduces the old baseline exactly (900,352 / 671,864 B), and the builds at
`169163b` and `12a4b32` have the same SHA-256 as the measurement binaries A
and B. Each row is the difference to the row above it.

| Area (commits) | File | `.text` | Why this code exists |
|---|---|---|---|
| Measurement markers (`169163b`) | +4,752 | +4,208 | Phase and action-entry markers and hover samples behind the runtime switch `INACIRCLE_MEASURE`; kept in the shipped binary so the binary that ships is the one measured. |
| seccomp policy (`7bb1f04`) | +784 | +32 | One policy table evaluated at compile time into a static BPF program per architecture; it lives in data, the code is tiny. |
| Mandatory sandbox gate, privilege check, clean environment (`111361b`, `5875d84`) | +8,640 | +7,568 | Landlock ABI probe, the uid/gid/capability snapshot and its refusal, the environment allowlist. No weakened mode exists. |
| Process-tree cleanup (`b33befa`) | +6,040 | +5,056 | Child subreaper, group `SIGKILL` before reaping, `waitid` over the kernel's child list. |
| Publication (`8f9ccae`) | +31,272 | +26,880 | Private staging, fresh copies, final modes, baselines, `RENAME_NOREPLACE` commit, outcome states. |
| History v1 and private state (`39ca066`) | +52,920 | +40,752 | Strict versioned writer and reader (22 refusal cases), intents before the rename, state folders opened once, registry. |
| Trash Undo and recovery (`37a655d`) | +74,552 | +63,328 | Trash selection (home and volume), `mountinfo`, `.trashinfo`, recovery intent, move-back, place proof, per-group results. The largest area: it replaces a delete that could not be made safe. |
| THP switch (`081459b`) | +64 | +64 | Two `prctl` calls. |
| Bounded reads and state (`38604ea`) | -10,928 | -9,472 | Bounded readers for input, cache, scan and history replaced whole-file reads; the net is a decrease, analysed below. |
| Text loss notice in `inacircle` (`9754585`) | +1,360 | +1,072 | The "Formatting was lost." line only for a real loss. |
| Wheel start and failure lifecycle (`86a9b4a`) | +7,232 | +5,920 | Own start deadline, ordered teardown, the failure notice through the bounded notification path. |
| Render generation tracking (`12a4b32`) | +3,448 | +3,280 | Which buffer is painted when, so a held buffer is never written. |
| Review fixes up to HEAD (`d6569c4` to `HEAD`) | +1,728 | +1,504 | Recovery trusts only the recorded source, retry after a no-op Undo, registry pid 0, manifest failure ordering. |
| **Total** | **+181,864** | **+150,192** | |

Cross-check by symbol (unstripped builds at `6e5604c` and `HEAD`, grouped
by the first two path segments of the demangled name; inlining and generics
blur it): `engine::trash` +46.8 KB (a new module), `engine::journal`
+39.8 KB, `engine::catalog` +13.9 KB, `engine::exec` +11.4 KB,
`engine::output` +7.5 KB, `dbus::notifications` +6.7 KB. The rest is spread
over the wheel and the shared standard-library generics.

`inacircle-text` grew from 669,920 B to 695,000 B (+25,080 B; `.text`
+22,720 B), almost all in the text-family structure budgets and loss
tracking (`9754585`: +23,944 B). That is under 100 KB, far under the
family limit of 1 MiB of code (397,314 B), and its baseline is refreshed
too.

The bounded-reads decrease, by evidence. Unstripped builds of the
commits from `081459b` (the row above) to `38604ea`, `nm -S --size-sort`
per commit (a build after each of the commits, same flags, `strip` off):
`.text` 819,752 B to 810,280 B (-9,472); the stripped file 1,079,376 B
to 1,068,448 B (-10,928). Where it
went, per commit: `7980655` (bounded input and state reads) -3,072 B of
`.text`; `9005e83` (the action folder scan bounded while it runs)
-8,368 B; the other six together +1,552 B. By symbol, `081459b` to
`38604ea`: the old whole-file `journal::job_records` (-10,812 B) is gone
and its work is done by the new bounded `journal::scan_file` (+11,513 B),
`read_line_bounded` (+1,057 B) and `read_header_len` (+1,125 B), so the
journal is a small net growth, not a shrink. The shrink is in the catalog
and the standard library: the iterator and sort instantiations over
`catalog::Bin` (a `FilterMap` over the directory iterator -8,929 B,
`quicksort` -4,421 B, `drift::sort` -2,534 B, `sort4_stable` -931 B)
were replaced by a bounded loop in `Catalog::load` (+8,914 B) and
`read_cache_from` (+2,920 B), and the `BufReader` `read_until`,
`skip_until` and `read_exact` instantiations (about -4.7 KB) are no
longer used. Net by symbol -11,186 B (unstripped; the stripped file
differs by 258 B of symbol-table and frame data). So the decrease is
fewer monomorphised generic instantiations, not a lost function, and the
bounded readers' own code is included in the later rows' totals.

Judgement: every area is a contract the hardening adds and tests; nothing
here is dead weight, and none is unexplained. The weakest explanation is
stated above (the measurement markers could be compiled out at the price
of measuring a different binary). All figures in this section are for
`c84f4ad` (read "HEAD" above as `c84f4ad`); later commits change code (a
plain message for a symlinked state, cache or jobs folder) and the
baselines are refreshed once more from the final commit. A release build
of the tip after those messages measures `inacircle` 1,093,584 B (`.text`
830,968 B), 11,368 B above `c84f4ad`, still 479,280 B under the limit; the
cause of that growth has not been analysed yet. The final code commit
`420a714` measured `inacircle` 1,103,168 B (`.text` 839,112 B) locally. The final
build, `70e9984` (it adds the second buffer's first-touch move and the
Trash ownership rule), measures 1,105,536 B (`.text` 841,176 B), +23,320 B over
`c84f4ad` and 467,328 B under the limit; both baselines were refreshed on it,
and the per-area account of that last growth was not made. The
aarch64 baseline is measured on the native arm64 runner, not here: on
`c84f4ad` it was 987,880 B (`.text` 688,292 B), +131,080 B over the earlier
baseline taken after the sandbox gate (856,800 B); on the final build it is
987,912 B (`.text` 704,420 B; native CI run 40955927479). It has no per-area
account; the x86_64 areas above are the guide. Both baselines are
refreshed in `scripts/` with their provenance.

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
