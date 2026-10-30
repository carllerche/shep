# SMP orchestration

`Smp` owns worker threads, each with a native `Runtime`. It is a cloneable,
thread-safe handle without generic application state. `CoreId` is an index
within that SMP group, not a hardware CPU or OS thread ID.

```rust,no_run
use shep_smp2::{CoreId, Smp, SmpConfig};

let smp = Smp::start(SmpConfig::with_cores(4))?;
// Await this future from an executor, or an ordinary thread's block_on helper.
let reply = smp.submit_to(CoreId(2), || async {
    // Construct !Send service state here; only the closure and reply cross threads.
    shep_smp2::sleep(std::time::Duration::from_millis(1)).await;
    42
});
# drop(reply);
// Initialize services and establish readiness before accepting requests.
// Stop producers and complete required operations before this blocking call.
smp.shutdown()?;
# Ok::<(), shep_smp2::SmpError>(())
```

A submission enters the destination inbox on its first poll. Accepted work
continues when its reply future is dropped, as long as the workers remain
running. Each worker consumes at most 64 commands per transport tick, then
runs their futures as independent local tasks. One sender's submissions to one
worker start in order; completion order is unconstrained. Self-submission uses
a shard-local FIFO. A closure or future polling panic returns `Panicked` without
stopping the worker.

Within one SMP group, each directed pair of different shards has a fixed
128-entry SPSC ring. Requests and replies share these command rings, with
replies traveling in the reverse direction. Producers stage commands locally
and attempt publication at a threshold of 16, offering the entire staged backlog
to the ring. Transport ticks flush at most 64 commands per destination.
The runtime synchronously polls transport
between task turns and during its parking handshake; transport is not an
executor task. Ticks flush partial batches and retry overflow. Incoming sources
rotate under the command budget so busy peers leave other sources and local
tasks opportunities to run.

While a shard is busy, every tick attempts to flush. If it becomes otherwise
idle with staged work still blocked by ring capacity, its sleep has an
exponential retry deadline: 1 µs, 2 µs, 4 µs, and so on, capped at 1 second.
Useful transport progress, task execution, or I/O activity resets the backoff;
empty staging removes the retry deadline entirely. An early wake without
activity retains the existing deadline. Application timers can shorten any
wait, and incoming work or shutdown can interrupt it. Draining a ring does not
notify its producer about capacity. Progress therefore does not depend on
application replies or a periodic maintenance timer.

External threads, standalone runtimes, and workers belonging to another SMP
group use a separate unbounded MPSC channel. Both external submissions and
published SPSC batches notify the destination reactor directly through its
existing atomic parking/eventfd handshake. There is no transport task-waker
registration.
Self-submissions stay local. Endpoint ownership and staging remain on their
worker; command execution and destruction happen outside transport borrows.

The rings bound transport storage, not outstanding work. Local overflow and
external ingress are unbounded; services provide admission control. The mesh
uses O(shards²) ring storage. Closing a worker rejects new external ingress,
cancels queued work, and wakes peer producers to retire their overflow and ring
endpoints. Replies use a one-result MPSC channel and `AtomicWaker`, including
when cancellation races with notification.

Startup waits for runtime construction, not service initialization. A startup
failure stops and joins all started threads. CPU affinity is best effort.
`shutdown` closes inboxes, cancels work, retires runtime resources and joins
threads; concurrent shutdown calls wait for teardown and return its saved
result. Pending replies resolve on cancellation. Worker failures close that
worker's inbox and are reported by shutdown; workers are not restarted.

Startup and shutdown block and must run outside native runtimes and SMP workers.
When using another async runtime, move these calls to an ordinary thread.
Dropping the last handle requests stop without joining, since destruction can
happen on a worker. Explicit shutdown is required to observe teardown completion
and errors. Worker-held handles can keep an SMP group alive until shutdown.

SMP does not own service state, route partitions, manage service dependencies,
drain requests, or flush application buffers. Shutdown cancellation does not
establish durability. Applications must await required operations, including
terminal completion of cancelled indefinite I/O, before stopping workers.
Workers continue driving accepted I/O to terminal completion during teardown.
Network cancellation publication is best effort; shutdown can wait indefinitely
if cancellation fails and an operation does not naturally complete.

Run `cargo bench -p shep-smp2 --bench smp` to measure round-trip throughput with
2 and 4 shards and 1, 16, 256, or 2048 concurrent calls per shard. Each shard
submits to the next shard in a cycle; results return through the reverse rings.
Workers start outside the timed loop. Measurements include external launch/join
overhead, request and reply allocation, and local task scheduling. Larger batches
exercise ring saturation and staging; the single-call case exercises partial
batch flushing. Use Criterion's saved baselines when comparing changes.
