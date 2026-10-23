# Storage I/O

Shep's storage consists of three independently usable infrastructure crates and
`shep-storage`, which owns the record formats and recovery protocols. SlateDB is
not a dependency of this infrastructure.

## Components

| Crate | Contract | Other uses |
| --- | --- | --- |
| `shep-smp-io` | Owned-buffer positional file I/O on a local completion driver | File serving, file copying, random-access storage |
| `shep-smp-io-scheduler` | Bounded admission, weighted service, and shared physical-device capacity | Foreground reads competing with maintenance or bulk transfers |
| `shep-append` | Buffered byte appends with written and durable positions | Journals, ingestion spools, append-only files |
| `shep-storage` | Framed records, segments, indexes, crash recovery, and KV metadata | Embedded persistent logs |

The dependencies point down this table. The infrastructure does not depend on
`shep-smp`, Raft, or the WAL service. A single Tokio `LocalRuntime`, or a `LocalSet`
on another Tokio runtime, can use it directly. Shep creates the driver in each
core's storage initializer.

The crates target Linux and require io_uring, using the `io-uring` crate for the
kernel interface. Driver initialization returns an error if ring creation or
completion-wakeup setup fails; storage startup propagates that error. There is
no blocking data backend. Metadata operations implemented with synchronous
filesystem APIs use bounded Tokio blocking helpers.

## Files and buffer ownership

`FileIo` is the interface implemented by raw files and scheduled files. It
provides positional reads and writes, synchronization, allocation, truncation,
and file length. It intentionally supplies no submission-order guarantee.
Conflicting file operations require explicit caller-managed dependencies.

A read or write takes ownership of a `Buffer` and returns it with the operation's
result. Buffers have stable allocations, initialized bytes, and explicit
alignment. Raw reads and writes may complete short. The driver retains buffers,
descriptors, and admission permits until actual completion, even when a waiting
future is dropped. `File::close` stops operations through its clones and waits
for work on that file, without synchronizing it or draining unrelated files.

Direct I/O is explicit and is never silently changed to buffered I/O. Alignment
is obtained from `STATX_DIOALIGN`, with a conservative 4096-byte Linux-filesystem
fallback when the query is unavailable. Invalid request alignment is rejected.
Applications using filesystems with other requirements should verify support.
The record-log data files use direct I/O by default; small metadata files remain
buffered. `StorageConfig::direct` selects the data-file policy.

## Admission and physical devices

Create one `Device` per physical-device domain and clone it into the schedulers
using that device. Shep's `StorageConfig` contains this shared handle, so cloning
the configuration across Raft cores shares the budget. The implementation does
not infer physical-device topology from paths.

A domain limits in-flight operations and bytes. Optional byte and operation
rates provide measured service limits, with bursts bounded by the domain's
capacities. Each scheduler separately limits queued work. Callers awaiting queue
space retain ownership of their requests; this is not an unlimited internal
submission queue. Requests larger than the byte capacity are rejected.

Classes have explicit shares. Weighted virtual service is accounted across all
classes registered in the shared domain, while dispatch remains local. Each
class is FIFO; independent classes and files may overlap. Cancellation of a
queued waiter removes its reservation. Once execution starts, an operation continues
independently of its waiting future, retaining capacity until completion. A
granted reservation that has not started execution can still be canceled.

The initial service estimate charges the larger of request bytes and 4096 bytes;
metadata operations therefore consume service and operation capacity too. This
is an initial policy, not an automatically calibrated device model. A mutex
protects the small shared capacity and fairness state; there is no central I/O
thread. Per-class metrics expose queue time, execution time, bytes, completed
operations, and queued/executing counts. Execution time includes downstream
admission and completion delivery, not just device execution.

Storage uses separate WAL, read, metadata, and recovery handles. Recovery has
lower shares. Dependencies must become ready *before* entering the scheduler;
waiting for a preceding write while holding its required capacity can deadlock.

## Append files

`AppendFile<F: FileIo>` operates on bytes, with no record framing or segment
layout. Opening requires a caller-validated logical end. Existing bytes count
as written; durability is established by synchronization rather than assumed
from validation alone.

Its positions are:

- `appended`: accepted into application buffers;
- `written`: the contiguous prefix of successfully completed writes;
- `durable`: the prefix covered by successful synchronization.

`sync_through(position)` waits for the relevant writes and then synchronizes the
file. Concurrent callers share a covering synchronization. A write followed by a
sync in the kernel submission queue alone would not provide this contract.

Chunks are dispatched when full, idle, explicitly requested, or under pool
pressure. Disjoint writes overlap. Partial direct-I/O pages are padded in an
owned buffer; subsequent overlapping writes wait for the preceding write and
any associated synchronization. The next head preserves the logical partial
page without mutating an in-flight buffer. Preallocation coordinates with the
file's writes and syncs, without blocking other files. A zero preallocation step lets writes extend the
file normally.

The buffer pool bounds both leased and cached chunk allocations. Pressure
reclaims idle partial heads so inactive files do not strand the budget. Each
open append file additionally retains at most `alignment - 1` tail bytes and
completion bookkeeping; these are outside the shared chunk budget. Consumers
must account for open-file state separately from chunk memory.

Write and synchronization errors are sticky. Out-of-order successful writes
cannot skip an earlier failure or advance the contiguous watermark past it.
Short writes are retried; an unaligned direct-I/O short write is an error.
`finish` excludes appends, drains writes, trims padding/preallocation, and syncs.
`truncate` excludes appends and metadata mutation and invalidates prior positions.
Byte appends may accept a prefix when their caller is canceled; record-oriented
consumers need a stronger transaction boundary.

## Storage integration and lifecycle

`shep-storage` uses `shep-append` for each active segment and typed scheduled
operations for reads, scans, indexes, and KV metadata. The former `Disk` worker
and `Disk::run` closure queue have been removed. `Storage::io()` replaces
`Storage::disk()`. Reads of active segments explicitly wait for their bytes to
be written. Log record appends and metadata transactions continue after their
waiting futures are dropped, preventing partial records or half-finished
truncations from preceding later operations.

Checksums, sparse indexes, recovery decisions, atomic metadata replacement, and
directory synchronization remain in `shep-storage`. On-disk record, index, and KV
formats are unchanged.

Normal shutdown is:

1. Stop producers and close the core's logs.
2. Call `Storage::shutdown()` to flush KV transactions, drain dispatched chunks,
   stop admission, and drain the completion driver.
3. End the Tokio runtime.

Raft core shutdown follows this sequence. Raw users call their scheduler's or
I/O driver's `shutdown` after finishing consumers. The driver also drains after
its last handle disappears. If a runtime is abruptly dropped with kernel I/O
in flight, the driver's destructor performs a synchronous safety drain before
freeing kernel-owned allocations. This is a memory-safety fallback, not a
replacement for graceful shutdown or a persistence guarantee.

## Validation and measurement

Deterministic file implementations exercise reordered completions, early write
failures, synchronization failures, partial-page dependencies, short writes, and
dropped waiters. Real-file tests exercise io_uring and direct I/O.
Scheduler tests cover weights, cancellation, and a device budget shared by
independent core runtimes. Storage tests cover existing buffered formats plus
direct-I/O recovery, truncation, and reads racing segment close.

The benchmark example supports durable appends, streaming appends, and durable
appends mixed with four uncached SST-like range readers:

```sh
cargo run --release -p shep-storage --example io_bench -- 8 300 4096 durable direct
cargo run --release -p shep-storage --example io_bench -- 32 1000 4096 stream direct
cargo run --release -p shep-storage --example io_bench -- 8 300 4096 mixed direct
cargo run --release -p shep-storage --example io_bench -- 8 300 4096 durable buffered
```

It reports payload throughput and p50/p99/p99.9 operation
latency, plus read p99 for the mixed workload. Streaming operation latency covers
buffer acceptance; its throughput includes the final synchronization. Creation
and initial metadata writes are excluded. This is a closed-loop workload with
one operation outstanding per log. It does not establish open-loop overload
latency, CPU efficiency, or performance parity with Redpanda.

### Local measurement sample

On the development VM, one before/after run of eight logs, 300 durable 4096-byte
appends per log, gave:

| Path | Payload throughput | p99 append + sync |
| --- | ---: | ---: |
| Previous FIFO worker, buffered files | 3.2 MiB/s | 33.88 ms |
| New io_uring path, direct files | 14.4 MiB/s | 3.80 ms |
| New direct path plus four range readers | 11.4 MiB/s | 10.75 ms |

The mixed run completed 7116 reads with read p99 of 2.11 ms. These are individual
local samples under the same durability contract, not controlled hardware
characterization or a Redpanda comparison. The file buffering policy changed
alongside execution and scheduling, so these samples do not isolate each layer's
contribution.

For an equivalent real-file comparison with Redpanda's production byte appender,
use [the paired benchmark runner](../tools/io-bench/README.md). It matches the
write geometry and persistence boundaries, records CPU and physical I/O, and
checks raw latency samples. Its component-level results are distinct from the
full `shep-storage` example above.
