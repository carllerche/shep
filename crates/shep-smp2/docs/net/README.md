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

- `read` and `write` return `(Result<usize>, Buffer)`, including on rejection
  or kernel errors. Short transfers are exposed; a nonempty read returning zero
  means EOF. Zero-length transfers return immediately.
- `write_all` takes an owned `Buffer` and returns `(Result<()>, Buffer)`.
  It sends bounded ranges without copying, handling partial sends.
  Errors can follow partial progress, as with standard TCP `write_all`.
- `Config::max_buffer_size` defaults to 64 KiB and bounds each transfer and
  send in `write_all` (the whole buffer may exceed the limit). Oversized buffers
  passed to `read` or `write` return `InvalidInput`.
  Driver admission saturation returns `WouldBlock`, preserving the input buffer.
  There is no admission waiter, background write queue, or buffer pool.
- `split` borrows one exclusive reader and one exclusive writer. They can run
  concurrently; two operations on the same direction cannot overlap. Streams
  and halves are local and are not clonable.
- `flush` checks connection state. Each preceding write already awaited its
  kernel completion; flush does not imply delivery or peer acknowledgement.
- `shutdown` sends FIN after completed writes and retains the read direction.
  It is idempotent. `close` consumes the stream, awaits write shutdown, then
  shuts down both directions and releases the descriptor. Applications requiring
  a peer response should half-close, read that response, and then close.
- Futures are lazy. Dropping an admitted operation shuts down **the entire
  connection or listener**, requests cancellation when capacity permits, and
  releases its completion claim. This may leave partial network effects.
  Canceling an accept requires rebinding a listener to accept again. Unpolled
  futures have no effects. Rejected operations leave the resource usable.

Socket shutdown is synchronous and does not wait for network I/O. It ensures
pending accepts, sends, receives and connection readiness waits can retire even
when the single driver cancellation reserve is occupied. Operations retain their
owned descriptors and buffers until the driver reaps them. Runtime shutdown
drops task consumers before draining the driver, triggering this cleanup.
Dropping a stream also shuts down its socket. The last descriptor owner then
publishes a detached io_uring CLOSE through the currently entered runtime,
even if an outstanding operation retained the descriptor beyond the stream's
lifetime. CLOSE runs on an io_uring worker
so a positive SO_LINGER setting does not block normal drop or ring submission.
The driver must continue to be driven to finish accepted closes. If the driver
is gone, closed, full, or currently borrowed, or no runtime is entered,
destruction falls back to a synchronous close; that fallback can still wait for
linger. The descriptor stores no runtime reference. Callers importing standard sockets must not retain
aliases that perform I/O or change socket flags.

Connect takes a numeric `SocketAddr`, starts a nonblocking connection through
safe `shep-sys` calls, waits using io_uring `PollAdd`, and checks `SO_ERROR`. It then
clears nonblocking mode for owned io_uring sends/receives. This avoids a queued
CONNECT starting after cancellation already shut down the socket. Bind/listen
setup is synchronous. DNS, provided buffers, multishot receive, zero-copy sends,
and recoverable cancellation that preserves a connection remain future work.

Run the native loopback tests:

```sh
cargo nextest run -p shep-smp2 --test net_tcp
```

Tests cover short sends, bounded chunking, half-close, EOF/reset, connection
refusal, owned-buffer rejection, lazy futures, canceled connection waits,
accept/read/write cleanup with saturated cancellation capacity, and runtime
shutdown with live socket tasks.
