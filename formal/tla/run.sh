#!/bin/sh
# Usage: ./run.sh Spec[.tla] [Config.cfg] [extra TLC args]
# Runs TLC on a spec. TLA2TOOLS can point at tla2tools.jar (v1.8.0).
set -e
JAR="${TLA2TOOLS:-$HOME/.cache/tla2tools-1.8.0.jar}"
if [ ! -f "$JAR" ]; then
  echo "tla2tools.jar not found at $JAR; set TLA2TOOLS=/path/to/tla2tools.jar" >&2
  echo "download: https://github.com/tlaplus/tlaplus/releases/download/v1.8.0/tla2tools.jar" >&2
  exit 1
fi
SPEC="${1%.tla}"; shift
CFG="${1:-$SPEC.cfg}"; [ $# -gt 0 ] && shift
DIR="$(cd "$(dirname "$0")" && pwd)"
SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/shep-tlc-XXXXXX")
META="$SCRATCH/states"
cd "$DIR"
# One TLC at a time per machine: concurrent runs each take every core and a
# large heap, and swap. flock (Linux) or lockf (BSD) waits for the previous run.
LOCK="${TLC_LOCK:-$DIR/.tlc.lock}"
# TLC's -cleanup skips the states directory when a run fails or is killed, and
# large runs leave gigabytes behind, so remove it ourselves however we exit.
# Java's extracted standard modules also live here, rather than leaving
# separate tlc-* directories behind in the system temporary directory.
trap 'rm -rf "$SCRATCH"' EXIT
trap 'exit 130' INT TERM HUP
set +e
set -- java -Djava.io.tmpdir="$SCRATCH" -Xmx${TLC_HEAP:-6g} -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers "${TLC_WORKERS:-auto}" -deadlock -cleanup -noGenerateSpecTE \
  -metadir "$META" -config "$CFG" "$@" "$SPEC.tla"
if command -v flock >/dev/null 2>&1; then
  flock "$LOCK" "$@"
elif command -v lockf >/dev/null 2>&1; then
  lockf -k "$LOCK" "$@"
else
  echo "flock or lockf is required to serialize TLC runs" >&2
  exit 1
fi
exit $?
