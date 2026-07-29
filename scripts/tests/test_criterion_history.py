import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).parents[1] / "criterion_history.py"


class CriterionHistoryTests(unittest.TestCase):
    def test_summary_and_comparison_use_median_point_estimates(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            estimate = root / "criterion" / "group" / "case" / "new" / "estimates.json"
            estimate.parent.mkdir(parents=True)
            estimate.write_text(
                json.dumps({"median": {"point_estimate": 120.0}}), encoding="utf-8"
            )
            current = root / "current.json"
            subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "summarize",
                    str(root / "criterion"),
                    str(current),
                    "--commit",
                    "current",
                ],
                check=True,
            )
            summary = json.loads(current.read_text(encoding="utf-8"))
            self.assertEqual(summary["benchmarks"], {"group/case": 120.0})

            baseline = root / "baseline.json"
            baseline.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "commit": "base",
                        "benchmarks": {"group/case": 90.0},
                    }
                ),
                encoding="utf-8",
            )
            markdown = root / "comparison.md"
            completed = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "compare",
                    str(baseline),
                    str(current),
                    "--threshold-percent",
                    "20",
                    "--markdown",
                    str(markdown),
                ],
                check=True,
                capture_output=True,
                text=True,
            )
            self.assertIn("Performance regression", completed.stdout)
            self.assertIn("+33.3%", markdown.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
