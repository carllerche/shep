#!/usr/bin/env python3
"""Install only the benchmark source and BUILD target into a Redpanda checkout."""
from pathlib import Path
import argparse
import shutil

BEGIN = "# BEGIN Shep real-file I/O benchmark"
END = "# END Shep real-file I/O benchmark"
LOAD = 'load("//bazel:build.bzl", "redpanda_cc_binary")'
TARGET = '''redpanda_cc_binary(
    name = "shep_io_bench",
    srcs = ["shep_io_bench.cc"],
    deps = [
        "//src/v/base",
        "//src/v/config",
        "//src/v/resource_mgmt:memory_groups",
        "//src/v/storage:chunk_cache",
        "//src/v/storage:resources",
        "//src/v/storage:segment_appender",
        "@boost//:program_options",
        "@fmt",
        "@seastar",
    ],
)
'''

def prepare(repo: Path) -> None:
    """Update our marked benchmark target without changing production sources."""
    package = repo / "src/v/storage/tests"
    build = package / "BUILD"
    original = build.read_text()
    if BEGIN in original:
        prefix, rest = original.split(BEGIN, 1)
        _, suffix = rest.split(END, 1)
        original = prefix.rstrip() + "\n" + suffix.lstrip("\n")
    if LOAD not in original:
        original = LOAD + "\n" + original
    source = Path(__file__).with_name("redpanda_append_bench.cc")
    destination = package / "shep_io_bench.cc"
    if destination.exists() and not destination.read_text().startswith("// Real-file companion to shep-append/"):
        raise RuntimeError(f"refusing to replace unrelated file: {destination}")
    shutil.copyfile(source, destination)
    build.write_text(original.rstrip() + f"\n\n{BEGIN}\n{TARGET}{END}\n")

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("redpanda", type=Path, nargs="?", default=Path.home() / "redpanda")
    prepare(parser.parse_args().redpanda.resolve())
