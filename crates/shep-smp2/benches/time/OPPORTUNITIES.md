# Applied timer optimizations

All three changes below are now applied. They retain safe slab ownership,
checked access, callback reentrancy, and the existing first-waker policy.
Measurements use the same fixed-work harness, compiler, CPU affinity, and gated
perf procedure as [ATTRIBUTION.md](ATTRIBUTION.md), with five alternating trials
per comparison.
The initial isolated-prototype trials are saved in
[opportunity-probes.json](opportunity-probes.json).

The final working-tree binary was compared against a saved, unoptimized arena
binary in five alternating trials. Raw counters and normalized figures are in
[applied-probes.json](applied-probes.json). Setup/destruction remain excluded;
the traversals themselves are included in instructions and cycles.

| Applied changes together | Instructions/op, before → after | User cycles/op, before → after |
|---|---:|---:|
| Readiness, 1,024 consumers | 36.01 → 31.01 | 8.21 → 6.77 |
| Pending poll, 1,024 consumers | 50.01 → 39.01 | 11.94 → 8.79 |
| Registration in reused pool, 32,768 consumers | 301.97 → 280.97 | 70.45 → 66.16 |

The isolated experiments identify the contribution of each change:

| Isolated change | Operation | Instructions/op | User cycles/op |
|---|---|---:|---:|
| Inline both readiness helpers | Readiness, 1,024 consumers | 36.01 → 31.01 | 8.15 → 6.86 |
| Separate slow polling setup into a non-inlined helper | Pending poll, 1,024 consumers | 50.01 → 39.01 | 11.88 → 8.82 |
| Same polling change | Registration in reused pool, 32,768 consumers | 301.97 → 290.97 | 69.97 → 67.54 |
| Initialize new entries directly for their deadline, after polling change | Registration in reused pool, 32,768 consumers | 290.97 → 280.97 | 67.35 → 66.58 |

The table's registration comparisons are separate alternating experiments;
their cycle figures should not be added as exact independent savings. The
last change has a clear instruction reduction but a small cycle difference
that needs confirmation on a stable machine. These measurements do not establish
an end-to-end reactor or executor throughput improvement.

## Implementation

1. Add `#[inline]` to both `Sleep::is_elapsed` and `TimerEntry::is_elapsed`.
   Annotating only the public wrapper leaves the inner call out of line.
   Ordinary inline hints suffice; `inline(always)` is not required by the probe.

2. Keep the existing-key/stored-waker check in `poll_elapsed`, and move initial
   registration and waker cloning/installation to a separate `#[inline(never)]`
   helper. This reduces the common Future::poll stack frame and skips the
   redundant pre-clone owner lookup for a freshly allocated entry. Clone still
   occurs outside owner borrows, and state is rechecked afterward. The prototype
   pending path slightly undercuts the old pointer implementation's 40.01
   instructions and roughly matches its earlier measured cycle cost.

3. On first reset/registration, initialize the local Entry with its requested
   deadline before inserting it. Skip the extend/unlink checks needed for an
   existing entry. This removes 10 more instructions per registration without
   changing the slot size or slab API. Existing-key reset behavior is preserved.

The applied implementation passes all 50 unit/integration tests under both
native execution and Miri, plus 24 native doctests and Clippy with denied
warnings. This includes a new regression that repeats pending polls with
replacement wakers, checks that no replacement is cloned, and verifies that
expiration wakes the original task. Callback reentrancy, clone panic/retry,
cancellation, reuse, growth, and wheel-boundary tests also pass. The library
still forbids unsafe code.

All 56 Criterion cases also completed successfully, saved as
`slab-owned-optimized`. Their estimates and confidence intervals are preserved
in [optimized-criterion.json](optimized-criterion.json). The shared VM's elapsed
times drift across runs, including unchanged operations; these historical
baseline comparisons do not establish an end-to-end speedup. The paired native
counters above provide the evidence for the targeted reductions.

## Other tradeoffs

Explicit pool initialization during reactor startup could move page faults
out of the first-registration path. Reservation alone does not populate the
pages. This exchanges startup work and earlier resident memory for less cold
latency; it does not remove initialization work. The existing reused-pool probe
demonstrates the potential difference but does not justify automatically
warming every default reservation.

Larger changes to slot representation, index width, or timer state encoding
should come later. They introduce representation limits or additional
invariants, whereas the three changes above keep the existing layout and
public APIs. The remaining borrow checks and dependent lookups are still real
costs after those changes.

A tested alternative replaced indexing with checked `get`, copied the result,
released the borrow, and only then checked for an invalid key. It did not remove
the borrow-counter writes or save instructions beyond the inline hints. It is
not included in the recommended patch.
