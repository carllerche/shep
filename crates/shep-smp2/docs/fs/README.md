# Native asynchronous filesystems

Filesystem operations are methods on `shep_smp2::io::Io`. Files, I/O handles,
and their futures stay on the owning core. No Tokio runtime is required.

```rust,ignore
let fs = runtime.io();
runtime.block_on(async move {
    let file = fs.open("segment", shep_smp2::fs::OpenOptions {
        create_new: true,
        direct: true,
        ..Default::default()
    }).await?;
    file.allocate(0, 1024 * 1024).await?;
    file.set_len(1024 * 1024).await?;
    // Use file.alignment() for buffer addresses, lengths and offsets.
    // Await writes before syncing their durability.
    file.sync_data().await?;
    file.close().await?;
    Ok::<(), std::io::Error>(())
})??;
```

## File contract

`FileIo` defines alignment, owned-buffer positional reads/writes, data/full sync,
allocation without length extension, truncation, and length queries. Raw `File`
implements it; schedulers can implement the same contract. It uses io2's Buffer,
which is a different type from the older shep-smp-io Buffer. Existing appender,
scheduler and storage consumers have not yet been migrated to this trait.

Transfers expose short results and return the input buffer even on validation,
admission, or kernel errors. Unpolled asynchronous operations do nothing. Eager
`submit_read_at` / `submit_write_at` publish immediately; consume their handles
or explicitly release them using the allocating Io. Abandoning a handle or
future does not undo accepted filesystem work.

Direct opens use O_DIRECT and query STATX_DIOALIGN on the metadata helper. Both
reported constraints must be nonzero powers of two. When alignment reporting is
unavailable, the conservative 4096-byte fallback matches the older file layer;
actual I/O errors remain visible. There is no buffered fallback. `Io::import_file`
queries an imported descriptor's flags and alignment. The older synchronous
`File::from_std` is for buffered descriptors only. Do not retain aliases that
change descriptor flags or perform conflicting operations.

Read/write, open, allocation, truncation, and file sync use io2 operations.
Length/alignment queries, directory creation/listing, rename, unlink and directory
sync use one lazily started worker per driver. Its bounded queue has
64 slots in addition to the executing job. Helper queue
saturation and io2 saturation return WouldBlock; no hidden admission backlog or
weighted scheduling is provided here. Opening may already have created or
truncated a file before a later alignment-query error or cancellation.

## Ordering and lifetime

Callers manage dependencies and exclude conflicting metadata mutations. In
particular, publishing metadata requires writing a temporary file, awaiting its
writes, syncing it, renaming it, then syncing the containing directory. Submission
order alone is not a durability dependency. Renaming between directories may
require syncing both directories.

`File::close` closes the shared admission gate, waits for accepted operations,
and consumes that clone. Canceling an admitted close leaves the gate closed.
Other clones and unconsumed completion results may retain the descriptor until
dropped. Close does not imply fsync.

`Io::close` stops admission for kernel and metadata work. `Driver::drain`
finishes accepted kernel work and joins the metadata worker, including jobs whose
waiters were dropped. Runtime shutdown uses this same teardown. Driver destruction
also joins the worker as a fallback; joining can block until accepted jobs finish.
The driver is shared with networking, so storage must finish its own transactions
and close its files before runtime shutdown. There is no separate filesystem
shutdown scope.

The runtime must keep driving the reactor while awaiting file close. Manual
drivers must deliver `Progress::wake`, which releases completion guards.
Scheduling weights, admission waiters, appender buffering, and transaction-level
drain remain responsibilities of the layers above the filesystem adapter.

Run `cargo nextest run -p shep-smp2` for
file, completion-lifetime, native runtime and socket regression coverage.
