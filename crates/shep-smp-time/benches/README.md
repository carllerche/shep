# Timer performance comparisons

The `timers` benchmark uses the public API, explicit time advancement, and
Criterion 0.8. It performs no I/O or wall-clock sleeps. Criterion is a dev-only
dependency, with plotting and parallel analysis disabled. The benchmark source
forbids unsafe code. The initial baseline predates the outer-cell removal and
timer-list specialization now applied in the workspace.
The initial run's environment and representative measurements are recorded in
[BASELINE.md](BASELINE.md).

## Running

The original `intrusive-b0a64f3` baseline and the applied cleanup's
`specialized-entry-list` baseline are already captured on this VM. Preserve
those names. From the workspace root, save a new baseline before the next
implementation change:

```sh
taskset -c 0 cargo bench -p shep-smp-time --bench timers -- \
  --save-baseline before-arena --noplot
```

After a change, compare with the saved baseline without replacing it:

```sh
taskset -c 0 cargo bench -p shep-smp-time --bench timers -- \
  --baseline before-arena --noplot
```

The default is 50 samples, one second of warm-up and three seconds of
measurement per case. There are 46 cases. Criterion writes estimates and raw
samples under `target/criterion/`; retain that directory between builds. Named
baselines and comparison options are described in the
[Criterion guide](https://criterion-rs.github.io/book/user_guide/command_line_options.html#baselines).
Use different names for each implementation you want to retain. Separate
worktrees need the same Criterion output directory to share baselines; keep
their compiled Cargo targets separate so stale binaries cannot enter a run.
Set `CRITERION_HOME` to an absolute directory to share only results:

```sh
CRITERION_HOME=/home/exedev/shep/target/criterion \
  taskset -c 0 cargo bench -p shep-smp-time --bench timers -- \
  --baseline before-arena --noplot
```

Run the same source against itself first to establish noise. Keep the CPU,
toolchain, optimization flags, benchmark source, sample settings, and background
load the same. CPU affinity prevents migration within the VM; it does not
isolate the host CPU. Confirm changes with repeated baseline/candidate runs,
including reversing their order, before accepting small differences.

Useful shorter runs:

```sh
# Execute fixtures and all cases once, including correctness checks; no statistics.
cargo test -p shep-smp-time --bench timers

# Filter cases and shorten sampling for exploration, not acceptance decisions.
taskset -c 0 cargo bench -p shep-smp-time --bench timers -- \
  '^(churn|reset_extend|expire_spread)/1024$' --quick --noplot
```

Criterion reports **time per batch** and **timers per second** for the scaled
cases. Divide batch time by the count for average time per timer. Its confidence
intervals describe throughput estimates, not individual timer p99 latency.

## What each case measures

Scaled cases run with 1, 1,024, and 32,768 live timers. Fixtures use pinned boxes
so moving the harness's containers cannot move registered entries. The inline
lifecycle case and executor tasks also exercise sleeps without a per-sleep Box.

| Cases | Measured work | Excluded work |
|---|---|---|
| `lifecycle/timer_create_drop` | Allocate, initialize, and drop a wheel | Clock read |
| `lifecycle/lazy_inline` | Construct/drop an unpolled inline sleep | Wheel construction and entering the timer scope |
| `lifecycle/register_cancel_inline` | Construct, pin on the stack, register, cancel | Wheel construction, timer scope entry; no per-sleep heap allocation |
| `lifecycle/register_cancel_boxed` | The same lifecycle with a Box allocation/free | Wheel construction, timer scope entry |
| `register/N` | First poll of previously unregistered sleeps | Sleep/Box/fixture construction and cleanup |
| `cancel/N` | Drop registered sleeps in shuffled order, including Box frees | Registration, shuffle preparation, remaining fixture cleanup |
| `reset_extend/N` | Extend deadlines lazily, leaving cached bucket membership | Fixture setup, later cascading, cleanup |
| `reset_shorten/N` | Shorten deadlines, unlink, and insert in an earlier bucket | Fixture setup and cleanup |
| `expire_dense/N` | Advance through one shared deadline | Registration, subsequent polling and freeing sleeps |
| `expire_spread/N` | Advance through deadlines spread over 4,096 ticks | Same |
| `expire_levels/N` | Advance through deadlines spanning all six levels at larger counts | Same |
| `expire_extended/N` | Advance through old cached buckets and extended deadlines | Registration and initial lazy resets |
| `expire_waker/N` | Spread expiration plus real Arc-backed wake/drop work | Waker construction, registration, sleep cleanup |
| `churn/N` | Allocate/register/extend/cancel all sleeps; reuse wheel and Vec capacity | Wheel, Vec capacity and deadline preparation |
| `next_wake/N` | Query a populated wheel; time is per query | Fixture setup/cleanup |
| `poll_pending/N` | Repeated polling of registered, pending sleeps with the same waker | Fixture setup/cleanup |
| `is_elapsed/N` | Read readiness of registered sleeps | Fixture setup/cleanup |
| `executor_expire/N` | Expire timers, deliver task wakes, run tasks to completion, release finished futures | Task spawning and first pending poll |

The main wheel-only cases use a noop waker. `expire_waker` includes an atomic
counter and Arc lifetime work; it is a separate measurement rather than an
estimate of every executor's wake cost. `executor_expire` uses `shep-smp-exec`'s
actual task wakers and wake queue. This is still component integration, without
an io_uring reactor or OS scheduling-delay measurement.

Operation fixtures create a fresh wheel. A future arena implementation's first
registration can include capacity growth in `register`; `churn` also exercises
steady-state slot reuse after warm-up. Use both before judging an arena's cost.

## Memory and correctness

The harness prints `size_of::<Sleep>()` and `size_of::<Timer>()`. At the current
baseline these are 120 and 24 bytes on x86_64. These are inline sizes, **not**
total allocated memory. An arena may shrink Sleep while retaining peak arena
capacity, so neither a smaller future nor quicker cancellation is sufficient
evidence of lower memory use. Before accepting that design, profile allocation
counts/bytes and retained capacity through grow/shrink/reuse workloads as well.
The Criterion process also allocates fixtures and samples, so process-wide peak
RSS is not an isolated timer memory measurement.

Validate changes with the existing timer integration tests and Miri in addition
to these measurements. The benchmark's preflight checks verify registration,
reset, cancellation, completion, exact wake counts, and executor drain outside
the timed sections; each case is also exercised by `cargo test --bench timers`.

## Implemented changes and remaining candidates

1. **Remove the outer entry `UnsafeCell` after an aliasing review.** All actual
   mutations now use the inner `Cell`/`RefCell` fields. An isolated probe changed
   `UnsafeCell<TimerShared>` to `TimerShared` and returned `&self.inner` directly;
   all 42 unit/integration tests passed under Miri. This removes one unsafe
   dereference without changing allocation strategy or layout. Miri passing is
   evidence for the change, not an exhaustive proof of the pointer invariants.
   This change is now applied; the shared-state access remains shared and all
   mutations stay inside `Cell`/`RefCell`.
2. **Specialize the intrusive list for timer entries.** Remove the generic
   unsafe `Link` trait and raw pointer-to-link conversion hooks. Keep direct
   next/previous pointers and existing allocation behavior. This reduces the
   unsafe abstraction surface, but retains lifetime and membership obligations.
   Monomorphization already removes generic dispatch, so expect little timing
   change; verify code size and throughput rather than claiming a speedup.
   This change is now applied as the private `timer/entry/list.rs` module, with
   explicit unsafe insertion/removal contracts and the upstream tests adapted
   to concrete timer entries.
3. **Own entries in a safe indexed arena.** Keep the wheel's six-level structure
   and constant-time doubly linked removal, using indices for buckets, pending
   entries, and links. Sleep keeps its owning handle and an entry key. This can
   remove the timer's raw pointers and dependence on the future's address.
   Compare bounds-checked lookups, slot reuse/growth, generation checks if keys
   can outlive a slot, cache locality, future size, and retained capacity.
   Extract wakers before releasing entries and invoke/drop them after releasing
   the arena borrow, preserving callback reentrancy.
4. **Use vectors of arena keys per bucket.** Store each entry's bucket and
   position, then `swap_remove` and update the moved entry to retain constant-time
   cancellation. This may improve cascade locality, but adds bucket capacities,
   growth allocations, and position bookkeeping. Compare it against indexed
   links separately so the cost of arena ownership is distinguishable.
5. **Use separately allocated Rc-owned entries.** Owning handles can make
   liveness safe without pinning the future, but ownership alone does not make
   raw intrusive links safe. Fully safe containers still require indices or
   ownership-bearing links, with cycle avoidance. Extra allocations, reference
   counts, and cache misses make this a less attractive first experiment.

An explicit entry-state enum can also remove sentinel-value and result-state
invariants. It does not independently eliminate raw-pointer unsafe code; check
its layout and reset/expiration performance before changing the packed state.
The outer-cell probe also needs an ownership/aliasing argument: mutable pin
projections still interact with aliases held by the wheel. Removing that wrapper
does not resolve the wheel's non-owning pointers or cancellation obligations.

The outer-cell cleanup and list specialization are applied. The indexed arena
remains a proposal; [ARENA.md](../ARENA.md) describes the ownership, slot lifetime,
reentrancy, and performance decisions to discuss before prototyping it.
