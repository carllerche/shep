#!/bin/sh
set -eu

# Rustdoc uses the target runner too. Preserve its merged-doctest subprocess
# behavior, since nextest does not run doctests.
case "$1" in
    */rust_out) RUSTDOC_DOCTEST_BIN_PATH="$1" exec "$@" ;;
    # Cargo composes Miri's interpreter runner after this configured runner.
    */cargo-miri|cargo-miri) exec "$@" ;;
esac

# Cargo and nextest both use this runner. Apply the policy to the whole package
# so new test targets need no imports, attributes, or setup calls.
if [ "${CARGO_PKG_NAME:-}" = "shep-smp2" ] &&
    [ "${NEXTEST_EXECUTION_MODE:-}" != "process-per-test" ]; then
    for argument do
        case "$argument" in
            # Discovery/help do not execute tests. Criterion benchmarks run
            # directly via `cargo bench`, which passes --bench.
            --list|--help|-h|--bench) exec "$@" ;;
        esac
    done
    exec python3 "$(dirname "$0")/nextest-binary.py" "$@"
fi

exec "$@"
