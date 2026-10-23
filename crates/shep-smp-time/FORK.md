# Monoio timer extraction

Extracted from `monoio/src/time/` and `monoio/src/utils/linked_list.rs` in
[Monoio](https://github.com/bytedance/monoio/tree/8675e9406ea7943c441d969e38c3f5fc198ef6d1/monoio/src),
commit `8675e9406ea7943c441d969e38c3f5fc198ef6d1`, from the clean local checkout
in `~/monoio` (package version 0.2.4). This extraction uses the MIT license;
copies of Monoio's license and the Tokio attribution are included.

The implementation is brought over for later adaptation. It retains the
six-level, 64-slot hierarchical timing wheel, millisecond rounding, intrusive
pinned entries, lazy registration and deadline extension, timeout polling
order, and interval missed-tick policies. The unused, unreferenced upstream
`wheel/stack.rs` is not copied; the active wheel uses `utils::linked_list`.

Extraction changes:

- Promote `time` to the `shep-smp-time` crate root and rewrite internal paths.
  Replace Monoio's `ready!` and `poll_fn` support with their standard-library
  equivalents. Keep `pin-project-lite` as the only normal dependency.
- Extract the four local methods of Monoio's driver interface (`with`,
  `submit`, `park`, `park_timeout`) so the timer wrapper can compile before
  Shep's io_uring reactor exists. No I/O backend is supplied here. The optional
  upstream `sync`/unpark feature and runtime-builder machinery are not copied.
- Make `TimeDriver::new(driver)` public, using the existing monotonic clock.
  `TimeDriver::with` installs a scoped local timer handle instead of using
  Monoio's enclosing runtime context. It restores the previous handle on unwind.
- Adapt examples and links to the standalone API, without Monoio's runtime
  macros. Correct inherited documentation that claimed cross-thread timers,
  virtual-clock APIs, or active shutdown behavior absent from this checkout.
- Preserve upstream unit tests for the wheel and linked-list helper, alongside
  integration tests of sleeps, resets, cancellation, timeouts, intervals, and
  composition with `shep-smp-exec`. Remove the unused `loom` test condition.
  In the linked-list test fixture's `Link::pointers`, use a shared reference to
  the interior-mutable links instead of constructing a mutable reference to
  the entire shared entry, correcting a Miri aliasing failure in that fixture.
- Apply workspace formatting and retain the edition-2021 unsafe operations
  under `allow(unsafe_op_in_unsafe_fn)`. Allow the upstream wheel's nested-if
  style rather than changing its control flow for Clippy.

Behavior deliberately retained for subsequent changes:

- Timer advancement happens after `park`/`park_timeout`; `submit` only forwards
  to the underlying driver. Busy-loop timer advancement is not changed here.
- A pending timer retains its first registered waker until it fires; polling it
  with another task's waker does not replace that registration in this checkout.
- `TimeDriver::drop` has no active timer shutdown. Outstanding sleeps retain
  their state but cannot advance once the driver is gone. Drop timer consumers
  before the driver.

This crate does not change the existing `shep-smp` runtime, add io_uring
operations, or redesign the timer implementation.
