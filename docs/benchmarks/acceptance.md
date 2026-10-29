# Performance acceptance

The absolute timing targets describe a **recorded reference environment**.
Portability is protected by environment-matched regression comparisons.
Safety and resource ceilings remain mandatory on every supported system.
This policy was adopted after the [niri/Plasma investigation](wheel-portability.md);
it does not turn its measured overruns into successful absolute results.

## Gates and their scope

| Gate | Scope and acceptance evidence |
|---|---|
| Wheel startup CPU | Reference environment: p95 ≤ 1000 µs from wheel-mode entry to the first mapped-wheel commit request |
| Other budgeted CPU intervals | Reference environment: p95 ≤ 1000 µs per documented waiting point for one input; separately measure early-list → level 1, drop → stage 2, and choice → action start |
| Hover repaint | Reference environment: ≤ 100 µs **wall time per event**; report event p95 and maximum, preserving the per-event ceiling |
| Timing regression | Every measured environment: compare its own baseline under the rule below; an additional machine is not subject to the reference's absolute timing gate |
| Engine memory | Every supported environment: ≤ 5 MB PSS (5,000,000 B) for wheel/engine/supervisor; record RSS alongside |
| Idle | No resident process between uses, no autonomous periodic work while waiting, and zero counted premature empty polls; verify each property separately |
| Safety | Sandbox, no overwrite, bounded input/memory processing and hard deadlines remain mandatory; performance cannot waive them |
| Binary/dependency budgets | Existing `CLAUDE.md` ceilings and shipped-dependency checks remain mandatory |

For non-wheel commands the existing `main` → exit CPU target excludes
per-event work. For the interactive wheel, apply the target to its
documented waiting points, not the time spent waiting for the user or a
sum of unrelated phases. Multi-input cost is reported separately; a
single-input acceptance series must not mix in batches or cancellations.

Use `getrusage(RUSAGE_SELF)` user + system time for process CPU. Also report
elapsed wall time over the same interval. Exec/loader work before the
wheel-mode marker and compositor waits are reported separately. The commit
marker records queued requests, not socket flush or presentation. A
classification-complete proxy does not establish the child's action-start
time; name the actual marker and leave an unmeasured gate open.

## Sample and percentile rule

Each acceptance metric/condition needs **at least 50 valid observations**.
For sorted observations `x[1] ... x[N]`, use nearest-rank:

```
p95 = x[ceil(0.95 * N)]             # one-based
index = (95 * N + 99) // 100 - 1   # zero-based, integer arithmetic
```

For N=50 the selected observation is the 48th; for N=100 it is the 95th.
Record N, p50 (lower middle), p95, minimum and maximum, with raw observations.
Fifty samples are a minimum for acceptance, not a precision or confidence
guarantee for the tail. Increase the sample count when repeatability is poor.

Declare the attempt count and workload before collecting data. Record all
attempts, including crashes, timeouts, missing markers and invalid runs,
with reasons. Failures need investigation; replacement runs are additional
attempts, not erasure of failures. Keep every valid outlier. A large duration
alone is not grounds for invalidation. Fewer than 50 valid observations
leaves the p95 gate open. Older 20-run series remain preliminary evidence.

Hover p95 requires individual repaint observations, not an average of
per-run medians or a percentile of per-run maxima. If the current logger
only retains summaries, collect event samples with a documented harness or
report that p95 is unavailable. A passing p95 does not waive a per-event
maximum, memory ceiling, failure or safety requirement.

## Baselines belong to environments

A baseline records at least:

- CPU model/topology, microcode, memory and relevant hardware;
- distribution, running kernel, scheduler/mitigations and runtime libraries;
- compositor/version, output size, refresh rate and scale;
- exact launcher, session, input fixture and action/catalog state;
- AC/battery state, governor, frequency bounds, EPP and power profile;
- source revision, executable hashes, compiler, target and release flags;
- cache definition, priming/eviction method and residency evidence where claimed;
- timing markers/clocks, observer/tool versions, sampling cadence and workload.

Record before/after policy, workload/load and known thermal/background
activity. A frequency-policy request is not a verified fixed CPU clock.
Warm executable pages do not imply warm instruction/data caches, libraries,
SHM pages or a warm catalog. Define cold executable tests separately from
boot-cold tests; do not pool them.

An intentional build change identifies A (baseline) and B (candidate);
other conditions, fixtures, measurement instrumentation and build settings
must match. Prefer alternating AB/BA pairs to reduce order/drift bias.
If alternating is impossible, state why and measure the resulting uncertainty.
Kernel, launcher or power-policy changes define another baseline rather
than an unexplained code regression. Retain previous baselines and their
scope; do not reset one just to hide a slower candidate.

## Regression and repeatability

For a positive baseline p95 compute
`100 * (candidate_p95 / baseline_p95 - 1)`. Report absolute values and
differences alongside the percentage; if the baseline is zero, a relative
percentage is undefined and needs an explicit absolute comparison.

- **More than 10%:** mandatory investigation before acceptance.
- **More than 15%**, reproduced in an independent matched comparison and
  not explained by measured noise: **merge blocker**.

These are initial engineering thresholds, **not thresholds calculated from
the current measurements**, and not statistical significance tests. Exactly
10% or 15% does not exceed that respective trigger. The >10% investigation
must still be resolved even below the merge-blocking threshold.

Calibrate repeatability using independent A/A series with the same binary
and conditions, each containing at least 50 valid observations. Report
their p95 spread and ordering/time trends. Then repeat suspect A/B results
with at least 50 valid observations per variant and condition in each
comparison. Explain why the effect exceeds observed A/A variation and
what uncertainty remains. If noise prevents a conclusion, improve the
method and collect more data; do not declare a pass or automatically widen
the thresholds. Threshold changes require a documented decision.

## Memory and idle evidence

Linux `/proc/*/smaps_rollup` PSS and `VmHWM` values labelled `kB` are KiB.
Multiply by 1024 before comparing PSS with 5,000,000 B. Since the
hardening branch the release large-drop test keeps every observation (one
file and 1024 files at stage 2, two runs each) and fails when **any**
sample is above 5,000,000 B; a zero, a missing or too few samples is
incomplete, never a pass. The same rule is in `scripts/measure-wheel.py`
(`memory_gate`) for the series. The earlier debug-build guard of 5120 KiB
is gone: the RSS guard in CI measures the shipped release binary.

PSS has no kernel-maintained peak. Record which processes are covered,
sampling points/cadence and workload (plain wheel, single input, large
drop, engine/supervisor). The wheel's current logger samples after full
paints; report this as **sampled maximum PSS**, not proof of every transient
peak. Memory acceptance requires covering the relevant live phases;
unobserved engine/supervisor phases remain an evidence gap. Actions use
their documented streaming/unit/input limits and sandbox resource net,
not the engine's 5 MB ceiling. Resource ceilings are maxima, not p95 gates.

The current `idle_wakeups` counter increments when `poll` returns no ready
descriptor **before both the stage and read deadlines**. Protocol/input
events and deadline expiry are excluded; zero is not a count of all idle
wakeups. Also observe a stationary wheel without user input for a declared
duration, identify any protocol/deadline wakeups and check for autonomous
periodic activity. Confirm no engine process remains after the operation
and its Undo window finish. Instrumentation and externally generated
events must be named, not silently counted as application polling.

## Plan-7 interpretation

ThinkCentre and X280 startup CPU, plus the reported X280 early-list/hover
overruns, remain documented absolute overruns. These environments are
outside the reference timing gate. Their small sequential series do not
establish an accepted p95 or an environment-matched regression guarantee.
The two full SHM paints remain an **open optimization opportunity**, not a
proven unavoidable hardware cost and not an implemented optimization.

The [fresh reference startup series](wheel-reference.md) verify its CPU
p95 with this method; an old p50 or a successful functional test could
not establish that result. Other timing points still require their own
evidence. Policy adoption does not by itself close missing memory, idle,
action-start, regression or other plan-success evidence. Plan 7 integrates
the fuzz/mutation infrastructure, the additional-system measurements and
the reference startup check. Its integration explicitly retains the
remaining performance coverage and automatic-close investigation as
follow-ups; none are represented as passing measurements. The safety and
resource ceilings remain in force. Subsequent plans touching these paths
must satisfy the applicable acceptance gates, with a closing review and
documented evidence.

## Hardening branch

The hardening branch ([sandbox-hardening.md](sandbox-hardening.md),
[hardening-performance.md](hardening-performance.md)) was measured under
this policy. Its status is stated gate by gate there: the ThinkCentre
startup A/B (warm and cold, engine binary only), the supervisor and
Undo-window memory phases, and the memory and idle gates have results on the
final code; the reference desktop A/A and A/B-B/A, the hover maximum on the
reference (36 / 28 us on the real niri; it exceeded 100 us only on the fake
compositor, in the A build too), choice-to-action with real clicks, the
X280 manual series (hover maximum 114 us once, outside the reference
gate) and Plasma Undo and restore are closed there. Open: physical ARM and
the X280 p95 series. None of the open ones is recorded as a pass, and the Plan 7 overrun and the historic ~30 s
automatic-close anomaly stay as they were.
