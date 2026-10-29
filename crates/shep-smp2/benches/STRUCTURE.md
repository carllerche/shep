# Structural layout changes

Compared with shep `5e5d7d84e001ac13c3998b15d130522d393b9a66`, using Monoio
`8675e9406ea7943c441d969e38c3f5fc198ef6d1` as a design reference. Measurements
use rustc 1.97.1, LLVM 22.1.6, x86_64-unknown-linux-gnu and Cargo's default
release profile without LTO. The tables below record the
layout figures and callback/allocation counts.

## Changes

- File stores its descriptor Rc and immutable alignment directly, removing the
  separately allocated FileInner and one descriptor-access indirection.
- The timer epoch lives once in shared Inner, outside the state RefCell. Handles
  contain only an Rc. Movable private SleepState uses an ordinary optional slab
  ID; public Sleep retains its !Unpin contract through a wrapper marker.
- Interval stores SleepState inline, removing its Box and pin indirection.
- Concrete operation data stays in Io::submit throughout bounded publication
  retries. The driver borrows it, constructs its SQE through a sealed interface,
  and returns only the identity or error. Enum requests borrow their fields to
  use the same opcode builders; typed requests never convert into that enum.
- TCP uses concrete request owners and an awaitable cancellation guard instead
  of an additional async helper and poll_fn. Filesystem open and descriptor close
  also use concrete owners. Public filesystem/network futures remain lazy.
- Op methods use their retained Io directly. Completion/release/cancel_target
  no longer accept an Io argument. Cross-driver cancellation is still rejected.
  Ready results and unchanged waiters skip waker cloning; callback paths retain
  post-clone and post-destruction rechecks outside state borrows.
- Lifecycle determines original-request retirement; Record no longer maintains
  the redundant own_done flag. Cancellation pins and resource retirement remain
  independent, and the record size is unchanged.

## Measured layouts and counts

| Object or operation | Before | After |
|---|---:|---:|
| File value | 16 B | 24 B |
| Separate Rc<FileInner> allocation | 32 B | absent |
| Timer handle | 24 B | 8 B |
| Sleep | 48 B | 32 B |
| Interval value | 32 B | 56 B |
| Separate interval Sleep allocation | 48 B | absent |
| TCP send/receive owner | 88 B, enum | 64 B, concrete |
| TCP send/receive cancellation guard | 96 B | 72 B |
| TcpStream read/write future | 360 B | 232 B |
| TcpStream write_all future | 376 B | 264 B |
| Fd::close future | 192 B | 64 B |
| File read_at/write_at future | 176 B | 176 B |
| I/O record / actual slab slot | 48 B | 48 B |
| Timer entry / actual slab slot | 56 B | 56 B |
| Allocations constructing an unpolled interval | 1 | 0 |
| Clones during 1,000 pending polls with the same waker | 1,000 | 1 |
| Drops during those pending polls, before retirement | 999 | 0 |
| Waker clones/drops on an already-completed poll | 1 / 1 | 0 / 0 |

Future sizes already include their nested owners. Interval value plus separate
sleep storage falls from 80 to 56 bytes. File value plus its two descriptor
wrappers falls from 72 to 48 bytes for one handle, excluding allocator overhead
and shared runtime storage. Each additional File clone grows by eight bytes;
the storage savings break even at four live handles, while eliminating an
allocation and an indirection still applies. Sharing the epoch saves 16 bytes
per Sleep and adds one 16-byte epoch to shared Inner.

Normal typed file submission/completion remains allocation-free with reserved
slab capacity and existing buffers. Unfinished abandonment still allocates one
input-retention box; completed abandonment and cancellation remain allocation-free.
These are storage and callback-count results, not throughput measurements.

## Reproduction and validation

Layouts come from the actual compiled library:

```sh
RUSTC_BOOTSTRAP=1 cargo rustc -p shep-smp2 --release --lib -- -Zprint-type-sizes
cargo test -p shep-smp2 --features allocation-tests
cargo nextest run -p shep-smp2 --lib --run-ignored ignored-only \
  -E 'test(teardown_aborts_on_fatal_error_or_panic_before_retirement)'
cargo clippy -p shep-smp2 --all-targets --features allocation-tests -- -D warnings
cargo check --workspace --all-targets
cargo bench -p shep-smp2 --bench driver --bench runtime --bench timers -- --test
cargo weft test -p shep-smp2 --lib --test io_driver --test io_poll \
  --test net_tcp --test time_futures --test fs_files \
  --weft-strategy random --weft-runs 20 --weft-seed 29 --weft-jobs 4
```

Native validation passes 224 unit/integration tests, 37 doctests, and the separate
normally ignored teardown-abort subprocess test. The selected Weft suite passes
122 tests with the settings above; its native-only subprocess test is skipped.
All 82 driver/runtime/timer benchmark smoke cases pass, as do Clippy with warnings
denied and the workspace check. The allocation feature
is used only natively, never with Weft's simulated allocator.

Regression tests exercise callback counts, completion during waker Clone, clone
panic/retry, completion during replacement-waker Drop, cancellation identity
retention, driver teardown, and TCP cancellation/backpressure. A registered
interval is moved between storage locations and owners with distinct epochs;
construction, registration from reserved capacity, movement and ticking are
also checked for allocations. Compile-fail docs preserve !Unpin for Sleep and
verify that external types cannot implement OperationData.
