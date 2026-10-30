# Task benchmarks

Run from the workspace root, before editing library code:

```sh
cargo bench -p shep-smp2 --bench tasks -- --save-baseline before
# After changes:
cargo bench -p shep-smp2 --bench tasks -- --baseline before
```

The suite uses public APIs and forbids unsafe code. Criterion takes 60 samples,
a 300 ms warmup and one second of measurement per case by default. For a more
stable dedicated-machine run, pass `--warm-up-time 2 --measurement-time 5`.
Keep compiler, CPU affinity, profile and benchmark source identical between
runs. Shared VM scheduling and frequency changes can overwhelm small effects;
repeat both versions before attributing a small difference to code changes.
Criterion stores raw samples and estimates in `target/criterion` (not tracked).

Timing boundaries:

- `lifecycle/{join,registered_join,detach,cancel,cancel_polled}/{1,64,4096}`:
  allocate, poll/complete or cancel, consume/drop output, and release both
  handles. Registered joins include initial parent registration;
  cancel_polled includes a pending future poll and parent registration.
  Each iteration processes the stated number of tasks. Throughput is tasks/s.
- `lifecycle/resident/{1,64,4096}`: allocate a live working set before polling,
  producing and consuming 128-byte outputs, then release all task storage.
  Includes the vector allocation; exposes working-set and cell-size effects.
- `steady/self_wake_poll`: one already allocated, always pending future wakes
  itself each poll. Includes scheduling once per turn.
- `steady/unnotified_run`: repeated owner checks without notification.
- `steady/join_same` and `join_replace`: pending join polling with the same or
  alternating real Arc-backed wakers. Setup and task destruction excluded.
- `steady/wake_and_poll`: wake an idle task, poll it, and save a cloned waker.
- `steady/coalesced_wake`: wake a task that is already notified.
- `steady/clone_drop_waker`: standard Arc-backed task waker clone/drop.
- `steady/completed_wake`: late wake after both local handles were released.
- `cross_thread/wake_and_poll`: a persistent foreign thread consumes a cloned
  task waker and acknowledges it; the owner then polls. Includes two synchronous
  channel handoffs, excludes thread startup. Measures end-to-end handoff, not
  isolated atomic latency. Never sends a task, future or join across threads.

The scheduler is an atomic counter with no queue/allocation. This isolates the
primitive from executor queue policy while retaining an observable scheduling
cost. Assertions check expected behavior during measurement. Steady-state cases
exclude allocation/destruction; lifecycle cases include them. Panic unwinding
is tested separately, since panic hooks distort microbenchmarks.

To save two named datasets and compare their medians directly:

```sh
cargo bench -p shep-smp2 --bench tasks -- --save-baseline after
python3 crates/shep-smp2/benches/task/compare.py before after
```

`--baseline` and `--save-baseline` are mutually exclusive in Criterion. Use the
former for its statistical comparison against an existing dataset; use the
latter to retain separately named estimates. The comparison script reports
median ratios, not a significance test.

The suite has 27 cases. It does not measure executor queue selection, timer or
I/O performance. Run executor/timer integration tests separately to check those
contracts. Allocation, polling and wake paths have intentionally different
timing boundaries and should not be combined into a single aggregate score.
