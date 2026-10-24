#!/usr/bin/env python3
"""Compare saved Criterion medians (nanoseconds); uses only the standard library."""
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("before")
parser.add_argument("after")
parser.add_argument("--root", type=Path, default=Path("target/criterion"))
args = parser.parse_args()
rows = []
for old_path in args.root.glob(f"**/{args.before}/estimates.json"):
    new_path = old_path.parent.parent / args.after / "estimates.json"
    if not new_path.exists():
        continue
    benchmark = json.loads((old_path.parent / "benchmark.json").read_text())["full_id"]
    old = json.loads(old_path.read_text())["median"]["point_estimate"]
    new = json.loads(new_path.read_text())["median"]["point_estimate"]
    rows.append((benchmark, old, new))
if not rows:
    parser.error("no matching saved benchmarks found")
print("| Benchmark | Before (ns) | After (ns) | Change |")
print("|---|---:|---:|---:|")
for name, old, new in sorted(rows):
    print(f"| {name} | {old:.2f} | {new:.2f} | {(new / old - 1) * 100:+.1f}% |")
