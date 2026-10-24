# Monoio task extraction

Adapted from `monoio/src/task/` in
[Monoio](https://github.com/bytedance/monoio/tree/8675e9406ea7943c441d969e38c3f5fc198ef6d1/monoio/src/task),
commit `8675e9406ea7943c441d969e38c3f5fc198ef6d1` (the local checkout in
`~/monoio`, package version 0.2.4). This extraction uses Monoio's MIT license;
the license and its Tokio attribution are included in this directory.

The task cell, erased vtable, polling harness, join handle, notification state
machine, and cell helpers come from `core.rs`, `raw.rs`, `harness.rs`, `join.rs`,
`state.rs`, and `utils.rs`. `lib.rs` replaces the module entry point. `waker.rs`
replaces upstream's raw task waker with standard-library `Wake` plumbing.

Changes for Shep:

- Expose the task primitives as a separate crate with no runtime dependencies.
  Neither Monoio nor Tokio is a runtime dependency. Tokio's channels are used
  in integration tests to exercise wakeups without a Tokio runtime.
- Futures, outputs, task handles, and join handles remain local. Remove
  upstream's `Send`/`Sync` implementations for join handles and the unsafe
  non-`'static` task constructor.
- The executor retains a `Task` for every live future. `run(&mut self)` polls
  once when notified; dropping `Task` cancels it and releases its future on
  the owner thread, even if a join handle or waker survives shutdown.
- Wakers share only atomic state, a process-unique task ID, and the scheduler's
  notification interface. They never reference the local task allocation.
  A `Schedule` implementation queues IDs and wakes the owner thread; it must
  tolerate stale IDs and shutdown. The executor and its tasks remain local;
  only notification plumbing needs `Send + Sync`.
- Keep Monoio's notification coalescing and reschedule self-wakes for another
  turn. Remove runtime TLS, thread IDs, remote-waker registries, blocking-task
  completion, feature switches, and diagnostic tracing.
- Turn future panics into `JoinError::Panicked` and cancellation into
  `JoinError::Cancelled`. Unwinding is contained per task; panic payloads are
  discarded. Preserve in-place stage assignment so pinned futures are dropped
  at their original addresses and a panicking destructor leaves a valid stage.
- Release stored join wakers on completion or detachment. Keep assertions
  against polling a consumed join handle.

This is the first component of the future `shep-smp-*` runtime. The existing
`shep-smp` continues to use Tokio while task scheduling, the io_uring reactor,
and the hashed timing wheel are introduced incrementally. Buffer ownership and
io_uring buffer-pool integration belong in later I/O components.

Further unsafe reduction is documented in [SAFETY.md](SAFETY.md). The local join
waker now uses `Cell<Option<Waker>>`; raw stage access is confined to one private
closure helper, and the pinning operation has an explicit invariant. Header,
core and trailer access share one checked-by-construction pointer boundary.
Construction uses `Box::leak`/`NonNull::from` instead of unchecked null handling.
The generic UnsafeCell extension helpers were removed. The public API,
allocation count and cross-thread notification state protocol are preserved.
See [the benchmarks](benches/README.md) for timing boundaries and measured
alternatives rejected because of their performance cost.
