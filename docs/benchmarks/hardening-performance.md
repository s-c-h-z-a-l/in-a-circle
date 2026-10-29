# Hardening performance acceptance

Method and the numbers that exist now. Gates and rules:
[acceptance.md](acceptance.md). Status of each gate is stated plainly: a
number is a result only where its row says so.

## Method

- **Harness.** `scripts/measure-ab.py` spawns the wheel with
  `niri msg action spawn -- sh -c` and `INACIRCLE_MEASURE=1`, waits 1 s,
  sends Escape with `wtype`, allows 5 s to exit (a survivor is killed),
  pauses 0.35 s. Before every attempt it reads the whole engine file and
  checks page residency with `mincore`. Builds alternate AB, BA, AB, ...
  (`ab` plans); `aa` plans are independent series of the A binary 10 s
  apart. Every attempt is one set of rows in `attempts.jsonl` (schema of
  `scripts/measure-wheel.py` plus `build`): crashes, hangs, missing
  markers and `-1` fields stay as `invalid`/`missing`, never dropped; a
  replacement is an extra attempt. A plan step runs until both builds have
  50 valid observations or 100 attempts each.
- **Summary.** `scripts/summarize-ab.py` (nearest-rank p95, lower-middle
  p50, N, attempts, non-valid, first/second-half p50 as order trend).
  Series and builds are never pooled. A regression is printed only where
  both sides have at least 50 valid observations.
- **Gates in code.** `memory_gate` compares every sample, after
  KiB x 1024, with 5,000,000 B (the maximum decides; a zero, missing or
  too few sample is INCOMPLETE, never PASS). `idle_gate` keeps premature
  empty polls, context switches in a window and resident processes apart.
  `marker_attempts` turns a missing or `-1` marker field into `missing`,
  PSS 0 into `invalid`, and a functional failure into `invalid` rows with
  its reason.
- **Builds.** A = `169163b` (instrumentation only, before the hardening hot
  path changes), sha256 `692a0e5d...27c7`. B = the hardening release, source
  `b696450`; the binary (sha256 `530de180...1019`, series label
  `B-c901291`) was built from `c901291`, whose only difference from
  `b696450` is tests and scripts. Same profile (fat LTO, `panic=abort`, stripped) and the
  same `INACIRCLE_MEASURE` instrumentation, which is a runtime switch.

## Memory (release, fake compositor, this machine)

The large-drop test keeps all four stage-2 observations (two runs of one
file, two of 1024 files) and each must pass the 5,000,000 B gate. Three
invocations, 12 observations, PSS sampled once after stage 2 (a sampled
value, not a transient peak):

| Workload | N | Max PSS | Gate |
|---|---|---|---|
| 1 file, stage 2 | 6 | 3,064,832 B | PASS |
| 1024 files, stage 2 | 6 | 3,219,456 B | PASS |

Anonymous memory grows by about 156 kB between the two (188 to 344 kB).
Not covered by this table: engine/supervisor phases of a running action and
the Undo window (an evidence gap, as in acceptance.md).

## Idle (fake compositor)

`a_stationary_wheel_wakes_for_nothing_and_leaves_no_engine`: with a seat
(pointer and keyboard) and the pointer inside, the wheel took **0 context
switches in a 2 s window** (5 of 5 runs), had no child process, reported
`idle_wakeups=0`, and was gone after close. The window sits after the
startup waits: a wheel without a seat device legitimately wakes once at
the 2 s startup deadline (found while writing the test). Protocol events
and deadline expiry are named, not counted as polling. This covers a
stationary wheel; the Undo window's end is not observed here.

## ThinkCentre (stock kernel, untuned)

Intel i5-7500T, kernel 7.2.11-arch1-1, governor `powersave`
(`balance_performance`, 0.8-3.3 GHz), niri 26.04, 4 CPUs, 8 GB.
**The machine has no monitor or dummy plug**, so the session niri has no
output. The series ran inside a second niri (winit backend) started under
the session niri: a real niri with one 1280x960 output, but the compositor
side (configure, frame pacing: wall time ~1.5 s) is not representative.
The CPU intervals are the wheel's own; wall times are reported, not
interpreted. Condition: `startup-plain-warm` (executable pages resident,
warm private cache). A cold condition was not run.

Startup CPU, main to first commit, microseconds (all attempts valid, 0
failures; N = 50 valid of 50 attempts per row):

| Series | Build | p50 | p95 | max |
|---|---|---|---|---|
| aa1 | A | 3268 | 3628 | 3658 |
| aa2 | A | 3085 | 3428 | 3474 |
| ab1 | A | 3327 | 3561 | 3772 |
| ab1 | B | 1896 | 1966 | 1979 |
| ab2 | A | 3080 | 3646 | 3808 |
| ab2 | B | 1890 | 1979 | 1996 |

- A/A p95 spread 5.5% (3628 vs 3428); the A/B effect is B **-45% / -46%**
  p95, reproduced in two independent alternating runs, far outside the A/A
  spread. aa2 drifts upward (first-half p50 2763, second-half 3302 us);
  the other series do not, and the drift is far below the A/B effect.
- `wheel_cpu_us` p95: B -8.9% / -10.3% against A. Process PSS p95 at plain
  startup: A 2.74 MB, B 1.82 MB (both under 5,000,000 B).
- Hypothesis, not measured: the improvement comes from the commit-first
  change (no obsolete full paint before the first commit; the second buffer
  is painted after the first flush). There is no phase breakdown that
  isolates it. Total wheel CPU moves less than the commit interval.
- ThinkCentre absolute CPU (about 2-3.6 ms) is outside the reference gate
  by policy ([acceptance.md](acceptance.md), Plan-7 interpretation); only
  the environment-matched regression applies, and there is none.

## Hover maximum (open budget question)

The gate is a per-event wall maximum of 100 us on the reference
environment ([acceptance.md](acceptance.md)). On the fake compositor on
this machine (release builds, 10 alternating runs of one stage-2 session,
122 hover events per run), the
median of the per-run maxima is:

| Build | p50 | p95 (median of runs) | max (median of runs; range) |
|---|---|---|---|
| A (`169163b`, before the hardening) | 17 us | 37 us | 102 us (90-122) |
| `86a9b4a` (before the render change) | 18 us | 37 us | 105 us (95-158) |
| B (this branch, with the render change) | 17 us | 37 us | 109 us (85-186) |

The maximum is in the first events after a stage swap (about 85-110 us
for the first two, then about 20 us). With a 20 ms pause between a buffer
release and the next event (a diagnostic variant of the same harness) the
medians of the per-run maxima fall to 43 / 45 / 48 us for A / `86a9b4a` /
B, but single runs still reach 138 / 113 / 169 us. So the maximum is at or
above 100 us in the **A build too**: not a hardening regression, but the
100 us per-event maximum is not shown to hold, and whether the delivery
pattern of the fake compositor or the real compositor decides it is
**open: a run on the real niri decides it**. Until then the gate is open,
not passed, and the integration proposal has to state the outcome. (The
harness and its raw files are not part of the repository.)

## Machines (inventory of 2026-10-29)

Read from the machines themselves, read-only. A missing value is missing,
not estimated. Older inventories (the ThinkCentre and X280 in
[wheel-portability.md](wheel-portability.md)) are history; the running
kernel and power state below were read again.

| | Reference desktop | ThinkCentre M910q (disposable) |
|---|---|---|
| CPU | AMD Ryzen 7 5700X, 8 cores / 16 threads (SMT on), one socket, family 25 model 33 stepping 2 | Intel Core i5-7500T, 4 cores / 4 threads, one socket |
| Microcode | 0xa201213 | 0xf8 |
| Cache | L1d 256 KiB, L1i 256 KiB, L2 4 MiB (8 inst.), L3 32 MiB | L1d 128 KiB, L2 1 MiB, L3 6 MiB |
| Frequency | max 4665 MHz, min 562 MHz; `amd-pstate-epp` active, governor `performance`, EPP `performance`, policy 1742-4665 MHz; boost enabled | 800-3300 MHz; `intel_pstate` active, governor `powersave`, EPP `balance_performance`, turbo allowed |
| RAM | 94 GiB visible (MemTotal 98,744,132 kB); user-reported DDR4-3200 dual channel; speed and slots not read (needs root) | 7.6 GiB visible (8,008,172 kB); speed and slots not read (needs root) |
| Board / firmware | ASUS ROG STRIX B550-F GAMING WIFI II, AMI BIOS 3621 (2025-01-13) | Lenovo 10MUS1P70L (ThinkCentre M910q), BIOS M1AKT59A (2023-10-27) |
| OS / kernel | CachyOS, 7.2.9-1-cachyos, `PREEMPT_DYNAMIC` (`PREEMPT`), HZ 1000; `CONFIG_SCHED_BORE` is not in the running config, so the default EEVDF applies (scheduler debugfs not readable) | Arch Linux, 7.2.11-arch1-1, `PREEMPT_DYNAMIC`, HZ 1000, EEVDF |
| Mitigations | **`mitigations=off`**, `split_lock_detect=off`, `nowatchdog`, `tsc=reliable clocksource=tsc`; spectre_v2, spec_store_bypass, spec_rstack_overflow, tsa and vmscape report "Vulnerable" | defaults: PTI, IBRS, MDS clear-buffers, retbleed IBRS, vmscape IBPB |
| Security modules | capability, landlock, lockdown, yama, bpf; Landlock ABI 10 (qualified) | same list; Landlock ABI 10 (qualified earlier on 7.2.11) |
| THP / swap | THP `madvise`; zram swap 8 GiB | THP `always`; 512 MiB swapfile |
| Compositor | niri 26.04 (8ed0da4), Wayland; DP-1 5120x1440 at 240 Hz, scale 1, VRR supported but off | niri 26.04 (8ed0da4); **no monitor attached**, so the session niri has no output; the series ran in a nested niri (winit) with one 1280x960 output |
| GPU | NVIDIA RTX 3080 Ti, driver 615.71.09 | not queried |
| Storage | NVMe (Samsung MZVL21T0HCLR), Btrfs `compress=zstd:1`, `noatime`, `discard=async`, subvolumes `@` and `@home`; `/tmp` tmpfs (`huge=advise`) | SATA SSD (SanDisk SD7TB6S256G1001), Btrfs `compress=zstd:3`, `noatime`, `discard=async`; `/tmp` tmpfs |
| Toolchain | rustc and cargo 1.98.1 (pinned by `rust-toolchain.toml`), release profile fat LTO, `codegen-units=1`, `panic=abort`, stripped | no `rustc` on its `PATH`; release binaries built on the reference machine and copied |

Native CI runners: Ubuntu 26.04 (kernel 7.0.0-1017-azure), x86_64 and
aarch64, Landlock ABI 8, rustc 1.98.1. Own ARM guest: aarch64, kernel
7.0.0-38-generic, ext4, ABI 8, QEMU TCG emulation. X280 (ThinkPad, CachyOS,
Plasma): **not available**, so no fresh inventory exists; its earlier
qualification (kernel 7.2.11-1-cachyos, ABI 10) is history. Physical ARM:
none.

Method items common to the series: launcher `niri msg action spawn -- sh
-c`, input fixture `wtype` Escape only after the layer surface is proved
mapped, observer `INACIRCLE_MEASURE=1` (a runtime switch in the shipped
binary), workload and counts as in each table, executable pages checked
resident with `mincore`, no global cache drop, PSS from `smaps_rollup` in
KiB times 1024. Executable hashes: A `692a0e5d...27c7`, B
`530de180...1019`. The A and B binaries were built with the pinned
toolchain on the reference machine. The results of the two machines are
never pooled.

## Binary size

The hardening grew `inacircle` from 900,352 B to 1,082,216 B (+181,864 B;
`.text` +150,192 B). The per-area account, measured by rebuilding the
stripped release at each area's last commit, is in
[binary-size.md](binary-size.md#hardening-growth-since-plan-7). The limit of
1.5 MB holds with 490,648 B to spare.

## Open

| Item | State |
|---|---|
| Hover maximum (100 us per event) | OPEN budget question: 85-186 us per-run maxima on the fake compositor, in the A build too; a run on the real niri decides |
| Desktop (reference) A/A and A/B-B/A, early-list / drop / choice | OPEN: needs the reference desktop with a user present (`scripts/measure-desktop-ab.sh --i-am-present`) |
| X280 Plasma A/B | OPEN: machine unavailable; not a pass, not a baseline reset |
| ThinkCentre cold condition, early-list, drop, choice, hover workloads | OPEN: only the plain warm startup ran; the cold condition has not been run |
| ThinkCentre with a real output | OPEN: nested niri only (no monitor attached) |
| Choice-to-action entry with real clicks | OPEN: needs a real pointer session |
| Separate counters for protocol events without input and for deadline wakes | OPEN: only premature empty polls (`idle_wakeups`) and inputs (`events`) are counted |
| SHM first-touch / page-fault cost apart from render CPU | OPEN: not measured |
| Presentation-time marker | OPEN: not implemented in the product; not shown to be available on every target compositor |
| Physical ARM desktop performance | OPEN; ARM VM results are emulated, functional only |
| Native ARM CI / own ARM VM | Native CI green on `c84f4ad` (run 40835264396). Own ARM VM hardening suite 31/31 on `b696450` (emulated, functional only), not re-run on the final commit |
| Engine/supervisor and Undo-window memory phases | OPEN: not yet measured (headless, to be run after the last code change) |

Reproduce: copy two release directories and the two scripts to the target,
inside a niri session run
`scripts/measure-ab.py --out DIR --a-bin A --b-bin B --plan aa,aa,ab,ab`,
then `scripts/summarize-ab.py DIR`.
