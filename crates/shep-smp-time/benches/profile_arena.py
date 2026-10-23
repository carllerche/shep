#!/usr/bin/env python3
"""Gate perf counters around arena_profile's operation, excluding fixture setup."""

import argparse
import json
import os
from pathlib import Path
import select
import statistics
import subprocess
import tempfile


def read_ready(stream):
    if not select.select([stream], [], [], 60)[0]:
        raise RuntimeError("timed out waiting for profile process")
    return stream.readline().strip()


def run(args):
    with tempfile.TemporaryDirectory(prefix="shep-timer-perf-") as directory:
        root = Path(directory)
        control = root / "control"
        ack = root / "ack"
        for path in (control, ack):
            os.mkfifo(path)
        control_fd = os.open(control, os.O_RDWR)
        ack_fd = os.open(ack, os.O_RDWR)
        command = [
            "taskset", "-c", str(args.cpu),
            "perf", "stat", "--delay=-1",
            f"--control=fifo:{control},{ack}",
            "-x", ";", "-o", str(root / "counters.csv"), "-e", args.events,
            "--", str(args.binary.resolve()), args.mode,
            str(args.count), str(args.rounds), str(args.capacity),
        ]
        child = None
        try:
            child = subprocess.Popen(
                command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True,
                env=dict(os.environ, SHEP_PROFILE_SYNC="1"),
            )

            def control_events(instruction):
                os.write(control_fd, instruction.encode() + b"\n")
                if not select.select([ack_fd], [], [], 60)[0]:
                    raise RuntimeError("timed out waiting for perf acknowledgement")
                if os.read(ack_fd, 100).strip(b"\x00\n") != b"ack":
                    raise RuntimeError("unexpected perf acknowledgement")

            if read_ready(child.stdout) != "ready":
                raise RuntimeError("profile process failed to prepare fixtures")
            control_events("enable")
            child.stdin.write("go\n")
            child.stdin.flush()
            if read_ready(child.stdout) != "done":
                raise RuntimeError("profile process did not complete")
            control_events("disable")
            child.stdin.write("exit\n")
            child.stdin.flush()
            stdout, stderr = child.communicate(timeout=60)
            if child.returncode:
                raise RuntimeError(stderr)
            csv = (root / "counters.csv").read_text()
            per_operation = {}
            for line in csv.splitlines():
                fields = line.split(";")
                if len(fields) > 3:
                    try:
                        value = float(fields[0])
                    except ValueError:
                        continue  # Unsupported events remain visible in raw_csv.
                    per_operation[fields[2]] = value / (args.count * args.rounds)
            return dict(per_operation=per_operation, summary=stdout.strip(), raw_csv=csv)
        finally:
            if child is not None and child.poll() is None:
                child.kill()
                child.communicate()
            os.close(control_fd)
            os.close(ack_fd)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("mode", choices=["readiness", "pending", "register", "register_reused"])
    parser.add_argument("count", type=int)
    parser.add_argument("rounds", type=int)
    parser.add_argument("capacity", type=int, nargs="?", default=1024)
    parser.add_argument("--repeat", type=int, default=5)
    parser.add_argument("--cpu", type=int, default=0)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--events",
        default="cycles:u,instructions:u,branches:u,branch-misses:u,page-faults,context-switches",
    )
    args = parser.parse_args()
    if min(args.count, args.rounds, args.repeat) <= 0 or args.capacity < 0:
        parser.error("count, rounds and repeat must be positive; capacity must be nonnegative")
    trials = [run(args) for _ in range(args.repeat)]
    result = dict(
        binary=str(args.binary.resolve()), mode=args.mode, count=args.count,
        rounds=args.rounds, capacity=args.capacity, events=args.events, cpu=args.cpu,
        trials=trials,
        median_per_operation={
            event: statistics.median(trial["per_operation"][event] for trial in trials)
            for event in trials[0]["per_operation"]
        },
    )
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result["median_per_operation"], indent=2))


if __name__ == "__main__":
    main()
