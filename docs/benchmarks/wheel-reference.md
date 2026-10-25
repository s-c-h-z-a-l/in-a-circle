# Reference wheel startup: nearest-rank p95

Measured on 2026-10-25 with the unmodified plan-7 engine from
`3a1857bb435874599ef7245fa9df8b3fd1b321d8`.

**Result:** three complete 50-observation series measured startup CPU p95
of **851, 860 and 849 µs**, each below the reference gate of 1000 µs.
The nearest-rank calculation selects the 48th sorted observation in each
series. This is fresh reference startup evidence under the
[acceptance policy](acceptance.md), replacing reliance on an older p50.
It does not establish the other timing/resource gates or authorize a merge.

The [data directory](data/wheel-reference-2026-10-25/) retains every
acceptance/interrupted-series attempt, setup failures, environment
readbacks and independently recalculable summaries. Nothing was optimized
or rebuilt for these measurements.

## Environment and method

| Condition | Recorded value |
|---|---|
| CPU | AMD Ryzen 7 5700X, 8 cores / 16 threads; no affinity pinning |
| Tuning | Owner-reported all-core OC 4650 MHz at 1.1 V; operating clock residency not measured |
| Memory | Owner-reported 96 GB DDR4-3200; OS `MemTotal` 98,744,136 KiB |
| OS / running kernel | CachyOS / `7.2.9-1-cachyos`, package `linux-cachyos 7.2.9-1` |
| Boot controls | `mitigations=off`, `nowatchdog`, `tsc=reliable`, `clocksource=tsc`; complete non-device flags in the environment record |
| CPU policy | `amd-pstate-epp`, `performance` governor/EPP and power profile; CPU0 policy range 1,742,662–4,665,835 kHz |
| Compositor | niri 26.04 (`8ed0da4`), package `26.04-1.1` |
| Display | Microstep/MSI MPG491CX OLED, DP-1, 5120×1440 at 240.000 Hz, scale 1; VRR inactive |
| Runtime / input tool | glibc `2.44+r50+g1848099f063e-3`, gcc-libs `16.2.1+r23+gd564253eb6c8-1`, wtype `0.4-2.2` |
| Launcher | niri IPC `spawn` → `sh -c` → `exec inacircle`; shell command matches the Super+F3 binding |
| Input workload | Plain wheel; no drag, selection or pointer movement requested; automatic Esc about 1 s after spawn |
| Cache | Entire engine executable read before each attempt; `mincore` verified 220/220 file pages resident |
| Build | Same 900,352 B engine and 669,920 B text family as the portability report; hashes/compiler/profile in [build.json](data/wheel-2026-10-25/build.json) |

This measures **niri IPC launching the configured command**, not a physical
hotkey's recognition time. The launcher remains part of the baseline
identity. No kernel/governor/EPP/profile change was made; before/after
readbacks and executable hashes match. A policy range is not a verified
fixed operating frequency. The existing text-family action directory and
catalog were used, without a catalog-cold claim. No executable-cache
eviction, library-cache eviction, reboot, instruction/data-cache priming,
CPU isolation or exclusive host scheduling was performed.

The marker is the existing `getrusage(RUSAGE_SELF)` user+system CPU delta
from wheel-mode entry to queued mapped-wheel commit requests. The matching
wall metric uses `Instant`; neither measures socket flush, compositor
presentation, input-to-photon time or exec/loader time before wheel mode.
PSS is sampled after full paints, RSS is the process high-water mark.

The controller ran in the host PID namespace, checked for one active
engine before sending Esc, and allowed 5 s for exit. Attempts had a 0.35 s
quiet interval; complete batches had a 10 s pause. In A1 the virtual
keyboard disconnected immediately after Esc. In A2/A3 it stayed connected
for another 50 ms; this affects close control after the startup marker
and is explicitly retained as a method difference.

## Complete series

Times are µs. Every complete series contains 50 valid scenarios; no valid
outlier was removed and no series were pooled for the gate.

| Series | CPU min | CPU p50 | CPU p95 | CPU max | Wall p50 | Wall p95 | Wall max |
|---|---:|---:|---:|---:|---:|---:|---:|
| A1 | 638 | 751 | **851** | 914 | 966 | 2147 | 2415 |
| A2 | 637 | 765 | **860** | 913 | 1975 | 2181 | 2192 |
| A3 | 663 | 774 | **849** | 886 | 2063 | 2236 | 2245 |

| Series | Sampled PSS max (KiB) | RSS HWM max (KiB) | Counted idle wakeups max |
|---|---:|---:|---:|
| A1 | 2630 | 4792 | 0 |
| A2 | 2629 | 4840 | 0 |
| A3 | 2631 | 4864 | 0 |

All 150 complete-series startup CPU observations are below 1000 µs.
The sampled plain-wheel PSS maxima are also below 5,000,000 B, but do not
cover engine/supervisor peaks, a large drop or every transient allocation.
The short stationary intervals and zero counted empty polls do not count
all protocol wakeups or establish a comprehensive idle trace.

The complete-series CPU p95 range is 849–860 µs: **1.30%** from minimum to
maximum (`100 × (860/849 − 1)`). The two series with the same revised close
control are also 849 and 860 µs. This is a **pilot repeatability check**,
not a confidence interval, a universal noise bound, or an A/B comparison
against the previous main build. It does not calibrate the provisional
10%/15% regression thresholds across other environments. More independent
matched series are needed before claiming a calibrated noise model.

Wall p50 changes substantially while CPU p95 remains close. These are
different clocks and distributions: no scheduler, compositor or close-tool
cause was isolated for that wall-time difference. Presentation was not
sampled. A CPU gate pass cannot establish a visible-latency guarantee.

## Interrupted and setup attempts

Between A1 and the final A2, a batch stopped after 31 attempts: 30 closed
normally; attempt 31 remained alive after the automated Esc and the 5 s
observer deadline. Its own safety deadline closed it at about **30 s**
(start 13:17:11 UTC, last measurement-log write 13:17:41.184610 UTC).
Its later marker measured **665 µs CPU / 2000 µs wall** to startup commit.
The startup observation is retained, with the scenario marked invalid.
The interrupted cohort is below 50 and is not acceptance evidence.

The same binary completed the following 100 attempts after the virtual
keyboard's post-Esc lifetime was increased to 50 ms. This observation
does **not** prove the cause was the input tool or rule out a product or
compositor interaction. The automated close anomaly remains unresolved
follow-up evidence; it is not explained away by a passing startup percentile.

Three earlier setup attempts are retained separately: two aborted because
the controller wrongly expected host-spawned processes in its restricted
PID namespace; one collected no marker, and sending virtual Esc coincided
with interruption of the controlling terminal conversation. A subsequent host-namespace IPC
smoke succeeded (743 µs startup CPU) before the counted series. An additional
746 µs setup marker could not be assigned unambiguously and is retained
as unassigned, never used for acceptance. Setup records are not pooled
with the complete series. No engine remained after the final series.

## Reproduction and consequence

Use the pinned release build and verify both hashes from `build.json`.
Run in an unlocked host niri session with the recorded output/policy,
outside a restricted PID namespace. Prime the same text-family catalog.
Define the shell command as in `environment.json`, pointing to the verified
engine, action directory and append-only measurement log. For each attempt:

1. Read the complete engine file and verify its residency with `mincore`.
2. Record the log offset, then run `niri msg action spawn -- sh -c "$launch"`.
3. Wait 1 s and verify the host engine process exists. Send
   `wtype -k Escape -s 50`; allow 5 s to exit.
4. Preserve the new log text, process/exit failures and every marker.
   Pause 0.35 s; repeat for at least 50 valid observations per series.

Preserve failures rather than restarting an unreported clean dataset.
The data README gives the exact percentile calculation. Keep CPU and wall
results separate, and record any deliberate change to close control.

**The reference startup CPU gate passes in the three complete series.**
Other interval/hover p95s, complete memory/idle evidence, matched code
regression comparisons and the automated-close anomaly require their own
evidence. They are retained as explicit follow-ups in the
[Plan-7 integration scope](acceptance.md#plan-7-interpretation), rather than
retroactively declaring the older observations accepted under the new
method. The ThinkCentre/X280 absolute overruns and the opportunity to
avoid two full SHM paints remain as documented in the
[portability report](wheel-portability.md).
