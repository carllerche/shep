# Native asynchronous filesystems

`Fs` binds filesystem work to a `shep-smp-io2::Io`. Files, the filesystem handle,
and their futures stay on the owning core. No Tokio runtime is required.

```rust,ignore
let fs = shep_smp_fs2::Fs::new(runtime.io(), Default::default())?;
runtime.block_on(async move {
    let file = fs.open("segment", shep_smp_fs2::OpenOptions {
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
    fs.shutdown().await;
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
actual I/O errors remain visible. There is no buffered fallback. `Fs::from_std`
queries an imported descriptor's flags and alignment. The older synchronous
`File::from_std` is for buffered descriptors only. Do not retain aliases that
change descriptor flags or perform conflicting operations.

Read/write, open, allocation, truncation, and file sync use io2 operations.
Length/alignment queries, directory creation/listing, rename, unlink and directory
sync use one lazily started worker per filesystem domain. Its bounded queue has
`Config::metadata_queue` slots in addition to the executing job. Helper queue
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

`Fs::drain` waits for accepted kernel and helper work in that domain. Producers
must be quiesced for a stable drain; appender buffers and future steps of storage
transactions are not accounted for by fs2. `Fs::shutdown` closes domain admission,
including its files, drains work and stops the helper. It does not close the
shared io2 driver, which may also serve sockets or another filesystem domain.
The runtime must keep driving the reactor while awaiting close/drain/shutdown.
Manual drivers must deliver `Progress::wake`, which releases completion guards.

Dropping the last domain owner joins its metadata worker as a fallback and can
block. Prefer explicit shutdown. Dropping the driver retains its existing kernel
retirement behavior. Scheduling weights, admission waiters, appender buffering,
and transaction-level drain remain responsibilities of the layers above fs2.

Run `cargo test -p shep-smp-fs2 -p shep-smp-io2 -p shep-smp2 -p shep-smp-net` for
file, completion-lifetime, native runtime and socket regression coverage.
