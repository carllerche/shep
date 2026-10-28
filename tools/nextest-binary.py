#!/usr/bin/env python3
"""Hand Cargo's already-built test executable to nextest's reuse-build API."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def run(binary, arguments):
    cargo = os.environ.get("CARGO", "cargo")
    metadata = json.loads(subprocess.check_output([
        cargo, "metadata", "--format-version", "1", "--no-deps",
        "--manifest-path", os.environ["CARGO_MANIFEST_PATH"],
    ]))
    package = next(p for p in metadata["packages"]
                   if p["name"] == os.environ["CARGO_PKG_NAME"])
    # Cargo appends an artifact hash to test executable names. Use the manifest
    # target for nextest's display name; execution always uses the supplied path.
    name = binary.name.rsplit("-", 1)[0]
    target = next(t for t in package["targets"]
                  if name in (t["name"], t["name"].replace("-", "_")))
    kind = target["kind"][0]
    binary_id = package["name"] if kind == "lib" else f"{package['name']}::{name}"

    rustc = os.environ.get("RUSTC", "rustc")
    host = next(line.removeprefix("host: ") for line in
                subprocess.check_output([rustc, "-vV"], text=True).splitlines()
                if line.startswith("host: "))
    libdir = subprocess.check_output([rustc, "--print", "target-libdir"], text=True).strip()
    # Retain the library paths Cargo set for this particular executable, even
    # with --release, custom profiles, --target-dir, or extra native libraries.
    linked_paths = [os.path.relpath(p, metadata["target_directory"])
                    for p in os.environ.get("LD_LIBRARY_PATH", "").split(os.pathsep) if p]
    binaries = {
        "rust-build-meta": {
            "target-directory": metadata["target_directory"],
            "base-output-directories": [],
            "non-test-binaries": {},
            "linked-paths": linked_paths,
            "platforms": {
                "host": {
                    "platform": {"triple": host, "target-features": "unknown"},
                    "libdir": {"status": "available", "path": libdir},
                },
                "targets": [],
            },
        },
        "rust-binaries": {
            binary_id: {
                "binary-id": binary_id,
                "binary-name": target["name"],
                "package-id": package["id"],
                "kind": kind,
                "binary-path": str(binary),
                "build-platform": "target",
            },
        },
    }

    # Nextest accepts the ordinary libtest filters after --. Translate the
    # common libtest options that instead have nextest runner equivalents.
    options, filters = [], []
    arguments = iter(arguments)
    for argument in arguments:
        if argument in ("--test-threads", "--color"):
            options.extend([argument, next(arguments)])
        elif argument.startswith(("--test-threads=", "--color=")):
            options.append(argument)
        elif argument in ("--quiet", "-q"):
            options.extend(["--status-level", "fail", "--final-status-level", "fail"])
        else:
            filters.append(argument)

    with tempfile.TemporaryDirectory(prefix="shep-nextest-") as directory:
        cargo_file = Path(directory) / "cargo.json"
        binaries_file = Path(directory) / "binaries.json"
        cargo_file.write_text(json.dumps(metadata))
        binaries_file.write_text(json.dumps(binaries))
        return subprocess.call([
            cargo, "nextest", "run",
            "--cargo-metadata", str(cargo_file),
            "--binaries-metadata", str(binaries_file),
            # Cargo applies a name filter to every binary. A binary containing
            # no matching tests must not stop the rest of the Cargo invocation.
            "--no-tests", "pass", *options, "--", *filters,
        ])


if __name__ == "__main__":
    sys.exit(run(Path(sys.argv[1]).resolve(), sys.argv[2:]))
