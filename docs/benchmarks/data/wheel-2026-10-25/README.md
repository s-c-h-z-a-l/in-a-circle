# Wheel measurements, 2026-10-25

Companion evidence for [The wheel on niri and Plasma](../../wheel-portability.md).
These are sanitized measurement records, not generated benchmark conclusions.
No credentials, network addresses, user paths, device serials or process logs
are included. `tc` identifies the ThinkCentre; `x280` identifies the ThinkPad.

## Files

- `machines.json`: model/firmware, CPU, memory, storage, kernel, runtime,
  display and policy specifications. Missing SMBIOS fields are explicit.
- `build.json`: source revisions, compiler/profile, executable sizes/hashes
  and dynamic runtime dependencies. The probe executable is a separate build.
- `runs.csv`: 185 run records. Original logger fields are preserved as
  integers; metadata identifies method, revision/control, cache and variant.
- `phase-points.csv`: 525 cumulative probe records, keyed by `series,sample`.
- `manual-jobs.csv`: eight jobs, with output counts, recorded sandbox state
  and Undo evidence. `not_invoked` is not an Undo failure.
- `render.csv`: reported integer-microsecond results from one invocation of
  the same existing UI benchmark on each of three machines. The benchmark
  truncates nanoseconds when printing these values.

## Series

| `series` | Runs | Method |
|---|---:|---|
| `tc-plan7-ssh` | 20 | original plan 7, fresh SSH connection for each start |
| `tc-main-ssh` | 20 | earlier main `2ba2040`, same SSH method, later sequential series |
| `x280-plan7-ssh` | 20 | original plan 7, repeated local subprocesses within one SSH connection |
| `x280-plan7-hotkey` | 20 | original plan 7, actual Plasma Ctrl+Meta+F3 application shortcut |
| `tc-plan7-manual` | 10 | mixed operator-driven wheel tests, including the Undo retest |
| `x280-plan7-manual` | 10 | mixed operator-driven wheel tests |
| `x280-minimum-control` | 20 | ten AB/BA pairs, original binary, 11% versus 100% minimum policy |
| `tc-diagnostic-phase` | 15 | rotating original/probe-off/probe-on variants, five each |
| `x280-diagnostic-phase` | 15 | same variant rotation at baseline policy |
| `x280-diagnostic-phase_locked` | 15 | same variant rotation with minimum policy 100% |
| `tc-diagnostic-cache` | 10 | five cold/warm executable-cache pairs, baseline policy |
| `x280-diagnostic-cache` | 10 | five cold/warm pairs, minimum policy 100% |

The historical label `phase_locked` means a **minimum-performance policy
request**, not a hardware-verified fixed clock. `original` is the normal
plan-7 binary; `profile_off` and `profile_on` use the separate probe build.
The single shortcut-setup smoke was not included in the 20-run hotkey series.

## Units, missing values and calculation

- Times ending in `_us`: microseconds. Parent/probe monotonic timestamps
  ending in `_ns`: nanoseconds on that machine's monotonic clock.
- `pss_kb` and `vm_hwm_kb`: Linux KiB, despite the logger's field spelling.
- CPU is user + system time. Hover values are wall time.
- Empty CSV cells mean the field was not collected for that series.
  A logged `-1` means its event/measurement never occurred.
- `child_*` fields cover the whole reaped child, including work before
  wheel-mode entry and after its surface closed. Fault/context-switch
  values are counts, not durations.
- `resident_pages_before/executable_pages` are `mincore` results sampled
  before the final pre-launch pause; these fields were not sampled in the
  original minimum-policy control or original plain-start series.
- Unless a paired effect is specified, p50 is `sorted(values)[(n-1)//2]`.
  Probe tables use medians of five per-phase differences. Compute combined
  paint time or a paint fraction per run first, then take its median.
- Probe `user_us/sys_us/minflt/majflt/mono_ns` values are cumulative, not
  per-phase costs. Subtract consecutive points in recorded order. Do not
  sum marginal phase medians to reconstruct a run.
- In cache comparisons, calculate cold-minus-warm within each `round`
  before taking the median. This differs from subtracting the two
  marginal medians.
- Do not pool launchers, original/probe binaries, power policies, cache
  conditions or the mixed manual workloads. Do not treat medians of
  per-run hover medians as a pooled event median.

For example, this independently checks one published startup row:

```python
import csv
from pathlib import Path

rows = list(csv.DictReader(Path("runs.csv").open()))
values = sorted(int(r["main_to_commit_cpu_us"]) for r in rows
                if r["series"] == "x280-plan7-hotkey")
assert len(values) == 20
assert (values[9], values[-1]) == (1518, 2813)
assert sum(v > 1000 for v in values) == 20
```

The [frozen probe patch](../../../../spikes/wheel-profile/) gives the exact
checkpoint locations and measurement code. It does not add a shipped
feature or change the normal build.
