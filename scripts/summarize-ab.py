#!/usr/bin/env python3
"""Tables from a measure-ab.py output directory (attempts.jsonl).

  summarize-ab.py <run-dir> [--json OUT] [--a-label LABEL --b-label LABEL]

Per series and build: attempts, valid, non-valid, p50/p95/max (nearest rank,
measure-wheel.py), then for every `ab*` series the B-against-A p95
regression per metric, for every pair of `aa*` series the A/A p95 spread,
and an order trend (p50 of the first against the second half of each
series, in attempt order). Series are never pooled. A regression is printed
only when both sides have >= 50 valid observations; so is an A/A spread. Which
build is A (the baseline) and which is B (the candidate) comes from the run's
environment.json (`roles`) or --a-label/--b-label, never from the order of the
labels: the sign of a regression must not depend on how the builds were named.
"""
import importlib.util
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("mw", HERE / "measure-wheel.py")
mw = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mw)

MAIN = ("main_to_commit_cpu_us", "main_to_commit_wall_us", "wheel_cpu_us", "pss_bytes")


def trend(values):
    """p50 of the first and second half in attempt order."""
    h = len(values) // 2
    med = lambda v: sorted(v)[(len(v) - 1) // 2] if v else None
    return med(values[:h]), med(values[h:])


def roles(run, argv):
    """(A label, B label): the command line wins, then the run's own record."""
    def opt(name):
        return argv[argv.index(name) + 1] if name in argv else None

    a, b = opt("--a-label"), opt("--b-label")
    if a is None and b is None:
        try:
            rec = json.loads((run / "environment.json").read_text()).get("roles") or {}
        except (OSError, ValueError):
            rec = {}
        a, b = rec.get("a"), rec.get("b")
    return a, b


def main():
    run = Path(sys.argv[1])
    attempts = mw.read_attempts(run / "attempts.jsonl")
    if not attempts:
        sys.exit(f"summarize-ab: {run / 'attempts.jsonl'}: no attempts")
    keys = sorted({(a["series"], a["build"]) for a in attempts})
    out = {"groups": [], "regressions": [], "aa_spread": []}
    by = {}
    for series, build in keys:
        subset = [a for a in attempts if a["series"] == series and a["build"] == build]
        for g in mw.summarize(subset, build=build)["groups"]:
            seq = [a["value"] for a in subset
                   if a["metric"] == g["metric"] and a["status"] == "valid"]
            g["trend_p50_first_second_half"] = trend(seq)
            out["groups"].append(g)
            by[(series, build, g["metric"])] = g
    print("series build metric: attempts valid nonvalid | p50 p95 max | half-p50s")
    for g in out["groups"]:
        if g["metric"] in MAIN:
            print(f"{g['series']:5} {g['build']:14} {g['metric']:24} {g['attempts']:4} {g['valid']:4} "
                  f"{sum(g['non_valid'].values()):3} | {g['p50']} {g['p95']} {g['max']} | "
                  f"{g['trend_p50_first_second_half']}")
    a, b = roles(run, sys.argv)
    for series in sorted({s for s, _ in keys if s.startswith("ab")}):
        present = {bd for s, bd in keys if s == series}
        if len(present) != 2:
            continue
        if a is None or b is None or {a, b} != present:
            sys.exit(f"summarize-ab: series {series} has builds {sorted(present)}; name the "
                     "baseline and the candidate with --a-label and --b-label "
                     "(or use a run directory that records its roles)")
        for m in MAIN:
            ga, gb = by.get((series, a, m)), by.get((series, b, m))
            if ga and gb:
                ok = ga["sample_gate"] and gb["sample_gate"]
                pct = mw.regression_percent(ga["p95"], gb["p95"]) if ok else None
                out["regressions"].append({"series": series, "metric": m, "a": a, "b": b,
                                           "a_p95": ga["p95"], "b_p95": gb["p95"],
                                           "regression_percent": pct, "comparable": ok})
                print(f"{series} {m}: {a} p95 {ga['p95']} vs {b} p95 {gb['p95']} -> "
                      f"{'n/a (insufficient)' if pct is None else f'{pct:+.1f}%'}")
    aas = sorted({s for s, _ in keys if s.startswith("aa")})
    for i, s1 in enumerate(aas):
        for s2 in aas[i + 1:]:
            for m in MAIN:
                g1 = [g for (s, b, mm), g in by.items() if s == s1 and mm == m]
                g2 = [g for (s, b, mm), g in by.items() if s == s2 and mm == m]
                if g1 and g2:
                    ok = g1[0]["sample_gate"] and g2[0]["sample_gate"]
                    # Undefined (None), never skipped and never a number from
                    # too few samples or a zero baseline.
                    pct = mw.regression_percent(g1[0]["p95"], g2[0]["p95"]) if ok else None
                    out["aa_spread"].append({"s1": s1, "s2": s2, "metric": m,
                                             "p95": [g1[0]["p95"], g2[0]["p95"]], "percent": pct,
                                             "comparable": ok})
                    print(f"A/A {s1} vs {s2} {m}: p95 {g1[0]['p95']} vs {g2[0]['p95']} "
                          f"({'n/a' if pct is None else f'{pct:+.1f}%'})")
    if "--json" in sys.argv:
        Path(sys.argv[sys.argv.index("--json") + 1]).write_text(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
