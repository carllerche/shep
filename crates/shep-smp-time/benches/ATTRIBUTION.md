# Attribution of the slab-owned timer costs

Measured 2026-10-23 against b5b9a22, with the same rustc 1.97.1 release profile
and CPU 0 affinity on this AMD EPYC 9554P KVM VM. The new implementation is the
initial slab migration before the [subsequent optimizations](OPPORTUNITIES.md).
Runtime code was not changed during this attribution analysis.
The measurements and ranges are saved in [attribution.json](attribution.json).

The steady-state cost comes from additional instructions, owner-state borrowing,
checked lookups, and longer chains of dependent loads. Lost readiness inlining
accounts for part of that increase. First registration additionally initializes
and first-touches storage that the old implementation initialized during sleep
construction. Capacity growth alone does not explain the cold registration cost.

## Steady-state operations

Native `perf stat` measurements use five alternating before/after trials.
Counters are enabled after fixture preparation and disabled before destruction,
with perf's acknowledged FIFO control. The handshake overhead is amortized over
16–33 million operations. Figures include the common Vec traversal and
`black_box` calls; they are not standalone instruction counts for just the API.

| Operation, 1,024 consumers | Before | Arena | Change |
|---|---:|---:|---:|
| Readiness: retired instructions/op | 14.01 | 36.01 | +157% |
| Readiness: user cycles/op | 2.59 | 8.19 | +216% |
| Pending poll: retired instructions/op | 40.01 | 50.01 | +25% |
| Pending poll: user cycles/op | 9.26 | 12.11 | +31% |

Instruction counts are nearly constant at 32,768 consumers too. Cycles rise
to 4.32 → 8.89 for readiness and 9.89 → 12.82 for pending polls. These fixed-work
timings differ from the Criterion timings in [ARENA.md](ARENA.md); counter and
elapsed-time ratios should not be substituted for each other on a shared VM.

Callgrind independently matches those instruction counts and reports the
following dynamic data-memory references. These count stack traffic as well as
timer data; they are not bytes transferred from DRAM or native PMU load counts.

| Operation | Reads/op, before → arena | Writes/op, before → arena |
|---|---:|---:|
| Readiness | 4 → 11 | 2 → 6 |
| Pending poll | 13 → 16 | 8 → 11 |

### What the machine code does

The registered readiness path previously read a state word directly from the
Sleep. The current path reads its key and owner, checks and updates the owner's
RefCell borrow flag, checks the slab's length, loads its allocation pointer,
computes `56 * (key - 1)`, checks the occupied discriminant, reads readiness, and
restores the borrow flag. Bounds and occupancy checks survive optimization.
These fields form a dependent load chain, so their cost is not described by
instruction count alone. Borrow-flag updates are ordinary single-threaded
stores, with no atomic operations or locks.

The old readiness body was inlined into the consumer's traversal. The new body
is an out-of-line call. Of its four additional writes, two update/restore the
borrow flag and two are call-frame traffic. Its seven additional reads consist
of five extra data reads and two call-frame reads on this compiled path.

A temporary copy with `#[inline]` on both `Sleep::is_elapsed` and
`TimerEntry::is_elapsed` reduces 36.01 instructions to 31.01 and 8.15 cycles to
6.86 in alternating trials. That removes about 23% of the extra instructions
and, in this experiment, about 23% of the extra cycles relative to the old path.
The dependent lookup and borrow remain. Annotating only the public wrapper
leaves the inner call out of line and does not reduce instructions. These
attributes have **not** been applied to the runtime working tree.

Pending polling already borrowed the old per-entry waker RefCell. The new
version instead borrows owner state and checks the slab, so the increase is
smaller than readiness's increase. Both versions call Future::poll out of line.
The new poll function saves/restores three additional stack words on the common
path, accounting for its three additional writes. Both common paths already
perform two borrow-flag stores. No waker clone or allocation occurs during
these repeated pending polls.

### Branches and cache behavior

Native branch misses are approximately 0.001/op for both implementations at
1,024 consumers, falling to 0.00004–0.00011/op at 32,768. The extra checks are
predictable in these workloads. Branch misprediction does not explain the
steady-state regression.

The VM's native `L1-dcache-load-misses` event returned zero for every workload;
those zeros are not useful cache evidence. Generic native `cache-misses` at
1,024 consumers remain below 0.0003/op for both versions. Pending polls at
32,768 have approximately 0.00787 → 0.00617 generic cache misses/op, while
readiness has 0.948 → 0.00740. These generic events do not establish a specific
cache level or latency, but do not support attributing the slowdown to a larger
number of those misses. Native dTLB miss rates also remain small in steady state.

Callgrind's simulated 32-KiB, 8-way, 64-byte-line L1 data cache reports
1.06 → 2.00 read misses/op for readiness at 1,024 consumers, and
2.12 → 2.00 for pending polling. The two readiness streams touch the Sleep and
the separate entry. The 32-MiB simulated last-level cache has no steady-state
data misses. This model omits the intermediate L2 and hardware prefetching;
these are simulated reference/miss counts, not hardware stall measurements.
No exact fraction of cycles can be assigned to individual loads from this data.

## First registration: separate page population from growth

Each row uses five trials of 32 fixtures, each containing 32,768 unpolled boxed
sleeps. Fixture construction is excluded. `register_reused` first fills and
drops temporary consumers against each owner before the measured registration.
It therefore reuses populated arena pages and free slots. Both versions receive
the same warm-up workload.

| Storage condition | ns/op | User cycles/op | Instructions/op | Page faults per 32K registrations |
|---|---:|---:|---:|---:|
| Before, fresh | 16.22 | 55.56 | 226.00 | 0 |
| Arena, fresh, reserve 1,024 | 43.71 | 99.29 | 303.05 | 464 |
| Arena, fresh, reserve 32,768 | 42.57 | 98.16 | 302.99 | 448 |
| Before, reused | 15.66 | 56.32 | 226.00 | 0 |
| Arena, reused | 20.64 | 70.35 | 301.97 | 0 |

Full reservation removes five capacity doublings, but barely changes this cold
case. Reserving a Vec does not initialize its slots or populate all its pages.
32,768 slots of 56 bytes occupy 448 pages of 4 KiB, matching the full-reservation
fault count. The previous inline entry was initialized with its Box during
excluded setup, so registration did not incur this page-population work.
The new lazy construction moves entry initialization into first registration.
Kernel page-fault handling is excluded from the user-cycle counter but included
in elapsed time. These costs depend on allocator/page reuse; this deliberately
fresh cohort is not a replacement for Criterion's repeated-fixture measurements.

Reusing the pool removes the page faults and roughly halves arena registration
time in this probe. The residual instruction increase is still about 34%.
Callgrind attributes approximately 48 additional instructions per registration
to the reset/insert path and 29 to Future::poll, with clock conversion unchanged.
The former includes slot initialization/free-list bookkeeping and checked wheel
links; the latter includes the first-poll sequence of owner borrows and lookups
around the callback-safe waker clone. Growth is not needed for either increase.

Valgrind's allocator copied old allocations during growth and consequently
reported more instructions/memory references than native perf for the growing
case. Do not use that copy volume as an estimate of native realloc copying:
native realloc can extend or remap allocations. The native reservation ablation
is the relevant evidence about growth in this run.

## Memory tradeoff

The previous DHAT measurements remain applicable: Sleep shrank from 120 to 48
bytes, plus a 56-byte arena slot per retained consumer. Full occupancy uses 104
payload bytes/consumer instead of 120, before common owner/container overhead.
The default pool reserves 56 KiB, and a 32K-slot high-water mark retains 1.75 MiB
after consumers disappear. Smaller aggregate live storage does not eliminate
the extra lookup stream or borrow-flag stores on status queries.

## Reproduce

The Rust example forbids unsafe code and uses only public timer APIs. The
runner requires Linux perf, taskset, and Python's standard library.

```sh
cargo build -p shep-smp-time --release --example arena_profile
python3 crates/shep-smp-time/benches/profile_arena.py \
  target/release/examples/arena_profile readiness 1024 32768 \
  --output /tmp/readiness.json
python3 crates/shep-smp-time/benches/profile_arena.py \
  target/release/examples/arena_profile register 32768 32 32768 \
  --output /tmp/register-reserved.json
python3 crates/shep-smp-time/benches/profile_arena.py \
  target/release/examples/arena_profile register_reused 32768 32 \
  --output /tmp/register-reused.json
```

Modes are `readiness`, `pending`, `register`, and `register_reused`; positional
arguments are mode, consumer count, rounds, and initial capacity. Readiness and
pending reuse one fixture; registration creates one fixture per round before
counters start. `--events` selects a comma-separated perf event list. Cache/TLB
measurements used `cache-references:u,cache-misses:u,dTLB-loads:u,dTLB-load-misses:u`
in separate runs. JSON contains normalized medians and raw CSV for every trial,
including event running percentages and unsupported-event markers. Alternate
before/after invocations when comparing versions.

For instruction/memory simulation, setup still runs with collection disabled:

```sh
taskset -c 0 valgrind --tool=callgrind --collect-atstart=no \
  '--toggle-collect=*profile_readiness*' --cache-sim=yes --branch-sim=yes \
  --I1=32768,8,64 --D1=32768,8,64 --LL=33554432,16,64 \
  --callgrind-out-file=/tmp/readiness.callgrind \
  target/release/examples/arena_profile readiness 1024 128
callgrind_annotate /tmp/readiness.callgrind
```

Use `*profile_pending*` or `*profile_register*` for the other modes. Registration
simulation used 4 rounds at 1,024 and 2 rounds at 32,768 consumers. Steady-state
simulation used 128 rounds at 1,024 and 16 rounds at 32,768. There are no actual
clock sleeps or timer-advancement operations in these profiles.

The before binary was built from an isolated b5b9a22 source snapshot. Copy the
same example into that snapshot, replacing `Timer::with_capacity(epoch, capacity)`
with `Timer::new(epoch)` and renaming that constructor argument `_capacity`.
Build both with the same compiler, dependency versions, and profile. The
standalone inline experiment uses the current source plus the two attributes
described above. Disassemble release binaries with `objdump -d -C` to inspect
the readiness and Future::poll paths rather than assuming guards/checks optimize
away.
