# Local native TCP

`shep_smp2::net` binds TCP listeners and streams to `shep_smp2::io::Io` and runs
under `shep-smp2::Runtime`. It forbids unsafe code and has no Tokio dependency.

```rust,no_run
use shep_smp2::net::{Buffer, Config, TcpListener};
use shep_smp2::Runtime;

let mut runtime = Runtime::new(Default::default())?;
let mut listener = TcpListener::bind(
    &runtime.io(), "127.0.0.1:7001".parse().unwrap(), Config::default(),
)?;
runtime.block_on(async move {
    let mut stream = listener.accept().await?;
    let (result, mut buffer) = stream.read(Buffer::new(4096, 1)?).await;
    let n = result?;
    buffer.resize(n);
    let (result, _buffer) = stream.write_all(buffer).await;
    result?;
    stream.close().await
})??;
# Ok::<(), std::io::Error>(())
```

- `read` and `write` return named `Read` and `Write` futures which yield
  `(Result<usize>, Buffer)`, including on rejection
  or kernel errors. Short transfers are exposed; a nonempty read returning zero
  means EOF. Zero-length transfers return immediately.
- `write_all` takes an owned `Buffer` and returns a named `WriteAll` future
  yielding `(Result<()>, Buffer)`.
  It sends bounded ranges without copying, handling partial sends.
  Errors can follow partial progress, as with standard TCP `write_all`.
- `Config::max_buffer_size` defaults to 64 KiB and bounds each transfer and
  send in `write_all` (the whole buffer may exceed the limit). Oversized buffers
  passed to `read` or `write` return `InvalidInput`.
  A full driver SQ triggers bounded submission/reaping; stalled progress returns
  `WouldBlock`, preserving the unpublished input buffer. Outstanding requests are
  not capped by SQ capacity.
  There is no admission waiter, background write queue, or buffer pool.
- `split` borrows one exclusive reader and one exclusive writer. They can run
  concurrently; each direction permits one live operation future at a time.
  Dropping that future releases the borrow before kernel retirement; await
  `cancel()` before reusing the direction when that ordering matters. Streams
  and halves are local and are not clonable.
- `flush` checks connection state. It does not wait for abandoned writes or
  imply delivery or peer acknowledgement. Await required writes explicitly.
- `shutdown` sends FIN after completed writes and retains the read direction.
  It is idempotent. `close` consumes the stream, awaits write shutdown, then
  shuts down both directions and releases the descriptor. Applications requiring
  a peer response should half-close, read that response, and then close.
- `accept` returns a named `Accept` future. Canceling it preserves the listener.
- Futures are lazy. Dropping an admitted read, write, write-all, or accept
  requests cancellation and abandons its result **without shutting down the
  socket**. An unpolled future has no I/O effects; rejected operations leave
  the resource usable. The driver retains buffers and descriptors until the
  original terminal completion. Cancellation publication is best effort: try
  once, submit existing SQEs if full, then retry once. A second rejection is
  discarded without a software backlog. Drop may enter the kernel to submit
  existing work, but does not wait for the original operation to retire.
- Each named future has a consuming `cancel()` method. Awaiting it requests
  cancellation and collects the original terminal result, not merely the
  cancellation request's completion. Read/write cancellation returns the buffer.
  If cancellation wins, the result is `ECANCELED`; if completion wins, its actual
  success/error is returned. Accept may return a connection that won the race.
  Canceling before admission returns `ECANCELED` without submitting I/O.
  Dropping the cancellation future abandons its result and retains cleanup.
  If publication fails or cancellation cannot stop the operation, `cancel().await`
  waits for natural completion and can wait indefinitely.
- Cancellation does not roll back I/O. An abandoned receive may consume bytes;
  an abandoned send may transmit bytes. Await `cancel()` to collect the outcome
  before starting another operation in that direction. `WriteAll::cancel()`
  retires the current chunk and sends no further chunks. It returns success if
  the whole buffer completed; otherwise cancellation/errors can follow partial
  progress. Its result does not report the cumulative bytes already sent.

For example, keep a read future alive while racing it against a shutdown signal
(by borrowing the future), then consume it with `read.cancel().await` if shutdown
wins. After it returns, the write half can finish outstanding responses. Timing
out an owned read future instead drops it and discards any result that raced the
timeout; the connection stays open, but protocol bytes may have been consumed.

A timeout can borrow the operation so it remains available for cancellation:

```rust,no_run
use shep_smp2::{net::{Buffer, TcpStream}, timeout};
use std::{io, time::Duration};

async fn read_with_deadline(
    stream: &mut TcpStream,
    buffer: Buffer,
    limit: Duration,
) -> (io::Result<usize>, Buffer) {
    let mut read = stream.read(buffer);
    match timeout(limit, &mut read).await {
        Ok(result) => result,
        Err(_) => read.cancel().await,
    }
}
```

The deadline starts a cancellation attempt; retirement may take longer or wait
indefinitely if cancellation fails. Completion can still win, in which case
this example returns the actual successful read.

Socket-owner destruction is separate from operation cancellation. Socket shutdown
is synchronous and does not wait for network I/O. Runtime shutdown drops task
consumers before draining the driver. The worker continues submitting and reaping
all accepted operations, including published cancellations, until their terminal
completions arrive. It may wait indefinitely if cancellation fails and a surviving
socket has an operation that does not naturally complete. No timeout frees live
kernel inputs, and a cancel CQE alone does not retire its original operation.
Dropping a stream also shuts down its socket. The last descriptor owner then
publishes a detached io_uring CLOSE through the currently entered runtime,
even if an outstanding operation retained the descriptor beyond the stream's
lifetime. CLOSE runs on an io_uring worker
so a positive SO_LINGER setting does not block normal drop or ring submission.
Ordinary SQ fullness first triggers bounded submission/reaping, including for
detached closes. The driver must continue to be driven to finish accepted closes. If the driver
is gone, closed, failed, or currently borrowed, no runtime context is available,
or publication fails or bounded SQ progress is exhausted,
destruction falls back to a synchronous close; that fallback can still wait for
linger. The descriptor stores no runtime reference. Callers importing standard sockets must not retain
aliases that perform I/O or change socket flags.

Connect takes a numeric `SocketAddr`, starts a nonblocking connection through
safe `shep-sys` calls, waits using io_uring `PollAdd`, and checks `SO_ERROR`. It then
clears nonblocking mode for owned io_uring sends/receives. This avoids a queued
CONNECT starting after cancellation already shut down the socket. Bind/listen
setup is synchronous. DNS, provided buffers, multishot receive, and zero-copy
sends remain future work.

Run the native loopback tests:

```sh
cargo nextest run -p shep-smp2 --test net_tcp
```

Tests cover short sends, bounded chunking, half-close, EOF/reset, connection
refusal, owned-buffer rejection, lazy futures, canceled connection waits,
accept/read/write cleanup under SQ pressure, explicit cancellation and socket
reuse, completion races, partial write-all progress, and runtime shutdown with
live socket tasks.
