#!/usr/bin/env python3
"""Summarize timing/memory attempts for acceptance (CLAUDE.md Rules, section 2).

Reads explicit JSONL files and writes explicit JSON outputs; it starts no GUI.
The summary checks sample sufficiency only. It never decides that a budget is
met: `sample_gate` says whether enough valid observations exist, nothing more.

Attempt record, one JSON object per line:
  {"metric": str, "condition": str, "series": str (default "A"),
   "unit": "us"|"bytes", "status": "valid"|"timeout"|"invalid"|"missing",
   "value": int (non-negative; required for valid, null otherwise), "note": str?,
   "build": str? (A/B build label; summarize --build selects one, never pools)}

Acceptance helpers: kib_to_bytes, marker_attempts (a `measure wheel`
line to attempts), memory_gate (every sample against 5,000,000 B) and
idle_gate (idle properties kept apart). Verdicts are PASS, FAIL or
INCOMPLETE; missing or zero evidence is never a PASS.
"""

import argparse
import json
import re
from pathlib import Path
import sys


# CLAUDE.md Rules 2: timing acceptance needs >= 50 valid observations per
# metric and condition. Below it the group is reported but not sufficient.
MIN_VALID = 50
# Failure records kept per group. The counts in `non_valid` stay exact; the
# list is for diagnosis, and 50 lines (one acceptance sample's worth) name
# the reasons of a failing series without a mostly-failing run of
# thousands bloating the summary. `failures_dropped` says how many more.
FAILURES_KEPT = 50
UNITS = ("us", "bytes", "count")
STATUSES = ("valid", "timeout", "invalid", "missing")
NON_VALID = STATUSES[1:]
DEFAULT_SERIES = "A"
# Engine memory ceiling, docs/benchmarks/acceptance.md: 5 MB = 5,000,000 B
# of PSS. Kernel "kB" in smaps_rollup are KiB: multiply by 1024 first.
MEMORY_CEILING_BYTES = 5_000_000


def nearest_rank(samples, percentile):
    """Nearest-rank percentile: sorted sample ceil(P*N/100), one-based.

    Integer arithmetic only: ceil(P*N/100) == (P*N + 99) // 100. The 99 is
    100 - 1, the rounding term for a division by 100. Empty input gives None.
    """
    if not 1 <= percentile <= 100:
        raise ValueError(f"percentile must be in 1..100, got {percentile}")
    if not samples:
        return None
    ordered = sorted(samples)
    rank = (percentile * len(ordered) + 99) // 100
    return ordered[rank - 1]


def parse_attempt(text, line_number):
    """Validate one JSONL line; raise ValueError naming the line on any defect."""

    def fail(why):
        raise ValueError(f"line {line_number}: {why}")

    try:
        record = json.loads(text)
    except json.JSONDecodeError as error:
        fail(f"invalid JSON ({error})")
    if not isinstance(record, dict):
        fail("record is not a JSON object")
    for key in ("metric", "condition"):
        if not isinstance(record.get(key), str) or not record[key]:
            fail(f"{key} must be a non-empty string")
    series = record.get("series", DEFAULT_SERIES)
    if not isinstance(series, str) or not series:
        fail("series must be a non-empty string")
    if record.get("unit") not in UNITS:
        fail(f"unit must be one of {UNITS}")
    if record.get("status") not in STATUSES:
        fail(f"status must be one of {STATUSES}")
    value = record.get("value")
    if value is not None and (type(value) is not int or value < 0):
        fail("value must be a non-negative integer or null")
    if record["status"] == "valid" and value is None:
        fail("a valid record needs a value")
    if record["status"] != "valid" and value is not None:
        fail("only a valid record may carry a value")
    note = record.get("note")
    if note is not None and not isinstance(note, str):
        fail("note must be a string")
    build = record.get("build")
    if build is not None and (not isinstance(build, str) or not build):
        fail("build must be a non-empty string")
    return {
        "metric": record["metric"],
        "condition": record["condition"],
        "series": series,
        "build": build,
        "unit": record["unit"],
        "status": record["status"],
        "value": value,
        "note": note,
        "line": line_number,
    }


def read_attempts(path):
    """Every attempt of a JSONL ledger. A line that is not UTF-8 is a
    ValueError naming its line (a text-mode read would raise a
    UnicodeDecodeError with no line number)."""
    attempts = []
    with open(path, "rb") as handle:
        for number, raw in enumerate(handle, start=1):
            try:
                line = raw.decode("utf-8")
            except UnicodeDecodeError as error:
                raise ValueError(f"line {number}: not UTF-8 ({error.reason})") from error
            if line.strip():
                attempts.append(parse_attempt(line, number))
    return attempts


def summarize(attempts, min_valid=MIN_VALID, build=None):
    """Group by (metric, condition, series); never pool series or conditions.

    Every non-valid attempt stays in the group's `failures` (line, status,
    note), so the reason survives next to the counts. sample_gate counts valid
    samples only; a large timeout share is visible in `non_valid`, not gated.
    """
    if min_valid < 1:
        raise ValueError(f"min_valid must be >= 1, got {min_valid}")
    if build is not None:
        attempts = [a for a in attempts if a.get("build") == build]
    grouped = {}
    for item in attempts:
        key = (item["metric"], item["condition"], item.get("series", DEFAULT_SERIES))
        grouped.setdefault(key, []).append(item)
    groups = []
    for (metric, condition, series), items in sorted(grouped.items()):
        builds = {item.get("build") for item in items}
        if len(builds) != 1:
            raise ValueError(f"{metric}/{condition}/{series}: builds {sorted(map(str, builds))} "
                             "would be pooled; select one with build=")
        units = {item["unit"] for item in items}
        if len(units) != 1:
            raise ValueError(f"{metric}/{condition}/{series}: mixed units {sorted(units)}")
        values = [item["value"] for item in items if item["status"] == "valid"]
        failures = [
            {"line": i.get("line"), "status": i["status"], "note": i.get("note")}
            for i in items if i["status"] != "valid"
        ]
        enough = len(values) >= min_valid
        groups.append({
            "metric": metric,
            "condition": condition,
            "series": series,
            "unit": units.pop(),
            "build": builds.pop(),
            "attempts": len(items),
            "valid": len(values),
            "non_valid": {s: sum(1 for i in items if i["status"] == s) for s in NON_VALID},
            "failures": failures[:FAILURES_KEPT],
            "failures_dropped": max(0, len(failures) - FAILURES_KEPT),
            "min": min(values) if values else None,
            "p50": sorted(values)[(len(values) - 1) // 2] if values else None,  # lower-middle
            "p95": nearest_rank(values, 95),
            "max": max(values) if values else None,
            "sample_gate": enough,
            "reason": "enough valid samples (budget not evaluated)" if enough
            else "insufficient samples",
        })
    return {"min_valid": min_valid, "groups": groups}


def regression_percent(a_p95, b_p95):
    """100 * (B/A - 1); undefined (None) when A is 0 or either side is missing."""
    if a_p95 is None or b_p95 is None or a_p95 == 0:
        return None
    return 100 * (b_p95 / a_p95 - 1)


def _side(group):
    """The evidence behind one p95: its gate and the full attempt counts."""
    side = {key: group[key] for key in ("p95", "sample_gate", "valid", "attempts", "non_valid")}
    # A summary written before `failures` existed has none: empty, not a crash.
    side["failures"] = group.get("failures", [])
    return side


def compare(a_summary, b_summary):
    """Regression of B's p95 against A's, matched by (metric, condition) only.

    Every pair on either side gets a row. A regression is given only when both
    sides pass sample_gate and A's p95 is non-zero; otherwise `comparable` is
    false, `regression_percent` is None and `reason` says why.
    """

    def index(summary, name):
        found = {}
        for g in summary["groups"]:
            key = (g["metric"], g["condition"])
            if key in found:
                raise ValueError(f"{name}: several series for {key}; split the input")
            found[key] = g
        return found

    a, b = index(a_summary, "A"), index(b_summary, "B")
    rows = []
    for key in sorted(a.keys() | b.keys()):
        row = {"metric": key[0], "condition": key[1], "unit": None,
               "a": None, "b": None, "a_p95": None, "b_p95": None,
               "comparable": False, "regression_percent": None, "reason": None}
        if key not in a or key not in b:
            row["reason"] = "missing in A" if key not in a else "missing in B"
        elif a[key]["unit"] != b[key]["unit"]:
            raise ValueError(f"unit mismatch for {key}")
        elif not (a[key]["sample_gate"] and b[key]["sample_gate"]):
            row["reason"] = "insufficient samples"
        else:
            row["regression_percent"] = regression_percent(a[key]["p95"], b[key]["p95"])
            row["comparable"] = row["regression_percent"] is not None
            row["reason"] = "ok" if row["comparable"] else "regression undefined"
        for side, groups in (("a", a), ("b", b)):
            if key in groups:
                row["unit"] = groups[key]["unit"]
                row[side] = _side(groups[key])
                row[side + "_p95"] = groups[key]["p95"]
        rows.append(row)
    return rows


def kib_to_bytes(kib):
    """smaps_rollup / VmHWM 'kB' are KiB."""
    return kib * 1024


def marker_attempts(line, *, condition, series, build, required, note=None, failure=None):
    """Attempts from one `measure wheel` line, one per required metric.

    A required metric absent from the line, or reported as -1 (never taken),
    is `missing`; a line of None (no marker at all) makes every one missing.
    `pss_bytes` comes from `pss_kb` (KiB -> B); 0 is `invalid`, not a value.
    A functional `failure` (crash, hang, non-zero exit) makes every row
    `invalid` with that reason: the run is kept, never silently skipped.
    """
    fields = {}
    if line is not None:
        for key, value in re.findall(r"(\w+)=(-?\d+)", line):
            fields[key] = int(value)
        if "pss_kb" in fields:
            fields["pss_bytes"] = kib_to_bytes(fields["pss_kb"])
    rows = []
    for metric in required:
        value = fields.get(metric)
        unit = "bytes" if metric.endswith("_bytes") else "count" if metric in COUNT_METRICS else "us"
        if failure is not None:
            status, why = "invalid", f"functional failure: {failure}"
        elif line is None:
            status, why = "missing", "no measure line"
        elif value is None:
            status, why = "missing", "field absent from the marker"
        elif value < 0:
            status, why = "missing", "marker reports -1 (never taken)"
        elif value == 0 and metric == "pss_bytes":
            status, why = "invalid", "PSS 0 is not a measurement"
        else:
            status, why = "valid", None
        rows.append({
            "metric": metric, "condition": condition, "series": series, "build": build,
            "unit": unit, "status": status,
            "value": value if status == "valid" else None,
            "note": "; ".join(x for x in (why, note) if x) or None,
        })
    return rows


COUNT_METRICS = ("idle_wakeups", "events", "paints", "busy_paints", "hover_events")


def memory_gate(summary, limit=MEMORY_CEILING_BYTES, min_samples=1):
    """Verdict per byte-unit group of a summary: the maximum decides.

    FAIL: any valid sample above `limit` (a ceiling, not a p95).
    INCOMPLETE: fewer than `min_samples` valid, any non-valid attempt, or a
    zero maximum. PASS only when none of those and all samples <= limit.
    FAIL wins over INCOMPLETE: an observed breach does not need more data.
    """
    out = []
    for g in summary["groups"]:
        if g["unit"] != "bytes":
            continue
        non_valid = sum(g["non_valid"].values())
        if g["max"] is not None and g["max"] > limit:
            verdict, why = "FAIL", f"max {g['max']} B > {limit} B"
        elif g["valid"] < min_samples:
            verdict, why = "INCOMPLETE", f"{g['valid']} valid < {min_samples}"
        elif non_valid:
            verdict, why = "INCOMPLETE", f"{non_valid} non-valid attempt(s)"
        elif not g["max"]:
            verdict, why = "INCOMPLETE", "maximum is 0: not a measurement"
        else:
            verdict, why = "PASS", f"max {g['max']} B <= {limit} B"
        out.append({"metric": g["metric"], "condition": g["condition"],
                    "series": g["series"], "build": g.get("build"), "valid": g["valid"],
                    "max": g["max"], "limit": limit, "verdict": verdict, "reason": why})
    return out


IDLE_PROPERTIES = ("premature_empty_polls", "context_switches", "resident_processes")


def idle_gate(observation):
    """Idle properties one by one for a stationary-wheel observation.

    premature_empty_polls: the wheel's own counter (poll returned nothing
    before a deadline); context_switches: external, over `window_s` seconds
    (any autonomous periodic work); resident_processes: engine/children
    still alive after the operation. Each must be 0. A missing key or a
    window of 0 s is INCOMPLETE; protocol events and deadline expiry are
    named by the caller and are not counted here.
    """
    props = {}
    for key in IDLE_PROPERTIES:
        value = observation.get(key)
        props[key] = "INCOMPLETE" if value is None else "PASS" if value == 0 else "FAIL"
    window = observation.get("window_s")
    if "FAIL" in props.values():
        verdict = "FAIL"
    elif "INCOMPLETE" in props.values() or not window:
        verdict = "INCOMPLETE"
    else:
        verdict = "PASS"
    return {"verdict": verdict, "properties": props, "window_s": window}


def write_json(path, data):
    Path(path).write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


def min_valid_arg(text):
    # The CLI may only tighten the CLAUDE.md floor, never relax it.
    value = int(text)
    if value < MIN_VALID:
        raise argparse.ArgumentTypeError(f"must be >= {MIN_VALID} (acceptance floor)")
    return value


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    s = sub.add_parser("summarize")
    s.add_argument("--attempts", required=True)
    s.add_argument("--output", required=True)
    s.add_argument("--min-valid", type=min_valid_arg, default=MIN_VALID)
    s.add_argument("--build", help="only attempts of this build label")
    g = sub.add_parser("gate-memory")
    g.add_argument("--summary", required=True)
    g.add_argument("--output", required=True)
    g.add_argument("--min-samples", type=int, default=1)
    c = sub.add_parser("compare")
    c.add_argument("--a", required=True, help="summary JSON of the baseline")
    c.add_argument("--b", required=True, help="summary JSON of the candidate")
    c.add_argument("--output", required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "summarize":
            attempts = read_attempts(args.attempts)
            if not attempts:
                # An empty ledger is no evidence, never a summary that
                # looks like one.
                raise ValueError(f"{args.attempts}: no attempts")
            write_json(args.output, summarize(attempts, args.min_valid, args.build))
        elif args.command == "gate-memory":
            summary = json.loads(Path(args.summary).read_text(encoding="utf-8"))
            gates = memory_gate(summary, min_samples=args.min_samples)
            write_json(args.output, {"gates": gates})
            return 0 if gates and all(x["verdict"] == "PASS" for x in gates) else 1
        else:
            loaded = [json.loads(Path(p).read_text(encoding="utf-8")) for p in (args.a, args.b)]
            write_json(args.output, {"comparisons": compare(*loaded)})
    except (ValueError, OSError) as error:
        print(f"measure-wheel: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
