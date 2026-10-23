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
`wheel/stack.rs` is not copied; the active wheel uses `utils::linked_list`.

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
- Apply workspace formatting and retain upstream's edition-2021 unsafe
  operations under `allow(unsafe_op_in_unsafe_fn)`. Allow the wheel's nested-if
  style rather than changing its control flow for Clippy. Remove its redundant
  private `poll_at` alias.

Behavior retained for subsequent changes:

- A pending timer retains its first registered waker until it fires; polling it
  with another task's waker does not replace that registration in this checkout.
- There is no active timer shutdown. Outstanding sleeps retain their state but
  cannot advance after the owner is gone. Drop timer consumers before the owner.

This crate does not change the existing `shep-smp` runtime or add io_uring
operations. Runtime assembly and waiting belong to `shep-smp-reactor`.
