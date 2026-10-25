# The wheel on niri and Plasma

Measured on 2026-10-25, using the plan-7 build at
`3a1857bb435874599ef7245fa9df8b3fd1b321d8`.

**Result:** the same engine and text-family binaries passed manual file
conversion tests on EndeavourOS/niri/Nautilus and CachyOS/Plasma/Dolphin.
Dragging, the two-stage wheel, cancellation, notifications, collision-safe
output names and Unicode filenames worked. Notification Undo was verified
on both systems, including a three-file Undo on Plasma. No compositor
adapter or product-code change was needed. The operator reported no
noticeable delay on either system; input-to-photon latency was not measured.

**The performance result is mixed.** Sampled PSS stayed below 3 MiB and
there were no counted idle wakeups. The historical 1 ms startup CPU budget did not
hold on either additional machine. The X280 also exceeded the early-list
and hover limits in manual runs. These remain measured absolute overruns;
the [adopted acceptance policy](acceptance.md) scopes absolute timing gates
to the reference environment and requires matched regression evidence
elsewhere. It does not relabel these samples as successful absolute
results. The earlier `main` build exceeded the startup limit on the
ThinkCentre too. Energy policy has a large causal effect, and two full
buffer paints dominate the profiled startup path.

This is evidence for two tested Wayland environments, not certification of
every compositor, file manager or hardware configuration. The original
[tuned-desktop measurements](wheel.md) remain a separate historical series.

## Machines and operating conditions

Identifiers below are model identifiers, not device serial numbers. The
hardware inventory was read from DMI, CPUID/`lscpu`, PCI/sysfs, EDID and the
running compositor. Installed package versions and policy readbacks were
captured on the same day. Complete structured specifications, including
CPU cache sizes and vulnerability/mitigation reports, are in
[`machines.json`](data/wheel-2026-10-25/machines.json).

| Hardware | ThinkCentre | ThinkPad |
|---|---|---|
| Model / machine type | Lenovo ThinkCentre M910q / `10MUS1P70L` | Lenovo ThinkPad X280 / `20KF001RMH` |
| BIOS | Lenovo `M1AKT59A`, 2023-10-27 | Lenovo `N20ET74W` 1.59, 2025-08-26 |
| CPU | Core i5-7500T, 4 cores / 4 threads, nominal 2.70 GHz | Core i5-8250U, 4 cores / 8 threads, nominal 1.60 GHz |
| CPUID / microcode | family 6, model 158, stepping 9 / `0xf8` | family 6, model 142, stepping 10 / `0xf6` |
| L3 cache | 6 MiB | 6 MiB |
| Installed memory | 8 GiB: 2 × 4 GiB Samsung `M471A5244BB0-CRC`, rank 1, Channel A/B | 8 GiB: SMBIOS reports 2 × 4 GiB, rank 1, Channel A/B; manufacturer/part not reported |
| Configured memory | DDR4-2400 MT/s, 1.2 V | DDR4-2400 MT/s, 1.2 V |
| OS-usable memory | 8,200,368,128 B | 8,064,671,744 B |
| SSD | SanDisk `SD7TB6S256G1001`, SATA, 256,060,514,304 B | Lenovo `LENSE20256GMSP34MEAT2TA`, NVMe, 256,060,514,304 B |
| Executable filesystem | Btrfs, `noatime,compress=zstd:3,ssd,discard=async` | Btrfs, `noatime,compress=zstd:1,ssd,discard=async` |
| Swap | file, 536,866,816 B; unused at inventory | zram, 8,064,598,016 B; 1,957,888 B used at inventory |
| GPU / kernel driver | Intel HD 630 (`8086:5912`) / `i915` | Intel UHD 620 (`8086:5917`) / `i915` |
| Mesa package | `1:26.2.5-1` | `3:26.2.4-1` |
| Display | NEC EA192M, DP-1, 1280×1024 at 60.020 Hz, scale 1 | internal eDP-1, 1920×1080 at 60.001 Hz, scale 1 |
| Display identification | 380×300 mm, no VRR | EDID `IVO`, product `0x04e6`, no model-name descriptor; 276×156 mm, no VRR/HDR |

SMBIOS calls both X280 memory devices SODIMMs; that label is not evidence
that the memory is replaceable. Memory/storage figures above distinguish
installed capacity, OS-usable memory and marketed decimal SSD capacity.
Inventory values such as swap use are snapshots, not run-by-run samples.

| Software and policy | ThinkCentre | ThinkPad |
|---|---|---|
| Distribution | EndeavourOS | CachyOS |
| Running kernel | `7.2.11-arch1-1` (`linux 7.2.11.arch1-1`) | `7.2.11-1-cachyos` (`linux-cachyos 7.2.11-1`, `cachyos-v3`) |
| Kernel configuration | `PREEMPT_DYNAMIC`, 1000 Hz | `PREEMPT_DYNAMIC`, 1000 Hz; O3, ThinLTO, x86-64-v3; package advertises AutoFDO/Propeller |
| Fair scheduler | distribution-default EEVDF; sched_ext disabled | EEVDF; BORE setting absent, sched_ext disabled; `SCHED_POC_SELECTOR=y` |
| Relevant boot flags | `nowatchdog nvme_load=YES loglevel=3` | `quiet nowatchdog splash` |
| Mitigations | enabled, including PTI/IBRS | enabled, including PTI/IBRS; SMT remains enabled |
| CPU driver / governor | active `intel_pstate` / `powersave` | active `intel_pstate` / `powersave` |
| Frequency policy range | 800–3300 MHz | 400–3400 MHz |
| Performance min/max | 24% / 100% | 11% / 100% |
| EPP / turbo | `balance_performance` / enabled | `balance_performance` / enabled |
| Power profile | `balanced`, mains-powered | `balanced`, AC connected |
| Automatic priority policy | ananicy-cpp inactive/not installed | ananicy-cpp `1.2.0-1.1` active; scx_loader inactive |
| Compositor | niri 26.04 (`8ed0da4`, package `26.04-1`) | KWin/Plasma 6.8.0 (`6.8.0-1.1`), Qt 6.12.0, KDE Frameworks 6.31.0 |
| File manager | Nautilus `51.1-1` | Dolphin `26.08.2-1.1` |
| Notification server | mako `1.11.0-1`; wofi `1.5.3-1` for action selection | Plasma's notification server; native Undo button |
| Test terminal | kitty `0.49.2-1` | Konsole `26.08.2-1.1` |
| glibc / gcc-libs | `2.44+r50+g1848099f063e-1` / `16.2.1+r23+gd564253eb6c8-1` | `2.44+r50+g1848099f063e-3` / same gcc-libs version |
| Microcode / firmware packages | `intel-ucode 20260925-1`, `linux-firmware 20260916-1` | `intel-ucode 20260925-1`, `linux-firmware 1:20260916-1` |

All CPU policies reported the listed governor, EPP and frequency range.
The distributions used their packaged kernels and ordinary power-saving
policies, not the reference desktop's overclock and `mitigations=off`.
They are not claimed to be identical to unmodified upstream defaults.
No kernel, governor, EPP or compositor-effect setting was changed for the
baseline runs. Temporary minimum-performance controls are labelled below.
Package, compiler, storage and compositor differences remain confounders
in any comparison *between* the two machines.

## Build and measurement definitions

Both machines ran byte-identical release binaries, built with Rust 1.98.1
(`48a229cea`, LLVM 22.1.8), target `x86_64-unknown-linux-gnu`, no additional
Rust flags, `opt-level=3`, fat LTO, one codegen unit, abort-on-panic and
stripping. The engine was 900,352 B; the text family 669,920 B. Source
commits, SHA-256 hashes, release settings and ELF runtime dependencies are
in [`build.json`](data/wheel-2026-10-25/build.json). The family was separate
from the engine, as in the shipped design.

| Metric | Definition |
|---|---|
| `main_to_commit_cpu_us` | `getrusage(RUSAGE_SELF)` user + system CPU from the start of wheel mode to the first mapped-wheel commit request |
| `main_to_commit_wall_us` | elapsed `Instant` time over that interval, including waits |
| `list_to_level1_cpu_us` | process CPU from an accepted early URI list to the lit category wheel's commit |
| `drop_to_stage2_cpu_us` | process CPU from the dropped data to the target wheel's commit |
| `hover_us_*` | **wall time**, measured with `Instant::elapsed`, covering repaint, damage and request construction |
| `idle_wakeups` | `poll` returned without a ready event before a deadline; not a count of all legitimate protocol/input events |
| `pss_kb` | highest PSS sampled after full paints, from `smaps_rollup`; not a kernel-maintained peak |
| `vm_hwm_kb` | kernel-reported peak RSS |

Linux's memory `kB` fields are KiB (1024 B). All memory results here use
KiB; binary sizes use bytes. CPU/wall times use microseconds. In ordinary
run tables, p50 is the lower middle of the sorted samples. `-1` means no
measurement was taken, not a negative duration. Hover percentiles are
per-run aggregates; their medians cannot reconstruct a pooled event p50.

The startup marker is inside `cmd_wheel`, not the first instruction of
Rust `main`. The commit marker is after queuing the client's commit
requests, before the next socket flush; it is not a presentation timestamp.
The legacy expression `run start - choice + catalog + classify` ends at
classification, before recorder setup and executor invocation. It is not
an independently measured action `execve` timestamp, so this report does
not use it to certify the choice-to-action-start budget.

## Functional checks

| Check | niri + Nautilus + mako | Plasma + Dolphin + native notifications |
|---|---|---|
| Plain open, Esc, reopen | operator confirmed | operator confirmed |
| Drag while holding files; category then target | operator confirmed | operator confirmed |
| Single text → Markdown; Markdown → text | successful sandboxed jobs | successful sandboxed jobs |
| Three-input conversion | successful job | successful job |
| Output collision handling | new names, originals preserved | new names, originals preserved |
| Unicode filename | converted in a mixed three-input batch | converted as a separate input |
| Notification Undo | one-file retest: removed 1, kept 0 | four jobs: removed 1, 3, 1, 1; kept 0 |
| Batch Undo | not separately verified on this machine | three outputs removed |
| Esc during drag; drop outside; timeout; hover/idle | operator confirmed | operator confirmed |

Each machine recorded four successful jobs and six outputs, all
`sandbox=yes`. Undo and job counts are in
[`manual-jobs.csv`](data/wheel-2026-10-25/manual-jobs.csv). On Plasma all six
new outputs were absent after Undo. Nine original fixture files on each
machine matched their pre-test copies byte for byte. No engine process
remained after the checks.

The first niri Undo attempt did not invoke the action: the test binding
omitted `--` in `makoctl menu -- wofi --dmenu`. Correcting that *test
configuration* and repeating one conversion verified Undo. It required no
product-code fix. Observer-reported checks are distinguished above from
history/filesystem evidence; the log alone does not identify every manual
cancellation gesture.

## Unmodified-binary results

Plain-start series below each contain 20 valid runs, closed by Esc about
one second after launch. No missing/negative commit measurement was
accepted and no outlier was removed.
They are preliminary diagnostic series, below the new 50-observation
minimum for p95 acceptance. Their sequential controls do not establish
environment-matched regression acceptance.

| Machine / launcher / revision | CPU p50 / max | Wall p50 / max | PSS max | Idle wakeups max |
|---|---:|---:|---:|---:|
| ThinkCentre, fresh SSH connection per run, plan 7 | 1985 / 3198 | 2218 / 3593 | 2679 | 0 |
| ThinkCentre, same method, earlier `main` (`2ba2040`) | 3020 / 3137 | 3384 / 4021 | 2694 | 0 |
| X280, one SSH connection, repeated direct subprocess launches, plan 7 | 5313 / 6145 | 6325 / 7956 | 2746 | 0 |
| X280, actual Plasma application shortcut, plan 7 | 1518 / 2813 | 2029 / 5115 | 2731 | 0 |

All 20 startup CPU samples in every series exceeded 1000 µs. The earlier
`main` control establishes that the ThinkCentre overrun predates plan 7;
it does not establish a plan-7 speedup. Series were sequential, not an
interleaved old/new regression experiment. A fresh SSH connection also
changes the machine's recent workload compared with a persistent session.
Do not pool launchers or infer a compositor/kernel effect from these rows.

Ten mixed manual wheel runs on each machine produced:

| Metric | ThinkCentre p50 / max | X280 p50 / max | Historical absolute limit |
|---|---:|---:|---|
| Startup CPU | 1671 / 4144 | 1485 / 1709 | ≤1000 µs |
| Early list → level 1 CPU | 656 / 1006 (6 samples) | 1024 / 1108 (9) | ≤1000 µs for one input |
| Drop → stage 2 CPU | 564 / 578 (4) | 775 / 816 (6) | ≤1000 µs for one input |
| Largest hover wall time | 86 (7 repaints total) | 133 (9) | ≤100 µs per repaint |
| PSS maximum | 2930 | 2868 | ≤5 MB |
| RSS high-water maximum | 5244 | 5184 | reported |
| Idle wakeups maximum | 0 | 0 | 0 |

The manual series include a three-input batch and cancelled drags; no
per-input normalization was applied. The X280's successful *single-input*
Markdown-to-text run measured 1103 µs for early-list → level 1, so that
overrun is not explained solely by the batch exemption. Three X280 runs
had hover maxima above 100 µs. Sample counts are small, especially for
hover; they document overruns, not a latency distribution guarantee.
Large drag-start wall times include waiting for the first pointer movement
and must not be read as equivalent amounts of CPU work.

## Startup phases

A separate diagnostic build added in-memory checkpoints for process CPU,
monotonic time and minor/major faults, then printed them after the surface
closed. The exact frozen patch and build instructions are in
[`spikes/wheel-profile`](../../spikes/wheel-profile/). The normal launchers
continued to use the unmodified build.

Each phase series used five runs per build variant in rotating order:
original, diagnostic with probes off, diagnostic with probes on. Executable
pages were resident, verified with `mincore`. Launches were local
subprocesses in one SSH session, with a short quiet interval. These are
not the same workloads as interactive hotkey launches.

Per-phase medians from the probe-enabled runs (CPU / wall, µs):

| Phase | ThinkCentre, baseline policy | X280, baseline policy | X280, high minimum: CPU |
|---|---:|---:|---:|
| Lock and initial setup | 64 / 64 | 87 / 88 | 27 |
| Menu model | 7 / 7 | 10 / 10 | 3 |
| Wayland connection | 48 / 49 | 60 / 61 | 19 |
| Registry roundtrip and dispatch | 87 / 583 | 148 / 799 | 38 |
| Bind/layer requests and initial flush | 23 / 21 | 32 / 30 | 8 |
| SHM memfd/mapping | 37 / 38 | 47 / 47 | 14 |
| Buffer/subsurface request construction | 7 / 7 | 11 / 11 | 2 |
| First full paint | 1635 / 1679 | 2262 / 2292 | 540 |
| Configure handling and flush | 39 / 38 | 54 / 53 | 12 |
| Second full paint | 1626 / 1647 | 2520 / 2585 | 532 |
| Initial event processing and commit requests | 418 / 425 | 491 / 713 | 239 |
| Surface/connection teardown after event loop | 175 / 175 | 262 / 263 | 100 |

The two paints together account for a median **81.7%** and **83.1%** of
profiled startup CPU respectively. Each paint incurred exactly 225 minor
faults: a 480×480×4 B buffer occupies 225 × 4096 B pages. Two newly mapped
buffers therefore first-touch 450 pages. Mapping is cheap; allocation and
first-touch costs are charged during painting. These are SHM/memfd pages,
not executable reads from disk. Painting both buffers is intentional in
the existing design, not an accidental new loop found in this test.

The approximately one-second event-loop interval is the test's wait for
Esc. Teardown above excludes subsequent measurement-log formatting,
post-overlay catalog work and final process exit. Complete child CPU,
fault and context-switch counts are retained in the data. The socket
flush after the first wheel commit was not separately instrumented.

### Probe calibration

Startup CPU p50, original / diagnostic off / diagnostic on, five samples
each (µs): ThinkCentre **4416 / 4292 / 4011**; X280 baseline
**5739 / 5620 / 5721**; X280 high minimum **1538 / 1593 / 1448**.

These small samples cannot resolve a positive probe overhead reliably
against power-state variation and changed code layout. Absolute phase
values are diagnostic estimates, not replacement budget scores for the
original binary. The much larger two-paint contribution is robust to
that uncertainty. Medians of individual phases need not sum to the median
of a complete run; CPU and wall clocks are sampled separately, so very
short intervals can also show small accounting differences.

## Same-machine energy-policy control

On the X280, ten pairs alternated AB/BA order with the *original* binary,
the same direct launcher and no user input except automated Esc. No
cache eviction was requested in this series; residency was not sampled. Only `intel_pstate/min_perf_pct` changed:

| Minimum policy | Startup CPU p50 | Min–max |
|---|---:|---:|
| Original 11% | 5522 | 5174–6099 |
| Temporary 100% | 1462 | 1415–1668 |

The ratio of medians is **3.78** and the observed ranges do not overlap.
This supports a causal effect of the minimum-performance policy. It does
not identify the instantaneous clock during the first millisecond or
justify a precise MHz-to-time scaling formula. A 100% policy request is
not a guarantee of a fixed clock under thermal/power limits. The result
still exceeded 1 ms. Governor/EPP were unchanged; `finally` restoration
and final readback confirmed 11%, `powersave`, `balance_performance`.

Scheduling wait is not process CPU time. Frequency, placement and cache
state can nevertheless change the CPU-seconds needed for the same work.
The X280 is an EEVDF measurement, not a BORE experiment.

## Cold executable pages versus a warm executable

Five alternating cold/warm pairs used only the diagnostic executable's
file cache: read it into cache for the warm case; additionally apply
`posix_fadvise(DONTNEED)` for the cold case. `mincore` verified **0/222**
resident pages cold and **222/222** warm before launch. No global cache
flush, reboot or eviction of shared libraries was performed. The X280
used the temporary high minimum to reduce the energy-policy confound;
the ThinkCentre retained its baseline policy.

| Median | ThinkCentre cold / warm | X280 cold / warm |
|---|---:|---:|
| Wheel-mode → commit CPU (µs) | 4022 / 3729 | 1584 / 1575 |
| Spawn request → wheel-mode entry wall (µs) | 9205 / 2074 | 18190 / 978 |
| Whole-child major faults | 1 / 0 | 1 / 0 |
| Wheel-mode → commit major faults | 0 / 0 | 0 / 0 |
| Wheel-mode → commit minor faults | 460 / 459 | 460 / 460 |

Pairwise cold-minus-warm median differences in commit CPU were **+7 µs**
and **−23 µs**, within run variation. The corresponding pre-entry wall
differences were **+7131 µs** and **+17271 µs**. Do not subtract marginal
medians and call that a paired effect. Cold executable availability mattered
primarily before the wheel-mode marker here; it did not explain the
post-entry CPU overrun. The pre-entry interval includes parent launch
work, fork/exec, loader, runtime initialization and argument handling;
it is not a pure SSD-latency measurement. New SHM pages are still needed
on every process start, even with a fully resident executable.

## Warm renderer control

The same release UI test binary ran the existing
`wheel::tests::hover_repaint_cost` benchmark on all three machines:

| Renderer-only metric (µs) | Tuned desktop | ThinkCentre | X280 |
|---|---:|---:|---:|
| Hover median / max, 2000 repaints | 15 / 46 | 33 / 103 | 30 / 65 |
| `Wheel::new` median, 50 samples | 1 | 3 | 3 |
| Fresh-buffer full paint median, 50 samples | 86 | 169 | 171 |
| First sample in fresh-buffer stage | 279 | 535 | 491 |
| Reused-buffer full paint median, 200 samples | 86 | 168 | 171 |

The fresh desktop control used the Ryzen 7 5700X reference host, kernel
`7.2.9-1-cachyos`, `amd-pstate-epp` with the `performance` governor. Its
policy readback is included in `machines.json`; clock residency during
the loop was not measured. This is not a new desktop startup-budget run.

The hover loop runs before the later construction/full-paint loops. This
is warm renderer throughput, not a cold wheel-start surrogate; a fresh
allocation may also reuse allocator-backed pages. The benchmark asserts
its hover **median**, whereas the documented UI limit is per repaint.
Its passing test result does not waive the ThinkCentre's 103 µs maximum
or the separate X280 manual 133 µs result.

## Reproduction and evidence

The [data directory](data/wheel-2026-10-25/) contains 185 run
records, 525 cumulative phase points, the eight manual jobs, renderer
results, exact build identity and machine specifications. It excludes
credentials, host addresses, usernames, serial numbers and working plans.
Its [README](data/wheel-2026-10-25/README.md) defines series, units and
recalculation. The probe patch is a frozen experimental artifact, not
part of the normal workspace or a new shipped binary.

For baseline niri starts, build the pinned source and use
`scripts/measure-remote.sh <host> 20`, with an active output and unlocked
session. The script assumes UID 1000 and `wayland-1`; adapt only those
session values for another host. It uses fresh SSH connections and `wtype`.
For an application-hotkey series, bind the same binary, prime the same
text-family catalog, record `INACIRCLE_MEASURE=1`, and collect 20 plain
starts closed by Esc. Record the launcher rather than combining these
series. On Plasma the test shortcut was Ctrl+Meta+F3; Meta+F3 retained its
existing desktop-switching action.

The commands above reproduce the original preliminary series, not the new
acceptance protocol. For acceptance collect at least 50 valid observations
per metric/condition, preserve all attempts, compute nearest-rank p95 and
use matched, preferably alternating A/B comparisons as defined in
[acceptance.md](acceptance.md). The script's p50/max summary alone is
insufficient; retain its raw samples and calculate p95 explicitly.

For phase/cache controls, follow the frozen probe instructions, record
both clocks/fault counters, keep all successful samples, verify file
residency and report the before/after policy. Automatic Plasma input used
`ydotool 1.0.4-2.1` with a keyboard-only transient helper, a private socket
and a runtime deadline. The helper was stopped after each experiment;
no input helper or engine process remained. The package stayed installed
as a test tool. The two normal launchers still pointed to the original
binary, not the probe build.

## Consequence for the budgets

The tests demonstrate useful portability and explain much of the observed
cost. They do not establish that every instruction is necessary or that
the code cannot be improved. The two-paint startup strategy is a design
tradeoff; energy policy materially changes its CPU-time cost.

There is no evidence here requiring an architectural rewrite, or proving
that the remaining cost is unavoidable hardware overhead. **Optimizing the
two full SHM paints remains an open development opportunity.**

The adopted [acceptance policy](acceptance.md) distinguishes absolute
reference timing gates, environment-specific regression guarantees and
mandatory safety/resource gates. Hover is wall time. Neither additional
machine is subject to the same absolute 1 ms reference gate, but the
observed overruns remain in the record. This report itself did not raise
any numerical limit. Its 20-run and mixed manual series cannot close the
new p95/regression gates. A [fresh three-series reference measurement](wheel-reference.md)
now passes startup CPU p95. The
[integration scope](acceptance.md#plan-7-interpretation) retains the other
performance coverage and automatic-close investigation as explicit
follow-ups; it does not claim that those observations passed the new gates.
