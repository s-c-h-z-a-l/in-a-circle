"""Timing acceptance must count every attempt and never pool or auto-pass."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "measure-wheel.py"


def attempt(value, status="valid", metric="start", condition="cold", series="A", unit="us"):
    return {
        "metric": metric,
        "condition": condition,
        "series": series,
        "unit": unit,
        "status": status,
        "value": value if status == "valid" else None,
    }


def group(result, metric="start", condition="cold", series="A"):
    found = [
        g for g in result["groups"]
        if (g["metric"], g["condition"], g["series"]) == (metric, condition, series)
    ]
    assert len(found) == 1, found
    return found[0]


class WheelMeasureTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(SCRIPT.is_file(), "measure-wheel.py is missing")
        spec = importlib.util.spec_from_file_location("measure_wheel", SCRIPT)
        self.m = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.m)

    def test_nearest_rank_values(self):
        self.assertEqual(self.m.nearest_rank(list(range(1, 51)), 95), 48)
        self.assertEqual(self.m.nearest_rank(list(range(1, 101)), 95), 95)
        self.assertEqual(self.m.nearest_rank([7, 3, 5], 95), 7)  # unsorted input
        self.assertEqual(self.m.nearest_rank([5], 50), 5)

    def test_nearest_rank_empty_and_bad_percentile(self):
        self.assertIsNone(self.m.nearest_rank([], 95))
        for bad in (0, 101, -1):
            with self.assertRaises(ValueError):
                self.m.nearest_rank([1, 2, 3], bad)

    def test_49_valid_is_insufficient(self):
        r = self.m.summarize([attempt(v) for v in range(1, 50)])
        g = group(r)
        self.assertFalse(g["sample_gate"])
        self.assertEqual(g["reason"], "insufficient samples")
        self.assertEqual((g["attempts"], g["valid"]), (49, 49))

    def test_timeout_stays_in_ledger(self):
        rows = [attempt(v) for v in range(1, 51)] + [attempt(None, "timeout")]
        g = group(self.m.summarize(rows))
        self.assertEqual(g["attempts"], 51)
        self.assertEqual(g["valid"], 50)
        self.assertEqual(g["non_valid"], {"timeout": 1, "invalid": 0, "missing": 0})
        self.assertTrue(g["sample_gate"])
        self.assertNotIn("accepted", g)  # never an automatic budget PASS

    def test_valid_outlier_is_kept(self):
        rows = [attempt(10) for _ in range(49)] + [attempt(9_000_000)]
        g = group(self.m.summarize(rows))
        self.assertEqual(g["max"], 9_000_000)
        self.assertEqual(g["p95"], 10)  # rank 48 of 50, outlier is rank 50
        self.assertEqual(g["valid"], 50)

    def test_series_and_conditions_not_pooled(self):
        rows = (
            [attempt(v, series="A") for v in range(1, 51)]
            + [attempt(v * 2, series="B") for v in range(1, 51)]
            + [attempt(v, condition="warm") for v in range(1, 31)]
        )
        r = self.m.summarize(rows)
        self.assertEqual(len(r["groups"]), 3)
        self.assertEqual(group(r, series="A")["p95"], 48)
        self.assertEqual(group(r, series="B")["p95"], 96)
        warm = group(r, condition="warm")
        self.assertEqual(warm["valid"], 30)
        self.assertFalse(warm["sample_gate"])

    def test_units_are_not_mixed(self):
        rows = [attempt(1), attempt(2, unit="bytes")]
        with self.assertRaises(ValueError):
            self.m.summarize(rows)

    def test_min_valid_is_a_parameter(self):
        r = self.m.summarize([attempt(v) for v in range(1, 6)], min_valid=5)
        self.assertTrue(group(r)["sample_gate"])

    def test_fifty_sample_fixture_by_hand(self):
        # Values 1..50 in shuffled order: p50 = sorted[24] = 25, p95 = sorted[47] = 48.
        values = list(range(50, 0, -1))  # reversed, so the code must sort
        self.assertEqual(sorted(values), list(range(1, 51)))
        with tempfile.TemporaryDirectory() as tmp:
            src = Path(tmp) / "a.jsonl"
            out = Path(tmp) / "o.json"
            src.write_text("".join(json.dumps(attempt(v)) + "\n" for v in values))
            done = subprocess.run(
                [sys.executable, str(SCRIPT), "summarize", "--attempts", str(src),
                 "--output", str(out)],
                capture_output=True, text=True,
            )
            self.assertEqual(done.returncode, 0, done.stderr)
            g = group(json.loads(out.read_text()))
        self.assertEqual(
            (g["min"], g["p50"], g["p95"], g["max"], g["unit"]), (1, 25, 48, 50, "us")
        )
        self.assertTrue(g["sample_gate"])

    def test_bad_lines_fail_with_line_number(self):
        good = json.dumps(attempt(1))
        bad_lines = [
            "not json",
            json.dumps({**attempt(1), "unit": "ms"}),
            json.dumps({**attempt(1), "status": "ok"}),
            json.dumps({**attempt(1), "value": -1}),
            json.dumps({**attempt(1), "value": 1.5}),
            json.dumps({**attempt(1), "value": True}),
            json.dumps({**attempt(1), "value": None}),
        ]
        for bad in bad_lines:
            with tempfile.TemporaryDirectory() as tmp:
                src = Path(tmp) / "a.jsonl"
                out = Path(tmp) / "o.json"
                src.write_text(good + "\n" + bad + "\n")
                done = subprocess.run(
                    [sys.executable, str(SCRIPT), "summarize", "--attempts", str(src),
                     "--output", str(out)],
                    capture_output=True, text=True,
                )
                self.assertNotEqual(done.returncode, 0, bad)
                self.assertIn("line 2", done.stderr, bad)
                self.assertFalse(out.exists(), bad)

    def test_series_defaults_to_a(self):
        row = attempt(3)
        del row["series"]
        self.assertEqual(self.m.parse_attempt(json.dumps(row), 1)["series"], "A")

    def test_regression(self):
        self.assertAlmostEqual(self.m.regression_percent(100, 112), 12.0)
        self.assertAlmostEqual(self.m.regression_percent(100, 90), -10.0)
        self.assertIsNone(self.m.regression_percent(0, 5))
        self.assertIsNone(self.m.regression_percent(None, 5))
        self.assertIsNone(self.m.regression_percent(5, None))

    def test_compare_matches_metric_and_condition_only(self):
        a = self.m.summarize([attempt(v, series="A") for v in range(1, 51)])
        b = self.m.summarize([attempt(v * 2, series="B") for v in range(1, 51)]
                             + [attempt(v, metric="other") for v in range(1, 51)])
        rows = [r for r in self.m.compare(a, b) if r["comparable"]]
        self.assertEqual(len(rows), 1)
        self.assertEqual((rows[0]["metric"], rows[0]["condition"]), ("start", "cold"))
        self.assertAlmostEqual(rows[0]["regression_percent"], 100.0)

    def test_compare_carries_gate_and_withholds_regression_when_insufficient(self):
        a = self.m.summarize([attempt(v) for v in range(1, 51)])
        b = self.m.summarize([attempt(v * 2) for v in range(1, 6)])
        row = self.m.compare(a, b)[0]
        self.assertTrue(row["a"]["sample_gate"])
        self.assertFalse(row["b"]["sample_gate"])
        self.assertEqual((row["b"]["valid"], row["b"]["attempts"]), (5, 5))
        self.assertIn("non_valid", row["b"])
        self.assertFalse(row["comparable"])
        self.assertIsNone(row["regression_percent"])

    def test_compare_reports_one_sided_pairs(self):
        a = self.m.summarize([attempt(v) for v in range(1, 51)]
                             + [attempt(v, metric="gone") for v in range(1, 51)])
        b = self.m.summarize([attempt(v) for v in range(1, 51)]
                             + [attempt(v, metric="new") for v in range(1, 51)])
        rows = {r["metric"]: r for r in self.m.compare(a, b)}
        self.assertEqual(set(rows), {"start", "gone", "new"})
        self.assertEqual(rows["gone"]["reason"], "missing in B")
        self.assertEqual(rows["new"]["reason"], "missing in A")
        for name in ("gone", "new"):
            self.assertFalse(rows[name]["comparable"])
            self.assertIsNone(rows[name]["regression_percent"])

    def test_cli_rejects_min_valid_below_floor(self):
        for bad in ("0", "-3", "49"):
            with tempfile.TemporaryDirectory() as tmp:
                src = Path(tmp) / "a.jsonl"
                out = Path(tmp) / "o.json"
                src.write_text(json.dumps(attempt(1)) + "\n")
                done = subprocess.run(
                    [sys.executable, str(SCRIPT), "summarize", "--attempts", str(src),
                     "--output", str(out), "--min-valid", bad],
                    capture_output=True, text=True,
                )
                self.assertNotEqual(done.returncode, 0, bad)
                self.assertFalse(out.exists(), bad)
        with self.assertRaises(ValueError):
            self.m.summarize([attempt(1)], min_valid=0)

    def test_failed_attempts_keep_line_status_and_note(self):
        with tempfile.TemporaryDirectory() as tmp:
            src = Path(tmp) / "a.jsonl"
            out = Path(tmp) / "o.json"
            rows = [attempt(1), {**attempt(None, "timeout"), "note": "no commit in 2s"},
                    attempt(2), attempt(None, "missing")]
            src.write_text("".join(json.dumps(r) + "\n" for r in rows))
            done = subprocess.run(
                [sys.executable, str(SCRIPT), "summarize", "--attempts", str(src),
                 "--output", str(out)],
                capture_output=True, text=True,
            )
            self.assertEqual(done.returncode, 0, done.stderr)
            g = group(json.loads(out.read_text()))
        self.assertEqual(g["failures"], [
            {"line": 2, "status": "timeout", "note": "no commit in 2s"},
            {"line": 4, "status": "missing", "note": None},
        ])
        with self.assertRaises(ValueError):
            self.m.parse_attempt(json.dumps({**attempt(1), "note": 5}), 1)

    def test_compare_with_a_zero_baseline_p95_is_undefined(self):
        a = self.m.summarize([attempt(0) for _ in range(50)])
        b = self.m.summarize([attempt(5) for _ in range(50)])
        row = self.m.compare(a, b)[0]
        self.assertTrue(row["a"]["sample_gate"] and row["b"]["sample_gate"])
        self.assertFalse(row["comparable"])
        self.assertIsNone(row["regression_percent"])
        self.assertEqual(row["reason"], "regression undefined")

    def test_compare_accepts_a_summary_without_failures(self):
        a = self.m.summarize([attempt(v) for v in range(1, 51)])
        for g in a["groups"]:
            del g["failures"]
        b = self.m.summarize([attempt(v * 2) for v in range(1, 51)])
        row = self.m.compare(a, b)[0]
        self.assertEqual(row["a"]["failures"], [])
        self.assertTrue(row["comparable"])

    def test_failures_are_bounded_and_the_rest_counted(self):
        n = self.m.FAILURES_KEPT + 7
        rows = [attempt(None, "timeout") for _ in range(n)] + [attempt(1)]
        g = group(self.m.summarize(rows))
        self.assertEqual(len(g["failures"]), self.m.FAILURES_KEPT)
        self.assertEqual(g["failures_dropped"], 7)
        self.assertEqual(g["non_valid"]["timeout"], n)  # the counts stay exact

    def test_an_empty_ledger_is_an_error_not_an_empty_summary(self):
        with tempfile.TemporaryDirectory() as tmp:
            src, out = Path(tmp) / "a.jsonl", Path(tmp) / "o.json"
            src.write_text("\n\n")
            done = subprocess.run(
                [sys.executable, str(SCRIPT), "summarize", "--attempts", str(src),
                 "--output", str(out)], capture_output=True, text=True)
            self.assertEqual(done.returncode, 1)
            self.assertIn("no attempts", done.stderr)
            self.assertFalse(out.exists())

    def test_a_non_utf8_line_names_its_line_number(self):
        with tempfile.TemporaryDirectory() as tmp:
            src = Path(tmp) / "a.jsonl"
            src.write_bytes((json.dumps(attempt(1)) + "\n").encode()
                            + b"\xff\xfe\n" + (json.dumps(attempt(2)) + "\n").encode())
            with self.assertRaisesRegex(ValueError, r"line 2: not UTF-8"):
                self.m.read_attempts(src)


MARKER = (
    "inacircle: measure wheel pid=7 start_abs_cpu_us=1 start_abs_wall_us=2 "
    "main_to_commit_cpu_us=600 main_to_commit_wall_us=1500 main_to_flush_cpu_us=620 "
    "main_to_flush_wall_us=1520 paints=2 busy_paints=0 paint_trace=- hover_events=0 "
    "idle_wakeups=0 events=3 wheel_cpu_us=800 meter_cpu_us=20 drop_to_stage2_cpu_us=-1 "
    "list_to_level1_cpu_us=-1 pss_kb=2800 vm_hwm_kb=4700"
)


class AcceptanceHarnessTests(unittest.TestCase):
    """T13a: byte conversion, markers, memory and idle gates."""

    setUp = WheelMeasureTests.setUp

    def test_kib_to_bytes(self):
        self.assertEqual(self.m.kib_to_bytes(4883), 5_000_192)
        self.assertEqual(self.m.kib_to_bytes(0), 0)

    def test_marker_becomes_attempts_with_bytes(self):
        rows = self.m.marker_attempts(
            MARKER, condition="startup", series="A1", build="A",
            required=("main_to_commit_cpu_us", "pss_bytes"))
        by = {r["metric"]: r for r in rows}
        self.assertEqual(by["main_to_commit_cpu_us"]["value"], 600)
        self.assertEqual(by["pss_bytes"]["value"], 2800 * 1024)
        self.assertEqual(by["pss_bytes"]["unit"], "bytes")
        self.assertEqual(by["pss_bytes"]["build"], "A")
        for r in rows:
            self.m.parse_attempt(json.dumps(r), 1)

    def test_missing_marker_is_missing_evidence_not_skipped(self):
        rows = self.m.marker_attempts(
            None, condition="startup", series="A1", build="A",
            required=("main_to_commit_cpu_us", "pss_bytes"))
        self.assertEqual({r["status"] for r in rows}, {"missing"})
        self.assertEqual(len(rows), 2)

    def test_missing_field_and_minus_one_are_missing(self):
        line = MARKER.replace(" pss_kb=2800", "").replace(
            "main_to_commit_cpu_us=600", "main_to_commit_cpu_us=-1")
        rows = self.m.marker_attempts(
            line, condition="c", series="A", build="A",
            required=("main_to_commit_cpu_us", "pss_bytes", "hover_us_p95"))
        self.assertEqual({r["status"] for r in rows}, {"missing"})

    def test_zero_pss_is_invalid_not_a_pass(self):
        rows = self.m.marker_attempts(
            MARKER.replace("pss_kb=2800", "pss_kb=0"), condition="c", series="A",
            build="A", required=("pss_bytes",))
        self.assertEqual(rows[0]["status"], "invalid")
        self.assertIsNone(rows[0]["value"])

    def test_functional_failure_is_not_silently_dropped(self):
        rows = self.m.marker_attempts(
            MARKER, condition="c", series="A", build="A",
            required=("main_to_commit_cpu_us",), failure="wheel outlived the deadline")
        self.assertEqual(rows[0]["status"], "invalid")
        self.assertIn("outlived", rows[0]["note"])

    def test_summarize_filters_by_build_without_pooling(self):
        rows = [{**attempt(v), "build": "A"} for v in range(1, 51)] + \
               [{**attempt(v * 2), "build": "B"} for v in range(1, 51)]
        parsed = [self.m.parse_attempt(json.dumps(r), i) for i, r in enumerate(rows, 1)]
        self.assertEqual(group(self.m.summarize(parsed, build="A"))["p95"], 48)
        self.assertEqual(group(self.m.summarize(parsed, build="B"))["p95"], 96)
        with self.assertRaises(ValueError):
            self.m.summarize(parsed)  # two builds in one group: pooled

    def _mem(self, kibs, extra=()):
        rows = [attempt(self.m.kib_to_bytes(k), metric="pss_bytes", unit="bytes")
                for k in kibs] + list(extra)
        return self.m.summarize(rows, min_valid=1)

    def test_memory_gate_every_sample_in_bytes(self):
        g = self.m.memory_gate(self._mem([2500, 2500, 4882, 2500]), min_samples=4)
        self.assertEqual(g[0]["verdict"], "PASS")
        g = self.m.memory_gate(self._mem([2500, 2500, 4883, 2500]), min_samples=4)
        self.assertEqual(g[0]["verdict"], "FAIL")  # 5,000,192 B; not 5120 KiB
        self.assertEqual(g[0]["max"], 5_000_192)

    def test_memory_gate_incomplete_cases(self):
        self.assertEqual(self.m.memory_gate(self._mem([2500] * 3), min_samples=4)[0]["verdict"],
                         "INCOMPLETE")
        miss = attempt(None, "missing", metric="pss_bytes", unit="bytes")
        self.assertEqual(self.m.memory_gate(self._mem([2500] * 4, [miss]), min_samples=4)[0]["verdict"],
                         "INCOMPLETE")
        self.assertEqual(self.m.memory_gate(self._mem([0] * 4), min_samples=4)[0]["verdict"],
                         "INCOMPLETE")
        self.assertEqual(self.m.memory_gate({"groups": []}, min_samples=1), [])

    def test_memory_gate_cli_exit_code(self):
        with tempfile.TemporaryDirectory() as tmp:
            summ = Path(tmp) / "s.json"
            out = Path(tmp) / "o.json"
            summ.write_text(json.dumps(self._mem([2500, 6000])))
            done = subprocess.run(
                [sys.executable, str(SCRIPT), "gate-memory", "--summary", str(summ),
                 "--output", str(out), "--min-samples", "2"], capture_output=True, text=True)
            self.assertNotEqual(done.returncode, 0)
            self.assertEqual(json.loads(out.read_text())["gates"][0]["verdict"], "FAIL")

    def test_idle_gate_properties_are_separate(self):
        ok = {"window_s": 2, "premature_empty_polls": 0, "context_switches": 0,
              "resident_processes": 0}
        self.assertEqual(self.m.idle_gate(ok)["verdict"], "PASS")
        for key in ("premature_empty_polls", "context_switches", "resident_processes"):
            bad = {**ok, key: 1}
            r = self.m.idle_gate(bad)
            self.assertEqual(r["verdict"], "FAIL", key)
            self.assertEqual(r["properties"][key], "FAIL")
            self.assertEqual(sum(v == "FAIL" for v in r["properties"].values()), 1)
        short = {k: v for k, v in ok.items() if k != "context_switches"}
        self.assertEqual(self.m.idle_gate(short)["verdict"], "INCOMPLETE")
        self.assertEqual(self.m.idle_gate({**ok, "window_s": 0})["verdict"], "INCOMPLETE")


class AbRunnerTests(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location(
            "measure_ab", SCRIPT.with_name("measure-ab.py"))
        self.ab = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.ab)

    def row(self, marker, failure=None):
        return {"attempt": 3, "utc": "t", "resident_pages": 221, "executable_pages": 221,
                "marker": marker, "failure": failure, "build": "B", "series": "ab1"}

    def test_ledger_row_per_metric_with_build_and_residency(self):
        rows = self.ab.to_ledger(self.row(MARKER), "startup")
        self.assertEqual({r["metric"] for r in rows}, set(self.ab.METRICS))
        self.assertTrue(all(r["build"] == "B" and r["series"] == "ab1" for r in rows))
        self.assertIn("resident 221/221", rows[0]["note"])
        self.assertEqual({r["status"] for r in rows}, {"valid"})

    def test_failed_attempt_is_kept_as_invalid_never_dropped(self):
        rows = self.ab.to_ledger(self.row(None, "wheel alive 5s after Escape"), "startup")
        self.assertEqual(len(rows), len(self.ab.METRICS))
        self.assertEqual({r["status"] for r in rows}, {"invalid"})

    def test_no_input_unless_wheel_verified_alive_and_mapped(self):
        import types
        calls = []
        ab = self.ab

        def fake_run(cmd, **kw):
            calls.append(list(cmd))
            return types.SimpleNamespace(returncode=0, stdout="", stderr="")

        saved = (ab.subprocess.run, ab.pids, ab.time.sleep, ab.residency, ab.MAP_DEADLINE_S)
        ab.subprocess.run, ab.time.sleep = fake_run, lambda s: None
        ab.pids = lambda exe: []  # the wheel never appears
        ab.residency = lambda exe: (0, 0)
        ab.MAP_DEADLINE_S = 0.0
        try:
            with tempfile.TemporaryDirectory() as d:
                build = ab.Build("B", d, d)
                row = ab.attempt(build, "ab1", 1)
        finally:
            ab.subprocess.run, ab.pids, ab.time.sleep, ab.residency, ab.MAP_DEADLINE_S = saved
        self.assertFalse(any(c[0] == "wtype" for c in calls), calls)
        self.assertFalse(row["mapped"])
        self.assertIsNotNone(row["failure"])
        self.assertEqual({r["status"] for r in ab.to_ledger(row, "startup")}, {"invalid"})

    def test_valid_count_requires_every_gated_metric(self):
        ok = self.row(MARKER)
        no_pss = self.row(MARKER.replace("pss_kb=", "pss_kb=0 x="))
        self.assertTrue(self.ab.all_metrics_valid(ok))
        self.assertFalse(self.ab.all_metrics_valid(no_pss))
        self.assertFalse(self.ab.all_metrics_valid(self.row(None)))


if __name__ == "__main__":
    unittest.main()
