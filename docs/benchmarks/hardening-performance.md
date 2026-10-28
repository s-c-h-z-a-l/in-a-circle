# Hardening performance acceptance (T13a)

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
  path changes), sha256 `692a0e5d...27c7`. B = the hardening release at
  `b696450` (sha256 `530de180...1019`; later commits changed tests and
  scripts only). Same profile (fat LTO, `panic=abort`, stripped) and the
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
- The improvement is the commit-first change (no obsolete full paint before
  the first commit); the second buffer is painted after the first flush.
  Total wheel CPU moves less than the commit interval.
- ThinkCentre absolute CPU (about 2-3.6 ms) is outside the reference gate
  by policy ([acceptance.md](acceptance.md), Plan-7 interpretation); only
  the environment-matched regression applies, and there is none.

## Open

| Item | State |
|---|---|
| Desktop (reference) A/A and A/B-B/A, early-list / drop / choice / hover | OPEN: T13b, owner present (`scripts/measure-desktop-t13b.sh`) |
| X280 Plasma A/B | OPEN: machine unavailable; not a pass, not a baseline reset |
| ThinkCentre cold condition, early-list, drop, choice, hover workloads | OPEN: only the plain warm startup ran |
| ThinkCentre with a real output | OPEN: nested niri only (no monitor attached) |
| Physical ARM desktop performance | OPEN; ARM VM results are emulated, functional only |
| Engine/supervisor and Undo-window memory phases | OPEN (evidence gap) |

Reproduce: copy two release directories and the two scripts to the target,
inside a niri session run
`scripts/measure-ab.py --out DIR --a-bin A --b-bin B --plan aa,aa,ab,ab`,
then `scripts/summarize-ab.py DIR`.
