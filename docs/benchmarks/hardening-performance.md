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
  path changes), sha256 `692a0e5d...27c7`. B = the final hardening code,
  commit `420a714`, sha256 `9a1ac05a...98c5` (built twice from the same
  tree, identical). Shipped source is unchanged after `420a714`; later
  commits touch tests, scripts and docs. Same profile (fat LTO,
  `panic=abort`, stripped) and the same `INACIRCLE_MEASURE` instrumentation,
  which is a runtime switch. An earlier run on a previous candidate
  (source `b696450`) gave the same startup numbers within noise (B p95 1966
  and 1979 us against 1977 and 1957 us now).

## Memory (release, fake compositor, reference desktop)

Every observation is gated at 5,000,000 B as a maximum (PSS from
`smaps_rollup`, KiB times 1024). PSS has no kernel-kept peak, so the
numbers below are **sampled maxima** unless a row says otherwise.

**Wheel (stage 2 after a drop).** `a_large_drop_stays_within_the_memory_budget`,
3 invocations, 12 observations, PSS sampled once after stage 2:

| Workload | N | Max PSS | Gate |
|---|---|---|---|
| 1 file, stage 2 | 6 | 3,053,568 B | PASS |
| 1024 files, stage 2 | 6 | 3,204,096 B | PASS |

Anonymous memory is about 190 kB for one file and 344 kB for 1024.

**Supervisor (the same process after the choice) with the notifications and
the Undo window.** `supervisor_memory_stays_within_the_ceiling_in_every_phase`
runs `inacircle run --notify` against a scripted notification server on a
private socket, with the Undo window set to 1 s and no click. A thread
outside the process reads its `smaps_rollup` every 2 ms from spawn to exit;
a phase ends at each notification the server receives. The run's own
`VmHWM` (the kernel's peak RSS, never below PSS) is printed before the
notices and reported next to the PSS: a peak at or under the ceiling would
bound every transient, but RSS counts the shared libc and loader pages in
full, so a peak above it proves nothing about PSS and is not gated. Three
invocations, same code:

| Workload | Phase | Samples | Max PSS | Max RSS |
|---|---|---|---|---|
| 1 input | conversion, before the final notice | 2 | 1,265,664 B | 3,727,360 B |
| 1 input | final notice and Undo window | 483-484 | 1,340,416 B | 3,801,088 B |
| 1024 inputs | conversion | 26 | 2,219,008 B | 4,624,384 B |
| 1024 inputs | final notice and Undo window | 477-483 | 2,219,008 B | 4,624,384 B |
| 1 input, 2.5 s action | action running | 484-485 | 1,227,776 B | 3,633,152 B |
| same | "running" notice with Cancel up | 723-725 | 1,301,504 B | 3,706,880 B |
| same | final notice and Undo window | 483 | 1,301,504 B | 3,706,880 B |

All PASS on PSS. The run's own peak (`VmHWM`) is at most 4,624,384 B
here, under the ceiling, so on this machine it bounds the PSS peak too. On
the x86_64 CI runner (Ubuntu 26.04, a slower run) the 1024-input case
reached PSS 3,648,512 B (sampled) but peak RSS 5,648,384 B, above 5,000,000 B:
the PSS gate holds, the RSS bound does not, which is why RSS is reported
and not gated. The 1024-input run has the least PSS headroom of the cases:
1.35 MB under the ceiling there. The conversion of one
input is over in about 5 ms, so only two samples fall into it; the
`VmHWM` figure covers that phase. The action children (`inacircle-text`,
the test action) are separate processes and are not part of the 5 MB
ceiling. Not covered: the wheel and the supervisor in one session on a real
compositor (the two halves are measured separately, wheel above,
supervisor here).

## Idle (fake compositor)

`a_stationary_wheel_wakes_for_nothing_and_leaves_no_engine`: with a seat
(pointer and keyboard) and the pointer inside, the wheel took **0 context
switches in a 2 s window**, had no child process, reported
`idle_wakeups=0`, and was gone after close; 5 of 5 runs on the final
code. The window sits after the startup waits: a wheel without a seat device
legitimately wakes once at the 2 s startup deadline (found while writing
the test). Protocol events and deadline expiry are named, not counted as
polling. After the last run no `inacircle` process existed. The supervisor's
Undo window is a timed wait on the notification connection and the
cancel signal (see the supervisor section of the architecture); the memory
runs above sampled it for a full second and it stayed under the ceiling,
but a separate wakeup count of the window is not made.

## ThinkCentre (stock kernel, untuned)

Intel i5-7500T, kernel 7.2.11-arch1-1, governor `powersave`
(`balance_performance`, 0.8-3.3 GHz), niri 26.04, 4 CPUs, 8 GB.
**The machine has no monitor or dummy plug**, so the session niri has no
output. The series ran inside a second niri (winit backend) started under
the session niri: a real niri with one 1280x960 output, but the compositor
side (configure, frame pacing: wall time ~1.5 s) is not representative.
The CPU intervals are the wheel's own; wall times are reported, not
interpreted.

Startup CPU, main to first commit, microseconds, final code. Every row is
50 valid of 50 attempts (0 failures). Warm = executable pages resident
(read whole, checked with `mincore`):

| Condition | Series | Build | p50 | p95 | max |
|---|---|---|---|---|---|
| warm | aa1 | A | 3300 | 3461 | 3536 |
| warm | aa2 | A | 3274 | 3497 | 3561 |
| warm | ab1 | A | 3309 | 3497 | 3621 |
| warm | ab1 | B | 1897 | 1977 | 1984 |
| warm | ab2 | A | 3198 | 3398 | 3425 |
| warm | ab2 | B | 1893 | 1957 | 1978 |
| cold | aa1 | A | 3243 | 3457 | 3574 |
| cold | ab1 | A | 3236 | 3475 | 3507 |
| cold | ab1 | B | 1886 | 1944 | 1963 |
| cold | ab2 | A | 3241 | 3461 | 3571 |
| cold | ab2 | B | 1886 | 1961 | 1963 |

The alternating runs are AB, BA, AB, ... until both builds have 50 valid
observations.

- Warm: B is **-43.5% / -42.4%** p95 against A in two independent
  alternating runs; the A/A p95 difference is +1.0% (3461 vs 3497). The
  earlier candidate run had -44.8% / -45.7% and an A/A spread of 5.5%; this
  one is quieter, and the first-half/second-half p50 trend is within 5% in
  every warm series (the largest, ab1 A, is a fall from 3399 to 3245 us). `wheel_cpu_us` p95 B -9.0% / -7.0%. Process PSS p95: A
  2.74 MB, B 1.86 MB (under 5,000,000 B). No regression.
- B against the earlier candidate run: p95 1977 / 1957 now, 1966 / 1979
  before. The small startup changes of the last fixes (an earlier flush,
  one comparison per event, one alarm at close) are not measurable.
- Cold: the binary's page cache was dropped before **every** attempt with
  `posix_fadvise(DONTNEED)` (no privilege; `scripts/measure-ab.py --cold`),
  from a binary on the disk file system (the `/tmp` of this machine is a
  tmpfs, where eviction does nothing), and `mincore` had to show at most 5%
  of its pages resident (all 250 attempts showed 0). "Cold" therefore means
  the engine binary only: the C library, the loader and the compositor's
  pages stay cached, and the page cache of the whole machine was not dropped
  (that needs root). B is **-44.1% / -43.3%** p95 against A cold, the
  cold and warm effects agree to within two percentage points (the second
  half of cold ab1 B runs 9% below its first half, 1898 to 1725 us, which
  does not change the p95). The first-touch cost of a cold binary
  does not show in the CPU interval (page-fault waits are not CPU) and the
  wall time is dominated by the nested compositor, so this condition says the
  CPU is unchanged, not that a cold start is as fast in wall time. One cold A/A
  series, not two.
- ThinkCentre absolute CPU (about 2-3.6 ms) is outside the reference gate
  by policy ([acceptance.md](acceptance.md), Plan-7 interpretation); only
  the environment-matched regression applies, and there is none.
- Hypothesis, not measured: the improvement comes from the commit-first
  change (no obsolete full paint before the first commit; the second buffer
  is painted after the first flush). There is no phase breakdown that
  isolates it. Total wheel CPU moves less than the commit interval.
- Measured later with fault counters: the same change moved buffer 1's
  first touch (225 minor faults, about 0.9 ms of CPU on the ThinkCentre)
  from the startup into the early list → level 1 point, which then
  measured 1.44 ms p50 on the fake compositor and 1.6 ms p50 / 2.1 ms max
  by hand on niri there. Buffer 1 is now faulted in while the wheel
  waits after its first commit; on the fake compositor list → level 1 is
  back to 531 / 537 / 538 µs (p50 / p95 / max, 50 runs) on the
  ThinkCentre, with main → commit unchanged
  ([wheel.md](wheel.md#the-second-buffers-first-touch-moved-into-the-wait)).

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
`9a1ac05a...98c5`. The A and B binaries were built with the pinned
toolchain on the reference machine. The results of the two machines are
never pooled.

## Binary size

The hardening grew `inacircle` from 900,352 B to 1,103,168 B on the final code (`.text`
839,112 B; +202,816 B and +167,248 B over Plan 7). The per-area account up
to `c84f4ad` (1,082,216 B), measured by rebuilding the stripped release at each
area's last commit, is in
[binary-size.md](binary-size.md#hardening-growth-since-plan-7). The limit of
1.5 MB holds with 469,696 B to spare. The native arm64 runner builds the same
code to 987,912 B.

## Open

| Item | State |
|---|---|
| Hover maximum (100 us per event) | OPEN budget question: 85-186 us per-run maxima on the fake compositor, in the A build too; a run on the real niri decides |
| Desktop (reference) A/A and A/B-B/A, early-list / drop / choice | OPEN: needs the reference desktop with a user present (`scripts/measure-desktop-ab.sh --i-am-present`) |
| X280 Plasma A/B | OPEN: machine unavailable; not a pass, not a baseline reset |
| ThinkCentre early-list, drop, choice, hover workloads | OPEN: only the plain startup ran (warm and cold); cold means the engine binary only, not a dropped machine-wide cache |
| ThinkCentre with a real output | OPEN: nested niri only (no monitor attached) |
| Choice-to-action entry with real clicks | OPEN: needs a real pointer session |
| Separate counters for protocol events without input and for deadline wakes | OPEN: only premature empty polls (`idle_wakeups`) and inputs (`events`) are counted |
| SHM first-touch / page-fault cost apart from render CPU | OPEN: not measured |
| Presentation-time marker | OPEN: not implemented in the product; not shown to be available on every target compositor |
| Physical ARM desktop performance | OPEN; ARM VM results are emulated, functional only |
| Native ARM CI / own ARM VM | Native CI green on `431d8df` (run 40895881758; shipped code equal to `420a714`). Own ARM VM hardening suite 31/31 on `b696450` (emulated, functional only), not re-run on the final commit |
| Engine/supervisor and Undo-window memory phases | Measured headless on the final code, all under 5,000,000 B (table above). Not covered: the wheel and supervisor in one session on a real compositor, and a wakeup count of the Undo window |

Reproduce: copy two release directories and the scripts to the target,
inside a niri session run
`scripts/measure-ab.py --out DIR --a-bin A --b-bin B --plan aa,aa,ab,ab`
(add `--cold` with the binaries on a disk file system for the cold
condition), then `scripts/summarize-ab.py DIR --a LABEL --b LABEL`. The
memory phases: `cargo test --release -p inacircle --test notify
supervisor_memory -- --nocapture`; the wheel's: `--test wheel a_large_drop`
and `a_stationary_wheel`.
