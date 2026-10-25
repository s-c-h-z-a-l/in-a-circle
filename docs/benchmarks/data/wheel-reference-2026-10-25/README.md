# Reference startup evidence, 2026-10-25

Companion data for [Reference wheel startup](../../wheel-reference.md).

- `runs.csv`: **181 attempts**: three complete 50-observation series and
  one interrupted 31-attempt series. All startup markers are retained;
  the interrupted final attempt has `status=invalid` because Esc failed
  to close it, with its marker collected later at the safety deadline.
- `setup.json`: three aborted setup attempts, one successful smoke and
  one unassigned marker. The unassigned marker is an observation, not a
  separately counted attempt. None contribute to acceptance.
- `environment.json`: environment identity, launcher/cache/workload,
  source/build reference and before/after policy/hash readbacks.
- `summary.json`: recalculated per-series statistics. The interrupted
  cohort's descriptive p95 has N=31 and is **not** acceptance.

Times ending in `_us` are µs. PSS/RSS `_kb` fields are Linux KiB. A logged
`-1` means the event did not occur; these plain starts have no drag/choice.
`resident_pages/executable_pages` describe engine **file-cache** residency,
not CPU caches or all libraries. UTC timestamps identify attempt starts.
`escape_hold_ms` is the post-keypress virtual-keyboard connection lifetime.

Status applies to the full controlled scenario: `invalid` does not erase
its available startup observation. Complete-series acceptance uses each
50-attempt series separately. Neither the interrupted cohort nor setup
records are silently filtered into it. There are 180 successful controlled
scenarios and one failed automatic close in `runs.csv`.

Recalculate the acceptance CPU and wall statistics with:

```python
import csv
from pathlib import Path

rows = list(csv.DictReader(Path("runs.csv").open()))
assert len(rows) == 181
for series in ("reference-A1", "reference-A2", "reference-A3"):
    group = [r for r in rows if r["series"] == series]
    assert len(group) == 50 and all(r["status"] == "valid" for r in group)
    for field in ("main_to_commit_cpu_us", "main_to_commit_wall_us"):
        values = sorted(int(r[field]) for r in group)
        n = len(values)
        print(series, field, "N", n, "p50", values[(n-1)//2],
              "p95", values[(95*n+99)//100-1], "max", values[-1])
```

This is nearest-rank `ceil(0.95*N)`, one-based: the 48th observation for
N=50. The report preserves the close-control change, failed scenario,
scope limits and absence of any old/new-code regression experiment.
