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
        rows = self.m.compare(a, b)
        self.assertEqual(len(rows), 1)
        self.assertEqual((rows[0]["metric"], rows[0]["condition"]), ("start", "cold"))
        self.assertAlmostEqual(rows[0]["regression_percent"], 100.0)


if __name__ == "__main__":
    unittest.main()
