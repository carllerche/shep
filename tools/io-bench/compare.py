#!/usr/bin/env python3
"""Build and compare production Shep/Redpanda byte appenders on real files."""
from __future__ import annotations
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import tempfile
import time
from datetime import datetime, timezone
from prepare_redpanda import prepare

ROOT = Path(__file__).resolve().parents[2]
PREFIX = "SHEP_IO_BENCH "
GEOMETRY = {"chunk": 65536, "pool": 30 * 1024 * 1024, "preallocate": 2 * 1024 * 1024, "readers": 4}
CORE = [("durable", 1, 97), ("durable", 1, 4096), ("durable", 8, 4096),
        ("durable", 8, 4128), ("stream", 8, 4096), ("mixed", 8, 4096)]


def capture(command: list[str], cwd: Path | None = None) -> str:
    """Run a read-only metadata command and return its complete textual output."""
    return subprocess.check_output(command, cwd=cwd, text=True, stderr=subprocess.STDOUT).strip()


def digest(path: Path) -> str:
    """Identify the exact executable used by an experiment, including dirty builds."""
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def build(args, output: Path) -> tuple[Path, Path]:
    """Install the isolated companion target and locate both optimized executables."""
    prepare(args.redpanda)
    bazel = shutil.which(args.bazel)
    if not bazel:
        raise RuntimeError("Bazelisk is required; install it using Redpanda's README instructions")
    if not args.skip_build:
        print("Building optimized Shep example...", flush=True)
        with (output / "build-shep.log").open("w") as log:
            subprocess.run(["cargo", "build", "--release", "-p", "shep-append", "--example", "io_compare"],
                           cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, check=True)
        print("Building optimized Redpanda companion...", flush=True)
        with (output / "build-redpanda.log").open("w") as log:
            subprocess.run([bazel, "build", "--config=release", f"--jobs={args.build_jobs}",
                            "//src/v/storage/tests:shep_io_bench"], cwd=args.redpanda,
                           stdout=log, stderr=subprocess.STDOUT, check=True)
    # Ask each build system for its output location; do not assume cache layout.
    metadata = json.loads(capture(["cargo", "metadata", "--no-deps", "--format-version", "1"], ROOT))
    shep = Path(metadata["target_directory"]) / "release/examples/io_compare"
    bazel_bin = Path(capture([bazel, "info", "--config=release", "bazel-bin"], args.redpanda).splitlines()[-1])
    redpanda = bazel_bin / "src/v/storage/tests/shep_io_bench"
    for binary in (shep, redpanda):
        if not binary.is_file():
            raise RuntimeError(f"missing benchmark binary: {binary}")
    return shep, redpanda


def environment(args, binaries: tuple[Path, Path]) -> dict:
    """Record source/binary identity, CPU limits, and the filesystem for interpretation."""
    def read(path: str):
        try:
            return Path(path).read_text().strip()
        except OSError:
            return None
    def git(repo: Path):
        return {"head": capture(["git", "rev-parse", "HEAD"], repo),
                "status": capture(["git", "status", "--short"], repo)}
    try:
        filesystem = capture(["findmnt", "-n", "-o", "SOURCE,FSTYPE,TARGET", "--target", str(args.data_root)])
    except (OSError, subprocess.CalledProcessError):
        filesystem = None
    return {
        "created_utc": datetime.now(timezone.utc).isoformat(), "uname": platform.uname()._asdict(),
        "cpu": args.cpu, "allowed_cpus": sorted(os.sched_getaffinity(0)),
        "cpuinfo": read("/proc/cpuinfo"), "cpu_quota": read("/sys/fs/cgroup/cpu.max"),
        "io_limits": read("/sys/fs/cgroup/io.max"), "filesystem": filesystem,
        "data_root": str(args.data_root), "data_device": os.stat(args.data_root).st_dev,
        "io_uring_disabled": read("/proc/sys/kernel/io_uring_disabled"),
        "shep": git(ROOT), "redpanda": git(args.redpanda),
        "binaries": {name: {"path": str(path), "sha256": digest(path)}
                     for name, path in zip(("shep", "redpanda"), binaries)},
        "arguments": {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
        "geometry": GEOMETRY, "redpanda_polling": args.redpanda_polling,
        "protocol": "one pinned CPU; one producer per file; common raw bytes; direct I/O; 4KiB alignment; fdatasync per append except stream; setup/close/verification excluded",
    }


def parse_result(text: str) -> dict:
    """Extract the one machine-readable record while allowing native startup logs."""
    records = [json.loads(line[len(PREFIX):]) for line in text.splitlines() if line.startswith(PREFIX)]
    if len(records) != 1:
        raise RuntimeError(f"expected one benchmark result, found {len(records)}")
    return records[0]


def validate_pair(result: dict, mode: str, groups: int, payload: int, iterations: int, warmup: int):
    """Reject runs whose geometry or durability differs from the common protocol."""
    expected = {"mode": mode, "groups": groups, "payload_bytes": payload, "iterations": iterations, "warmup": warmup,
                "layer": "append-file", "backend": "io_uring", "alignment_bytes": 4096,
                "chunk_bytes": GEOMETRY["chunk"], "pool_bytes": GEOMETRY["pool"],
                "preallocate_bytes": GEOMETRY["preallocate"], "verified": True,
                "fsyncs": groups if mode == "stream" else groups * iterations}
    for key, value in expected.items():
        if result.get(key) != value:
            raise RuntimeError(f"invalid comparison: {key}={result.get(key)!r}, expected {value!r}")
    if result["wall_ns"] <= 0 or result["write_bytes"] < groups * payload * iterations:
        raise RuntimeError("invalid duration or physical write-byte accounting")


def validate_samples(path: Path, result: dict):
    """Cross-check every reported percentile and operation count against raw samples."""
    values = {"append": [], "read": []}
    with path.open() as file:
        for row in csv.DictReader(file):
            value = int(row["latency_ns"])
            if value < 0:
                raise RuntimeError("negative latency sample")
            values[row["operation"]].append(value)
    writes = sorted(values["append"])
    reads = sorted(values["read"])
    if len(writes) != result["groups"] * result["iterations"] or len(reads) != result["reads"]:
        raise RuntimeError("raw sample counts disagree with the workload")
    for field, p, samples in [("p50_ns",500,writes), ("p99_ns",990,writes),
                              ("p999_ns",999,writes), ("read_p99_ns",990,reads)]:
        expected = samples[(len(samples)-1)*p//1000] if samples else 0
        if result[field] != expected:
            raise RuntimeError(f"{field} disagrees with raw operation samples")


def run_one(args, output: Path, binary: Path, engine: str, case: tuple, trial: int) -> dict:
    """Pin one process, validate its output, and preserve raw evidence outside timing."""
    mode, groups, payload = case
    name = f"{mode}-g{groups}-b{payload}-r{trial}-{engine}"
    data = Path(tempfile.mkdtemp(prefix=name + "-", dir=args.data_root))
    common = ["--directory", str(data), "--mode", mode, "--groups", str(groups),
              "--iterations", str(args.iterations), "--warmup", str(args.warmup), "--payload", str(payload)]
    for key, value in GEOMETRY.items():
        common.extend(["--" + key, str(value)])
    command = [str(binary), *common]
    if engine == "redpanda":
        command += ["--smp", "1", "--cpuset", str(args.cpu), "--memory", "1G",
                    "--reserve-memory", "0", "--reactor-backend=io_uring", "--aio-fsync=1",
                    "--default-log-level=warn"]
        if args.redpanda_polling == "parked":
            command += ["--idle-poll-time-us=0", "--poll-aio=0"]
    stdout_path = output / (name + ".stdout.txt")
    stderr_path = output / (name + ".stderr.txt")
    with stdout_path.open("w") as stdout, stderr_path.open("w") as stderr:
        completed = subprocess.run(command, cwd=data, stdout=stdout, stderr=stderr,
                                   timeout=args.timeout, preexec_fn=lambda: os.sched_setaffinity(0, {args.cpu}))
    if completed.returncode:
        raise RuntimeError(f"{name} exited {completed.returncode}; see {stderr_path}; data retained at {data}")
    result = parse_result(stdout_path.read_text())
    validate_pair(result, mode, groups, payload, args.iterations, args.warmup)
    if result["engine"] != engine:
        raise RuntimeError("benchmark executable reported the wrong engine")
    validate_samples(data / "latencies.csv", result)
    shutil.copyfile(data / "latencies.csv", output / (name + ".latencies.csv"))
    result.update({"trial": trial, "command": command, "cpu": args.cpu,
                   "redpanda_polling": args.redpanda_polling, "sample_file": name + ".latencies.csv"})
    payload_total = groups * args.iterations * payload
    result["mib_per_second"] = payload_total / (result["wall_ns"] / 1e9) / (1024 * 1024)
    result["cpu_us_per_append"] = result["cpu_seconds"] * 1e6 / (groups * args.iterations)
    result["cpu_percent"] = result["cpu_seconds"] / (result["wall_ns"] / 1e9) * 100
    result["write_amplification"] = result["write_bytes"] / payload_total
    if args.keep_data:
        result["data_directory"] = str(data)
    else:
        shutil.rmtree(data)
    with (output / "results.jsonl").open("a") as file:
        file.write(json.dumps(result) + "\n")
    print(f"{name}: {result['mib_per_second']:.2f} MiB/s; p99={result['p99_ns']/1e6:.3f} ms; CPU={result['cpu_percent']:.1f}%", flush=True)
    return result


def summarize(results: list[dict], output: Path):
    """Compare per-engine trial medians while retaining dispersion in the CSV."""
    keys = sorted({(r["mode"], r["groups"], r["payload_bytes"]) for r in results})
    rows = []
    for mode, groups, payload in keys:
        for engine in ("shep", "redpanda"):
            runs = [r for r in results if (r["mode"], r["groups"], r["payload_bytes"], r["engine"]) == (mode, groups, payload, engine)]
            if not runs:
                continue
            row = {"mode": mode, "groups": groups, "payload_bytes": payload, "engine": engine, "trials": len(runs)}
            for field in ("mib_per_second", "p50_ns", "p99_ns", "p999_ns", "cpu_us_per_append", "cpu_percent", "write_amplification", "read_p99_ns", "reads"):
                values = [r[field] for r in runs]
                row[field] = statistics.median(values)
                row[field + "_min"] = min(values)
                row[field + "_max"] = max(values)
            rows.append(row)
    with (output / "summary.csv").open("w", newline="") as file:
        writer = csv.DictWriter(file, fieldnames=list(rows[0]))
        writer.writeheader(); writer.writerows(rows)
    lines = ["# Real-file append comparison", "", "Medians across separate process runs. Throughput ratio is Shep / Redpanda; values above 1 favor Shep.",
             "p99 values are medians of per-run p99s, not a pooled percentile. Full ranges and CPU/read/write-amplification metrics are in `summary.csv`.", "",
             "| Mode | Files | Append bytes | Shep MiB/s | Redpanda MiB/s | Ratio | Shep p99 ms | Redpanda p99 ms | Shep CPU µs/append | Redpanda CPU µs/append |",
             "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for key in keys:
        pair = {r["engine"]: r for r in rows if (r["mode"], r["groups"], r["payload_bytes"]) == key}
        if len(pair) != 2:
            continue
        a, b = pair["shep"], pair["redpanda"]
        lines.append(f"| {key[0]} | {key[1]} | {key[2]} | {a['mib_per_second']:.2f} | {b['mib_per_second']:.2f} | {a['mib_per_second']/b['mib_per_second']:.2f}× | {a['p99_ns']/1e6:.3f} | {b['p99_ns']/1e6:.3f} | {a['cpu_us_per_append']:.2f} | {b['cpu_us_per_append']:.2f} |")
    lines += ["", "Both implementations verified every byte and final file length after timing. Exact measured fsync counts were asserted.",
              "Mixed CPU figures include the background readers. Streaming throughput includes final sync; streaming per-operation latency measures buffer acceptance.",
              "", "This measures the byte-appender layer, not full log recovery, record framing, replication, or Kafka throughput."]
    (output / "summary.md").write_text("\n".join(lines) + "\n")


def main():
    """Build once, alternate process order, and execute the reproducible case matrix."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--redpanda", type=Path, default=Path.home() / "redpanda")
    parser.add_argument("--bazel", default="bazel")
    parser.add_argument("--build-jobs", type=int, default=8)
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--data-root", type=Path, default=ROOT / "target/io-bench-data")
    parser.add_argument("--cpu", type=int, default=min(os.sched_getaffinity(0)))
    parser.add_argument("--iterations", type=int, default=1000)
    parser.add_argument("--warmup", type=int, default=64)
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--suite", choices=("core", "full"), default="core")
    parser.add_argument("--case", action="append", help="Override suite: MODE:GROUPS:BYTES, repeatable")
    parser.add_argument("--redpanda-polling", choices=("native", "parked"), default="native")
    parser.add_argument("--timeout", type=float, default=300)
    parser.add_argument("--cooldown", type=float, default=0.25)
    parser.add_argument("--keep-data", action="store_true")
    args = parser.parse_args()
    args.redpanda = args.redpanda.expanduser().resolve(); args.output = args.output.resolve(); args.data_root = args.data_root.resolve()
    if args.cpu not in os.sched_getaffinity(0) or args.iterations < 1 or args.warmup < 0 or args.repetitions < 1:
        parser.error("invalid CPU, iteration, warmup, or repetition count")
    args.output.mkdir(parents=True, exist_ok=True); args.data_root.mkdir(parents=True, exist_ok=True)
    if (args.output / "results.jsonl").exists():
        parser.error("output already contains results; choose a fresh directory")
    binaries = build(args, args.output)
    metadata = environment(args, binaries)
    (args.output / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
    cases = CORE if args.suite == "core" else [(m,g,b) for m in ("durable","stream") for g in (1,8,32) for b in (97,4096,131072)] + [("mixed",8,4096)]
    if args.case:
        cases = [(mode, int(groups), int(size)) for mode, groups, size in (v.split(":") for v in args.case)]
    results = []
    # Reverse engine order on alternating trials/cases to reduce systematic order effects.
    for number, case in enumerate(cases):
        for trial in range(1, args.repetitions + 1):
            order = (0,1) if (number + trial) % 2 else (1,0)
            for index in order:
                engine = ("shep", "redpanda")[index]
                results.append(run_one(args, args.output, binaries[index], engine, case, trial))
                time.sleep(args.cooldown)
    for engine, binary in zip(("shep", "redpanda"), binaries):
        if digest(binary) != metadata["binaries"][engine]["sha256"]:
            raise RuntimeError(f"{engine} binary changed during the experiment")
    summarize(results, args.output)
    print(f"Comparison saved to {args.output / 'summary.md'}", flush=True)

if __name__ == "__main__":
    main()
