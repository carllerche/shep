# Unsafe reduction measurements

The retained implementation removes unsafe join-waker access, unchecked
non-null construction, and duplicated pointer/stage dereferences. It keeps the
inline pinned future, two allocations per new task, original handle/cell sizes,
manual local ownership and existing atomic notification protocol. Runtime unsafe
blocks decrease from 30 to 17; unsafe function definitions decrease from 10 to 7
(excluding the five unsafe vtable function-pointer types). Some reduction is
consolidation of invariants rather than elimination of the underlying raw
access. `state.rs` and `waker.rs` now forbid unsafe code. See ../../docs/task/SAFETY.md.

## Environment and method

- Baseline library: commit `424fa32551d24eee621d48199fb91086c23a6a3d`.
- Rust 1.97.1 (`8bab26f4f`, LLVM 22.1.6), default Cargo bench/release profile.
- Criterion 0.8.2; AMD EPYC 9554P x86_64 KVM VM, 16 vCPUs, no CPU pinning.
- All 27 cases run before any library changes, then again after changes.
  Both original library sources and retained library sources were rebuilt and
  measured a second time with the same benchmark and dependency lockfile.
- Full runs: 60 samples, 300 ms warmup, 1 s measurement per case. A further
  confirmation run used 1 s warmup / 3 s measurement for the eight steady-state
  cases and registered_join/4096. Before/after sources were restored with a
  finally guard between rebuilds; no Git checkout/reset or commit was used.
- Detailed historical estimates, metadata and source hashes are available in
  Git history. Raw Criterion samples were saved in target/criterion.

Shared-VM effects are visible, especially allocation-heavy cases and the channel
handoff. Some checks/compilation ran during the first exploratory runs. The
repeat and longer confirmation measurements ran without those validation jobs.
Treat small ratios as measured variation, not a portable performance guarantee.

## Retained implementation

Median times from the second complete before/after pair. Lifecycle rows with a
count greater than one are whole-batch times, not per-task times. Negative
changes indicate faster measurements.

| Benchmark | Before (ns) | After (ns) | Change |
|---|---:|---:|---:|
| cross_thread/wake_and_poll | 10723.45 | 9495.14 | -11.5% |
| lifecycle/cancel/1 | 46.96 | 47.15 | +0.4% |
| lifecycle/cancel/4096 | 188909.67 | 185317.95 | -1.9% |
| lifecycle/cancel/64 | 2934.51 | 2968.62 | +1.2% |
| lifecycle/cancel_polled/1 | 61.07 | 61.11 | +0.1% |
| lifecycle/cancel_polled/4096 | 249757.95 | 253233.92 | +1.4% |
| lifecycle/cancel_polled/64 | 3921.47 | 3891.77 | -0.8% |
| lifecycle/detach/1 | 48.94 | 48.89 | -0.1% |
| lifecycle/detach/4096 | 203480.24 | 191162.00 | -6.1% |
| lifecycle/detach/64 | 3093.45 | 3077.07 | -0.5% |
| lifecycle/join/1 | 57.35 | 57.99 | +1.1% |
| lifecycle/join/4096 | 236425.88 | 230227.49 | -2.6% |
| lifecycle/join/64 | 3773.53 | 3666.25 | -2.8% |
| lifecycle/registered_join/1 | 59.33 | 60.73 | +2.3% |
| lifecycle/registered_join/4096 | 244942.24 | 259908.56 | +6.1% |
| lifecycle/registered_join/64 | 3871.58 | 3820.87 | -1.3% |
| lifecycle/resident/1 | 81.39 | 80.19 | -1.5% |
| lifecycle/resident/4096 | 444085.29 | 429625.03 | -3.3% |
| lifecycle/resident/64 | 6775.81 | 5808.56 | -14.3% |
| steady/clone_drop_waker | 3.88 | 3.89 | +0.1% |
| steady/coalesced_wake | 3.13 | 3.15 | +0.6% |
| steady/completed_wake | 3.20 | 3.15 | -1.5% |
| steady/join_replace | 9.69 | 9.80 | +1.1% |
| steady/join_same | 3.57 | 3.62 | +1.3% |
| steady/self_wake_poll | 14.47 | 14.80 | +2.3% |
| steady/unnotified_run | 1.65 | 1.64 | -0.5% |
| steady/wake_and_poll | 18.50 | 18.37 | -0.7% |

Most lifecycle/steady cases remain close to baseline, with no large repeatable
regression like those in the rejected prototypes. The longer confirmation
measurements are included below so the small adverse ratios are visible too.
For self-wake polling, disassembly of the baseline and retained benchmark
specialization contains the same 81 instructions after ignoring instruction
addresses, relocations and panic-message metadata. The retained pending poll
path adds no allocation, borrow check, lock or instruction to that specialization.
This does not prove identical timing: code placement and VM scheduling still
matter. The cross-thread benchmark's changing sign between runs is consistent
with handoff/scheduling noise; the notification implementation is unchanged.

| Benchmark | Before (ns) | After (ns) | Change |
|---|---:|---:|---:|
| lifecycle/registered_join/4096 | 246735.15 | 254557.03 | +3.2% |
| steady/clone_drop_waker | 3.99 | 4.02 | +0.7% |
| steady/coalesced_wake | 3.24 | 3.11 | -4.0% |
| steady/completed_wake | 3.19 | 3.13 | -1.9% |
| steady/join_replace | 9.79 | 10.06 | +2.7% |
| steady/join_same | 3.51 | 3.64 | +3.8% |
| steady/self_wake_poll | 14.48 | 15.00 | +3.6% |
| steady/unnotified_run | 1.62 | 1.61 | -0.4% |
| steady/wake_and_poll | 18.42 | 17.69 | -4.0% |

## Alternatives measured and rejected

1. **RefCell for the inline stage.** Replaced raw stage references with checked
   borrows, leaving just the unchecked pin projection in core.rs. Passed native
   tests. It enlarged the task allocation and added runtime borrow handling.
   Resident-task medians regressed about 21–38%, and join lifecycles about 7–11%
   in the first pair. The larger effects exceeded ordinary repeat-run changes.
   Estimates are saved as `refcell`; this implementation is not retained.
2. **Typed Rc trait objects for both local owners.** Removed raw vtable calls,
   the erased join output pointer, manual reference counting and deallocation;
   just the two stage/pin unsafe blocks remained. Passed native tests and Miri.
   Handle/allocation sizes and dispatch changed. Compared with before-repeat,
   ordinary lifecycle medians regressed about 12–32%; unnotified_run rose from
   1.65 ns to 4.15 ns. Estimates are saved as `rc`; this implementation is not
   retained. These measurements reject this prototype, not every possible Rc
   design.

The library code for those experiments was restored after measurement; only the
estimates and explanations remain. The saved `after` and `after-repeat` datasets
both refer to the retained small refactor. Neither production dependencies nor
public APIs changed; Criterion is a dev dependency.

## Correctness checks

- Native task tests, including four new regression cases (20 total), and four
  compile-fail docs checking local-only ownership.
- Task suite under nightly-2026-10-22 Miri, plus a separate Tree Borrows run.
- Executor and timer tests/docs exercising the task integration.
- Release task tests, Clippy with warnings denied, and rustfmt.

Run commands and timing boundaries are in README.md. Miri and tests provide
execution evidence, not an exhaustive proof of all foreign wake interleavings.
