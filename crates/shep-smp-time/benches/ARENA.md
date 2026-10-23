# Slab-owned timer migration

The runtime now owns entries in shep-util-slab, with checked ID links and
`#![forbid(unsafe_code)]`. It retains the timing wheel and public sleep/reset
behavior. Default reservation is 1,024 slots, configurable with
`Timer::with_capacity`; `Timer::capacity` exposes retained capacity.

## Validation

- 50 unit/integration tests pass, including eight arena/waker regression cases.
- Miri passes all 50 tests, using small pools for the large capacity-boundary
  case. The native suite checks capacities 0/1/256/1,024/4,096 and growth/reuse.
- All 24 doctests, 56 benchmark smoke cases, Clippy with denied warnings,
  strict rustdoc, formatting, and workspace checks pass.
- LLVM coverage is 94.34% of lines overall; entry logic is 97.84%, list logic
  99.28%, and Sleep/interval logic 100%. Unused legacy error constructors account
  for much of the uncovered code.

## Timing

These figures describe the initial migration before the subsequent
[hot-path optimizations](OPPORTUNITIES.md), whose results are reported separately.

Measured on 2026-10-23, rustc 1.97.1, Cargo's default bench profile, on the same
AMD EPYC 9554P exe.dev KVM VM pinned to CPU 0. Criterion uses 50 samples,
one-second warm-up, and three-second target measurement per case. The current
56 cases are saved as `slab-owned-arena` under `target/criterion`.

For ten hot-path/rearm cases, a focused before measurement used an isolated
copy of commit b5b9a22 and the same benchmark bodies/settings. Those samples are
saved as `before-arena-matched`. That copy omits the new reservation benchmarks,
whose constructor does not exist in the old implementation. Other before
figures below use the earlier `specialized-entry-list` measurement of that
pointer-based implementation. All figures divide batch time by consumer count.

| Case | Count | Before, ns/timer | Arena, ns/timer | Before source |
|---|---:|---:|---:|---|
| Owner construction/drop | 1 | 123.42 | 191.46 | Historical |
| Unpolled inline lifecycle | 1 | 12.39 | 7.51 | Historical |
| Inline register/cancel | 1 | 41.36 | 39.90 | Focused |
| First registration | 1,024 | 22.25 | 19.76 | Focused |
| First registration | 32,768 | 20.15 | 31.14 | Focused |
| Cancel including Box free | 1,024 | 22.84 | 22.11 | Focused |
| Lazy extension | 1,024 | 9.29 | 10.20 | Historical |
| Shorten/reinsert | 1,024 | 18.10 | 20.78 | Focused |
| Spread expiration | 1,024 | 13.98 | 14.64 | Historical |
| Steady churn | 1,024 | 62.52 | 70.71 | Historical |
| Steady churn | 32,768 | 75.89 | 72.92 | Historical |
| Pending poll | 1,024 | 2.64 | 3.19 | Focused |
| Read readiness | 1,024 | 1.07 | 2.27 | Focused |
| Expire and finish executor tasks | 1,024 | 88.25 | 89.06 | Historical |
| Rearm an observed elapsed sleep | 1 | 27.17 | 20.64 | Focused |
| Rearm observed elapsed sleeps | 1,024 | 14.00 | 14.28 | Focused |
| Rearm observed elapsed sleeps | 32,768 | 16.68 | 15.39 | Focused |

There are costs as well as benefits. Readiness now requires a timer-state
borrow and slab lookup. Large first-registration batches include arena growth
beyond the default reservation; steady churn reuses that storage. Initial owner
construction includes the new reservation. Retaining elapsed slots makes
rearming allocation-free at the slab level. This does not imply every workload
is faster or unchanged.

The VM's unchanged-code repeat drift is substantial, as shown in BASELINE.md.
Small differences and historical comparisons are inconclusive; the focused
before run itself differs from older measurements. In particular, do not infer
a first-registration improvement at 1,024 consumers from these point estimates.
The larger readiness and cold-registration differences are consistent with the new
lookup/storage work and deserve attention when choosing workload/capacity.

Subsequent [counter-based attribution](ATTRIBUTION.md) separates the lookup and
inlining costs from first-registration initialization/page faults. In its fresh
32K-consumer probe, full reservation barely changes the cold cost, while reusing
populated pool pages roughly halves registration time. Capacity growth alone
does not explain the cold registration slowdown.

First registration around reserved capacity, in ns/timer:

| Reservation | Consumers | Time |
|---:|---:|---:|
| 0 | 1 | 42.86 |
| 256 | 256 | 18.48 |
| 256 | 257 | 20.51 |
| 1,024 | 1,024 | 18.37 |
| 1,024 | 1,025 | 19.45 |
| 4,096 | 4,096 | 19.40 |
| 4,096 | 4,097 | 20.01 |

These measure fresh fixtures; reservation/setup is excluded, and registration
includes growth if needed. They are not physical cold-page or p99 measurements.

## Retained memory

Valgrind DHAT 3.22 snapshots use the safe `arena_memory` example against the
current code and the isolated b5b9a22 copy. The old copy substitutes its existing
`Timer::new` and reports zero arena capacity; the profiling workloads otherwise
match. The example intentionally forgets owners for the chosen phase so DHAT's
end-of-process live heap records that state. Figures are requested live heap
bytes, including common process bookkeeping; not RSS or transient growth peaks.

| Phase | Consumers | Before, bytes | Arena, bytes |
|---|---:|---:|---:|
| Empty owner, zero arena reservation | 0 | 6,936 | 6,976 |
| Empty owner, default reservation | 0 | 6,936 | 64,320 |
| Live boxed sleeps | 1,024 | 138,008 | 121,664 |
| Expired boxed sleeps retained | 1,024 | 138,008 | 121,664 |
| All consumers dropped | 1,024 | 6,936 | 64,320 |
| Live boxed sleeps | 32,768 | 4,201,240 | 3,676,992 |
| All consumers dropped | 32,768 | 6,936 | 1,841,984 |

Sleep is 48 bytes rather than 120. The arena's slot is 56 bytes on this build,
so a full pool plus each future uses 104 bytes per consumer instead of 120,
before shared/container overhead. A 1,024-slot reservation alone requests
57,344 bytes, and a 32,768-slot high-water reservation retains 1,835,008 bytes.
The snapshot includes a Vec of boxed owners for both versions; actual executor
task layouts differ. Empty reservations and post-burst retention therefore
matter just as much as the smaller future. Slots contain no retained wakers
after their consumers are dropped.

## Next performance work

The migration deliberately keeps existing interval boxing, first-waker policy,
and pin-shaped APIs. It does not add shrinking or unchecked access. The
[applied follow-up](OPPORTUNITIES.md) inlines readiness, separates slow polling
setup, and initializes new entries directly before insertion. Remaining owner
borrowing and dependent lookup costs deserve measurement on a stable machine.
Memory reclamation after bursts is a separate policy decision, with live-key
stability and free-list tests required before adding it to the minimal slab.
