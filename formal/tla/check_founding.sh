#!/bin/sh
# Focused regressions for the resumed founding investigation. Expected
# failures must name the intended invariant; parser errors never count.
set -eu
cd "$(dirname "$0")"
OUT=$(mktemp -d "${TMPDIR:-/tmp}/shep-founding-XXXXXX")
trap 'rm -rf "$OUT"' EXIT
trap 'exit 130' INT TERM HUP

run() {
  spec=$1 cfg=$2 expected=$3
  task_rc=0
  ./run.sh "$spec" "$cfg.cfg" >"$OUT/$cfg.log" 2>&1 || task_rc=$?
  if [ "$expected" = pass ]; then
    if [ "$task_rc" -ne 0 ] || ! grep -q 'No error has been found' "$OUT/$cfg.log"; then
      cat "$OUT/$cfg.log"
      exit 1
    fi
  elif [ "$task_rc" -ne 12 ] || ! grep -q "^Error: Invariant $expected is violated" "$OUT/$cfg.log"; then
    cat "$OUT/$cfg.log"
    exit 1
  fi
  printf '%s: %s\n' "$cfg" "$expected"
}

run BootstrapFoundTrace BootstrapFoundTrace_typo OneCluster
run BootstrapFoundTrace BootstrapFoundTrace_amnesia OneCluster
run BootstrapFoundTrace BootstrapFoundTrace_wipe WipedStaysOut
run BootstrapFound BootstrapFound_promise_found NeverFounded
run BootstrapFound BootstrapFound_claim_found NeverFounded
run BootstrapFound BootstrapFound_typo_promise_t1 pass
run BootstrapFound BootstrapFound_same3_promise_t1 pass
run BootstrapFound BootstrapFound_same3_claim_t1 pass
run BootstrapFound BootstrapFound_typo_claim_t1 pass
