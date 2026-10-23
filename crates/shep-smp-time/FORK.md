# Monoio timer adaptation

Adapted from `monoio/src/time/` and `monoio/src/utils/linked_list.rs` in
[Monoio](https://github.com/bytedance/monoio/tree/8675e9406ea7943c441d969e38c3f5fc198ef6d1/monoio/src),
commit `8675e9406ea7943c441d969e38c3f5fc198ef6d1`, from the local checkout
in `~/monoio` (package version 0.2.4). This extraction uses the MIT license;
copies of Monoio's license and the Tokio attribution are included.

The wheel, timer entries, sleep/timeout/interval futures, and intrusive list
come from Monoio. The six-level, 64-slot hierarchy, millisecond rounding,
pinned entries, lazy registration and deadline extension, timeout polling
order, and interval missed-tick policies are retained. The unused upstream
`wheel/stack.rs` is not copied; the active wheel uses a concrete timer-entry
list in `timer/entry/list.rs`, specialized from `utils::linked_list`.

Changes for Shep:

- Replace Monoio's `TimeDriver<D>` wrapper with a concrete local `Timer`. There
  is no driver trait, wrapped I/O backend, submit/park delegation, or runtime
  loop. `shep-smp-reactor` will own the executor, timer, and io_uring operations
  and choose when to advance time and wait for completions.
- Expose `Timer::new(now)`, `with`, `advance(now)`, and `next_wake()`. The reactor
  supplies the epoch and current monotonic time directly. No clock trait or
  mock waiting backend is necessary to exercise the timer deterministically.
- `next_wake()` reports an absolute wheel-processing time, which can precede
  the earliest user deadline when a coarse bucket needs cascading. Compute it
  from the wheel on demand instead of retaining the wrapper's wait bookkeeping.
- Release the wheel borrow before calling a waker during advancement. A wake
  callback can reset or drop another timer without conflicting with that borrow.
  Clamp backward time to the wheel's current elapsed tick.
- Search the top level starting after the current slot. Entries in that slot
  belong to the next rotation; considering it first could hide an earlier
  deadline in another slot, delaying expiration and invalidating cancellation's
  cached bucket lookup. Tests straddling the full wheel range caught this
  inherited upstream bug.
- Release the wheel borrow before dropping a cancelled entry's stored waker.
  Its destructor can cancel another sleep. Previously, that nested cancellation
  could panic and free an entry still linked in the wheel, causing a use-after-free
  after the panic was caught. Regression tests cover reentrant and panicking
  waker destructors.
- Use `Cell` for copyable completion results, initial deadlines, and list links.
  Link mutations use shared references instead of constructing mutable references
  to aliased nodes. Remove the outer entry `UnsafeCell`: `TimerShared` is never
  replaced or mutably referenced, and every aliased mutation uses the inner
  `Cell`/`RefCell` fields. Retain pinning and unlink-before-drop for the wheel's
  non-owning pointers.
- Specialize the intrusive list for `TimerShared`. Remove the unsafe `Link`
  trait, its implementation, generic pointer wrapper, conversion hooks,
  `ManuallyDrop`, and phantom type bookkeeping. Keep next/previous cells in
  the entry's wheel state and direct non-owning handles in bucket/pending lists.
  Insertion now explicitly requires an unsafe caller to retain the pinned owner
  until removal, replacing the generic trait's implicit lifetime assumption.
  Adapt the four upstream list tests to actual pinned timer entries, preserving
  FIFO, neighbor consistency, repeated removal, and absent-node coverage.
- Use safe pinned shared access in polling and resetting, and shared cancellation
  in `Drop`. Remove unchecked wheel indexing, unused list filtering/tail APIs,
  the unused `true_when` field, and inherited cache-line padding. Cell-only state
  helpers are private and safe; raw-handle methods retain the unsafe lifetime
  and detached-membership contracts.
- Promote `time` to the crate root; keep wheel and entry internals in a private
  `timer` module. Replace Monoio's `ready!` and `poll_fn` support with standard
  library equivalents. `pin-project-lite` is the only normal dependency.
- `Timer::with` installs a scoped local handle instead of using Monoio's runtime
  context. It restores the previous handle on return or unwind. Timers stay
  bound to the owner selected at construction.
- Adapt examples and links to the standalone API without Monoio runtime macros.
  Correct inherited claims about cross-thread timers, virtual-clock APIs, and
  active shutdown behavior absent from this checkout.
- Preserve upstream wheel and linked-list unit tests alongside integration tests
  of deterministic advancement, bucket cascading, rounding, cancellation,
  resets, timeouts, intervals, wake callbacks, and executor composition. Remove
  the unused `loom` test condition. In the linked-list test fixture, use a shared
  reference to the interior-mutable links instead of creating a mutable
  reference to the entire shared entry, correcting its Miri aliasing failure.
- Adapt Monoio's `runtime::tests::timer` and the cancellation/drop assertion in
  `tests/tcp_connect.rs::connect_timeout_dst` to deterministic timer-only tests.
  Replace io_uring waiting and external TCP connections with explicit advancement
  and a local pending operation. Add integration tests for every wheel boundary,
  repeated top-level rotations, shared-bucket cancellation, lazy extensions,
  pending-queue resets during wake callbacks, timeout cleanup/overflow, and all
  interval missed-tick policies. A seeded absolute-deadline model checks mixed
  registration/reset/drop/advance traces and exact wake counts.
- Deny `unsafe_op_in_unsafe_fn` and `unused_unsafe`, document each remaining
  unsafe operation, and remove broad unused-code suppressions. Safety comments
  describe the local borrow/lifetime protocol instead of upstream's mutexes and
  atomics. Apply workspace formatting; retain the wheel's nested-if style with
  a Clippy allowance. Remove its redundant private `poll_at` alias.

Behavior retained for subsequent changes:

- A pending timer retains its first registered waker until it fires; polling it
  with another task's waker does not replace that registration in this checkout.
- There is no active timer shutdown. Outstanding sleeps retain their state but
  cannot advance after the owner is gone. Drop timer consumers before the owner.

This crate does not change the existing `shep-smp` runtime or add io_uring
operations. Runtime assembly and waiting belong to `shep-smp-reactor`.
