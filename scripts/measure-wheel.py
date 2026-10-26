#!/usr/bin/env python3
"""Summarize timing/memory attempts for acceptance (CLAUDE.md Rules, section 2).

Reads explicit JSONL files and writes explicit JSON outputs; it starts no GUI.
The summary checks sample sufficiency only. It never decides that a budget is
met: `sample_gate` says whether enough valid observations exist, nothing more.

Attempt record, one JSON object per line:
  {"metric": str, "condition": str, "series": str (default "A"),
   "unit": "us"|"bytes", "status": "valid"|"timeout"|"invalid"|"missing",
   "value": int (non-negative; required for valid, null otherwise), "note": str?}
"""

import argparse
import json
from pathlib import Path
import sys


# CLAUDE.md Rules 2: timing acceptance needs >= 50 valid observations per
# metric and condition. Below it the group is reported but not sufficient.
MIN_VALID = 50
UNITS = ("us", "bytes")
STATUSES = ("valid", "timeout", "invalid", "missing")
NON_VALID = STATUSES[1:]
DEFAULT_SERIES = "A"


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
    return {
        "metric": record["metric"],
        "condition": record["condition"],
        "series": series,
        "unit": record["unit"],
        "status": record["status"],
        "value": value,
        "note": note,
        "line": line_number,
    }


def read_attempts(path):
    attempts = []
    with open(path, encoding="utf-8") as handle:
        for number, line in enumerate(handle, start=1):
            if line.strip():
                attempts.append(parse_attempt(line, number))
    return attempts


def summarize(attempts, min_valid=MIN_VALID):
    """Group by (metric, condition, series); never pool series or conditions.

    Every non-valid attempt stays in the group's `failures` (line, status,
    note), so the reason survives next to the counts. sample_gate counts valid
    samples only; a large timeout share is visible in `non_valid`, not gated.
    """
    if min_valid < 1:
        raise ValueError(f"min_valid must be >= 1, got {min_valid}")
    grouped = {}
    for item in attempts:
        key = (item["metric"], item["condition"], item.get("series", DEFAULT_SERIES))
        grouped.setdefault(key, []).append(item)
    groups = []
    for (metric, condition, series), items in sorted(grouped.items()):
        units = {item["unit"] for item in items}
        if len(units) != 1:
            raise ValueError(f"{metric}/{condition}/{series}: mixed units {sorted(units)}")
        values = [item["value"] for item in items if item["status"] == "valid"]
        enough = len(values) >= min_valid
        groups.append({
            "metric": metric,
            "condition": condition,
            "series": series,
            "unit": units.pop(),
            "attempts": len(items),
            "valid": len(values),
            "non_valid": {s: sum(1 for i in items if i["status"] == s) for s in NON_VALID},
            "failures": [
                {"line": i.get("line"), "status": i["status"], "note": i.get("note")}
                for i in items if i["status"] != "valid"
            ],
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
    return {key: group[key] for key in
            ("p95", "sample_gate", "valid", "attempts", "non_valid", "failures")}


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
    c = sub.add_parser("compare")
    c.add_argument("--a", required=True, help="summary JSON of the baseline")
    c.add_argument("--b", required=True, help="summary JSON of the candidate")
    c.add_argument("--output", required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "summarize":
            write_json(args.output, summarize(read_attempts(args.attempts), args.min_valid))
        else:
            loaded = [json.loads(Path(p).read_text(encoding="utf-8")) for p in (args.a, args.b)]
            write_json(args.output, {"comparisons": compare(*loaded)})
    except (ValueError, OSError) as error:
        print(f"measure-wheel: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
