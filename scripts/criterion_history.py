#!/usr/bin/env python3
"""Create and compare stable summaries of Criterion benchmark results."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import sys


def collect(root: Path) -> dict[str, float]:
    benchmarks: dict[str, float] = {}
    for path in sorted(root.glob("**/new/estimates.json")):
        data = json.loads(path.read_text(encoding="utf-8"))
        relative = path.relative_to(root)
        name = "/".join(relative.parts[:-2])
        benchmarks[name] = float(data["median"]["point_estimate"])
    if not benchmarks:
        raise ValueError(f"no Criterion estimates found under {root}")
    return benchmarks


def summarize(args: argparse.Namespace) -> int:
    document = {
        "schema_version": 1,
        "commit": args.commit,
        "runner": args.runner,
        "benchmarks": collect(args.criterion_dir),
    }
    args.output.write_text(
        json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return 0


def load(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def compare(args: argparse.Namespace) -> int:
    baseline = load(args.baseline)
    current = load(args.current)
    base_values = baseline["benchmarks"]
    current_values = current["benchmarks"]
    rows: list[str] = []
    regressions: list[tuple[str, float]] = []
    for name in sorted(current_values.keys() & base_values.keys()):
        before = float(base_values[name])
        after = float(current_values[name])
        delta = ((after / before) - 1.0) * 100.0 if before else 0.0
        verdict = "regression" if delta > args.threshold_percent else "ok"
        if verdict == "regression":
            regressions.append((name, delta))
        rows.append(
            f"| `{name}` | {before:,.0f} | {after:,.0f} | {delta:+.1f}% | {verdict} |"
        )

    missing = sorted(base_values.keys() - current_values.keys())
    added = sorted(current_values.keys() - base_values.keys())
    markdown = [
        "### Criterion comparison",
        "",
        f"Baseline `{baseline.get('commit', 'unknown')}` → current `{current.get('commit', 'unknown')}`; alert threshold: +{args.threshold_percent:g}%.",
        "",
        "| Benchmark | Baseline median (ns) | Current median (ns) | Change | Verdict |",
        "| --- | ---: | ---: | ---: | --- |",
        *rows,
    ]
    if added:
        markdown.extend(["", "New benchmarks: " + ", ".join(f"`{name}`" for name in added)])
    if missing:
        markdown.extend(
            ["", "Missing benchmarks: " + ", ".join(f"`{name}`" for name in missing)]
        )
    args.markdown.write_text("\n".join(markdown) + "\n", encoding="utf-8")

    for name, delta in regressions:
        print(
            f"::warning title=Performance regression::{name} median increased by {delta:.1f}% "
            f"(threshold {args.threshold_percent:g}%)"
        )
    print(f"Compared {len(rows)} benchmark(s); {len(regressions)} regression alert(s).")
    return 0


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)

    summary = commands.add_parser("summarize")
    summary.add_argument("criterion_dir", type=Path)
    summary.add_argument("output", type=Path)
    summary.add_argument("--commit", default=os.environ.get("GITHUB_SHA", "local"))
    summary.add_argument("--runner", default=os.environ.get("RUNNER_OS", sys.platform))
    summary.set_defaults(func=summarize)

    comparison = commands.add_parser("compare")
    comparison.add_argument("baseline", type=Path)
    comparison.add_argument("current", type=Path)
    comparison.add_argument("--threshold-percent", type=float, default=20.0)
    comparison.add_argument("--markdown", type=Path, required=True)
    comparison.set_defaults(func=compare)
    return result


def main() -> int:
    args = parser().parse_args()
    try:
        return args.func(args)
    except (KeyError, OSError, TypeError, ValueError, json.JSONDecodeError) as error:
        print(f"criterion history error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
