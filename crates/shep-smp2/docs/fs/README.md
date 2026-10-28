# Local asynchronous filesystems

File constructors and namespace functions use the runtime entered on the current
thread. They resolve that context on first poll and panic if no runtime is
entered, like other context-dependent runtime APIs. Creating and dropping an
unpolled future performs no filesystem work. Once opened, a file retains its
owning I/O handle; its operations do not look up a new runtime.

```rust,no_run
use shep_smp2::{Runtime, fs::{self, File, OpenOptions}};

let mut runtime = Runtime::new(Default::default())?;
runtime.block_on(async {
    fs::create_dir_all("data").await?;
    let file = File::open("data/segment", OpenOptions {
        create_new: true,
        direct: true,
        ..Default::default()
    }).await?;
    file.allocate(0, 1024 * 1024).await?;
    file.set_len(1024 * 1024).await?;
    // Use file.alignment() for buffer addresses, lengths and offsets.
    // Await writes before syncing their durability.
    file.sync_data().await?;
    fs::sync_dir("data").await?;
    file.shutdown().await;
    Ok::<(), std::io::Error>(())
})??;
# Ok::<(), std::io::Error>(())
```

## File contract

`File` provides positional reads/writes, data/full sync, allocation without length
extension, truncation, and length queries. Clones share the descriptor and one
admission gate. Independent requests may run concurrently and have no implicit
ordering; callers exclude conflicting mutations.

Transfers own their `Buffer` and return `(Result<usize>, Buffer)`, including on
validation, admission, and kernel errors. Short transfers are exposed. Dropping
an admitted future abandons its result without undoing accepted work or disabling
the file. Operations retain their buffers and descriptors through completion.

`OpenOptions::default()` enables both reads and writes, without creating or
truncating the file. Direct I/O is explicit and never silently downgraded.
Direct opens use O_DIRECT and query STATX_DIOALIGN on the bounded worker. Both
reported constraints must be nonzero powers of two. When alignment reporting is
unavailable, a conservative 4096-byte fallback is used; actual I/O errors remain
visible. Buffered files report alignment one.

`File::from_std(file).await` imports a descriptor, querying its flags and any
direct-I/O alignment constraints. Do not retain aliases that change descriptor
flags or perform conflicting I/O. Opening may create or truncate a file before a
later alignment-query error or cancellation; those effects are not rolled back.

Read/write, open, allocation, truncation, and file sync use the I/O driver.
Length/alignment queries, directory creation/listing, rename, unlink and directory
sync use one lazily started worker per driver. Its queue has 64 slots in addition
to the executing job. Worker or driver saturation returns `WouldBlock`; there is
no hidden admission backlog or weighted scheduling.

## Ordering and lifetime

Publishing metadata requires writing a temporary file, awaiting its writes,
syncing it, renaming it, then syncing the containing directory. Submission order
alone is not a durability dependency. Renaming between directories may require
syncing both directories.

`File::shutdown(&self)` stops admission through all clones and waits for accepted
operations, including those whose futures were dropped. It returns `()`, does not
sync, and does not await descriptor closure. Canceling a polled shutdown leaves
the shared gate closed; another shutdown can finish waiting. Clones and retained
completion results keep the descriptor alive until dropped. The runtime must
continue driving completions while shutdown waits.

`Io::close` stops admission for kernel and worker jobs. Driver drain finishes
accepted kernel work and joins the worker, including abandoned jobs. Runtime
shutdown uses the same teardown. Driver destruction also joins the worker as a
fallback, which can block until accepted jobs finish. Storage should finish its
transactions and shut down its files before runtime shutdown. There is no
separate filesystem shutdown scope.

Scheduling, appender buffering, and transaction-level drain remain the
responsibility of layers above this adapter.
