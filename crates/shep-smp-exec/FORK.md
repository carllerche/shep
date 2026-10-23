# Monoio executor adaptation

Based on [Monoio](https://github.com/bytedance/monoio/tree/8675e9406ea7943c441d969e38c3f5fc198ef6d1/monoio/src),
commit `8675e9406ea7943c441d969e38c3f5fc198ef6d1`, from the clean local
checkout in `~/monoio` (package version 0.2.4). The MIT license is included.
This crate adapts the scheduling architecture to `shep-smp-task`; it does not
depend on Monoio or bring in its drivers.

Source decisions:

- `scheduler.rs`: a local FIFO queue; a local wake schedules another turn
  rather than polling recursively.
- `runtime.rs`: scoped current-executor context for `spawn`, rejection of
  nested execution, and bounded task turns before yielding to the I/O driver.
- `driver/mod.rs` and `driver/uring/waker.rs`: driver-owned waiting and a
  thread-safe wake handle. The executor does not implement parking itself.
- `macros/scoped_tls.rs`: restore the previous context on unwind. Shep uses
  standard-library TLS and `Rc`/`Weak` instead of copying the unsafe scoped-TLS
  macro or adding a TLS dependency.

Changes for Shep:

- Separate the local executor from the runtime loop and reactor. The public
  surface is `Executor::new(driver_waker)`, `spawn`, `run_ready(budget)`, ready
  and ownership checks, and shutdown. `spawn` is also available as a free
  function inside a running task, matching Monoio's scoped spawning model.
- Keep a local registry of all tasks, not only ready tasks. This follows
  `shep-smp-task`'s ownership contract: executor shutdown drops every future on
  its owning thread even if join handles or foreign wakers survive.
- Release all registry borrows before polling or dropping a future so spawning
  from tasks and shutdown destructors cannot conflict with a `RefCell` borrow.
- Route same-executor wakes directly to the local queue. Use a standard-library
  inbox of opaque task IDs and a supplied driver waker for other notifications,
  instead of Monoio's runtime-ID registries, caches, and forwarded raw wakers.
- Use an explicit turn budget rather than Monoio's queue-length-derived limit.
  Both remote ingestion and queue consumption are bounded. A future still
  cooperatively yields by returning from `poll`; this is not preemption.
- Close admission before shutdown cleanup. Spawns into a closed executor
  immediately receive `JoinError::Cancelled`. Destructor-triggered spawns use
  the closing executor's scope, including when another executor is running.

The future io_uring runtime will supply the driver's retained wakeup mechanism,
service the ring and timing wheel between turns, and check `has_ready()` before
waiting. `shep-smp-exec` adds no alternate I/O backend, blocking loop, thread
pool, work stealing, timers, or changes to the existing `shep-smp` execution path.
