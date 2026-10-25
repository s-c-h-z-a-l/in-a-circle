# Frozen plan-7 startup probes

`plan7-phases.patch` records the exact instrumentation used for the
[2026-10-25 wheel portability measurements](../../docs/benchmarks/wheel-portability.md).
It applies to `3a1857bb435874599ef7245fa9df8b3fd1b321d8`, not arbitrary
future engine code. It is an experimental reproduction artifact; the
normal workspace never builds or links it.

The patch adds process user/system CPU, monotonic timestamps and
minor/major-fault counters at wheel setup boundaries. Checkpoints append
to an in-memory vector; output occurs after the surface closes. Set
`INACIRCLE_PHASE_PROFILE=1` to enable them, alongside `INACIRCLE_MEASURE=1`.
The same patched binary without the phase switch is the probe-off control.
Probe-on/off/original must be kept as separate series.

Build in a temporary source copy, keeping the product checkout intact:

```sh
project_dir=$PWD
profile_dir=$(mktemp -d)
git archive 3a1857bb435874599ef7245fa9df8b3fd1b321d8 | tar -x -C "$profile_dir"
cd "$profile_dir"
git apply --unidiff-zero "$project_dir/spikes/wheel-profile/plan7-phases.patch"
cargo build --release -p inacircle
```

Use Rust 1.98.1 and the pinned lockfile. The measured build settings and
hashes are in [build.json](../../docs/benchmarks/data/wheel-2026-10-25/build.json).
Copy the executable to the target machine separately from the normal
engine, and use the same text-family action directory and primed catalog.
Run under the real unlocked Wayland session, collect both `measure wheel`
and `profile label=...` lines, then close with Esc. A missing/negative
commit record is a failed measurement. Never overwrite the normal launcher
with this probe build.

For the phase comparison, use five rounds of original/off/on variants,
rotating which variant starts each round. The recorded collector primed
the selected executable's file pages, checked `mincore`, waited 0.5 s,
launched a fresh process, sent Esc after 1 s and allowed 5 s for exit.
Twenty baseline starts follow the existing wheel method; the five probe
rounds are a bounded diagnostic sample, not a distribution guarantee.
The 1 s Esc delay exceeds normal configure/commit time; the 5 s exit
guard detects a stuck overlay well before its own 30 s backstop. The
0.5 s pause standardizes the interval but does not prove a cold CPU.
The trace reserves 32 entries for its 15 checkpoints to avoid vector
reallocation during startup; this is capacity, not an input limit.

`wait4` supplied whole-child CPU, faults and context switches. The full
phase/whole-child records are in the companion CSVs.

For the cache comparison, alternate the order of five cold/warm pairs.
Read the executable for both conditions; for cold only, additionally call
`posix_fadvise(..., POSIX_FADV_DONTNEED)`. Check residency with `mincore`
before the 0.5 s pause. Do not drop global caches or call this a cold-boot
test. Shared libraries and fresh SHM pages are different factors. The
X280 cache/probe calibration also used a temporary 100% `intel_pstate`
minimum; that requests a high floor, not an exact fixed frequency.
Record the original policy, restore it in a `finally`/trap path and verify
it afterwards.

Cumulative `profile` counters are differenced between adjacent labels.
`first_commit` is a queued request, not presentation. `surface_gone` is
after connection/mapping teardown, while `reported` also includes normal
measurement-report work. The later phase-dump cost is outside the recorded
startup. The checkpoint's CPU and wall counters are sampled separately;
very short differences and the probe calibration must be interpreted
with the limitations in the report.
