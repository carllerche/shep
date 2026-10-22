//! The write-behind side of a log's active segment, after Redpanda's
//! `segment_appender`.
//!
//! An append copies the record into a chunk-sized buffer from the core's
//! [`WritePool`] and returns. A chunk goes to the disk worker when it fills,
//! when a flush needs it, or when no append has come for the idle timeout.
//! Since the worker runs jobs in order, a sync submitted after a chunk covers
//! it, and nothing waits for writes to complete except the pool.
//!
//! Redpanda writes with O_DIRECT and must not touch a buffer the device may
//! be reading, so it copies a partly written page into a fresh chunk. Here a
//! dispatched chunk is simply handed off, and the next append starts a new
//! one.
//!
//! The state sits behind a mutex only because the idle timer is a tokio task;
//! everything else touches it from the core's thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::{Notify, OwnedSemaphorePermit};
use tokio::time::Instant;

use crate::disk::{Disk, DiskFile, WritePool};

pub(crate) struct Appender {
    state: Arc<Mutex<WriteState>>,
    timer: Arc<Notify>,
    pool: WritePool,
}

struct WriteState {
    disk: Disk,
    chunk_size: usize,
    prealloc_step: u64,
    active: Option<Active>,
    /// One past the last record the OS has, shared with the worker.
    written: Arc<AtomicU64>,
    last_append: Instant,
    timer_armed: bool,
    closed: bool,
}

/// The file being appended to.
struct Active {
    file: Arc<DiskFile>,
    /// Bytes handed to the worker; the head chunk continues from here.
    pos: u64,
    /// The file's length after preallocation.
    alloc_end: u64,
    head: Option<Chunk>,
    /// One past the last record whose bytes have all been handed off.
    dispatched: u64,
}

struct Chunk {
    buf: Vec<u8>,
    /// Returned to the pool when the chunk's write completes.
    permit: OwnedSemaphorePermit,
    /// One past the last record that ends in this chunk.
    end: Option<u64>,
}

/// What [`Appender::close`] leaves to the caller.
pub(crate) struct Closed {
    pub sync: futures::future::BoxFuture<'static, std::io::Result<()>>,
    pub size: u64,
}

pub(crate) struct AppenderConfig {
    pub chunk_size: usize,
    pub prealloc_step: u64,
    pub idle: Duration,
}

impl Appender {
    pub(crate) fn new(
        disk: Disk,
        pool: WritePool,
        config: AppenderConfig,
        written: Arc<AtomicU64>,
    ) -> Appender {
        let state = Arc::new(Mutex::new(WriteState {
            disk,
            chunk_size: config.chunk_size.clamp(1, pool.capacity()),
            prealloc_step: config.prealloc_step,
            active: None,
            written,
            last_append: Instant::now(),
            timer_armed: false,
            closed: false,
        }));
        let timer = Arc::new(Notify::new());
        tokio::spawn(idle_timer(
            Arc::downgrade(&state),
            timer.clone(),
            config.idle,
        ));
        Appender { state, timer, pool }
    }

    /// Starts appending to a new, empty file whose first record is `next`.
    pub(crate) fn activate(&self, file: Arc<DiskFile>, next: u64) {
        let mut ws = self.state.lock().unwrap();
        debug_assert!(ws.active.is_none());
        ws.active = Some(Active {
            file,
            pos: 0,
            alloc_end: 0,
            head: None,
            dispatched: next,
        });
    }

    pub(crate) fn is_active(&self) -> bool {
        self.state.lock().unwrap().active.is_some()
    }

    /// Buffers one record, whose index is `index`, waiting for pool space
    /// if needed.
    pub(crate) async fn write(&self, header: &[u8], payload: &[u8], index: u64) {
        let mut parts = [header, payload];
        let mut part = 0;
        loop {
            {
                let mut guard = self.state.lock().unwrap();
                let ws = &mut *guard;
                let active = ws
                    .active
                    .as_mut()
                    .expect("append without an active segment");
                let mut full = false;
                if let Some(head) = &mut active.head {
                    while part < parts.len() && head.buf.len() < ws.chunk_size {
                        let n = (ws.chunk_size - head.buf.len()).min(parts[part].len());
                        let end = active.pos + (head.buf.len() + n) as u64;
                        if end > active.alloc_end {
                            // Grow the file ahead of the data, so writes
                            // don't extend it a little at a time.
                            let step = ws.prealloc_step.max(1);
                            active.alloc_end = end.div_ceil(step) * step;
                            ws.disk.set_len(&active.file, active.alloc_end);
                        }
                        head.buf.extend_from_slice(&parts[part][..n]);
                        parts[part] = &parts[part][n..];
                        if parts[part].is_empty() {
                            part += 1;
                        }
                    }
                    while part < parts.len() && parts[part].is_empty() {
                        part += 1;
                    }
                    if part == parts.len() {
                        head.end = Some(index + 1);
                    }
                    full = head.buf.len() == ws.chunk_size;
                }
                if full {
                    dispatch(ws);
                }
                if part == parts.len() {
                    ws.last_append = Instant::now();
                    let pending = ws.active.as_ref().is_some_and(|a| a.head.is_some());
                    if pending && !ws.timer_armed {
                        ws.timer_armed = true;
                        self.timer.notify_one();
                    }
                    return;
                }
            }
            let chunk_size = self.state.lock().unwrap().chunk_size;
            let permit = self.pool.acquire(chunk_size).await;
            let mut ws = self.state.lock().unwrap();
            let active = ws
                .active
                .as_mut()
                .expect("append without an active segment");
            if active.head.is_none() {
                active.head = Some(Chunk {
                    buf: Vec::with_capacity(chunk_size),
                    permit,
                    end: None,
                });
            }
        }
    }

    /// Hands the head chunk to the worker and submits an `fdatasync` after
    /// it. Returns the sync and one past the last record it covers, or `None`
    /// if no segment is active.
    pub(crate) fn sync(&self) -> Option<(impl Future<Output = std::io::Result<()>> + use<>, u64)> {
        let mut ws = self.state.lock().unwrap();
        dispatch(&mut ws);
        let active = ws.active.as_ref()?;
        Some((ws.disk.sync(&active.file), active.dispatched))
    }

    /// Hands the head chunk off so a read can see it; reads queue behind it.
    pub(crate) fn dispatch(&self) {
        dispatch(&mut self.state.lock().unwrap());
    }

    /// Bytes handed to the worker in the active file.
    pub(crate) fn dispatched_bytes(&self) -> u64 {
        let ws = self.state.lock().unwrap();
        ws.active.as_ref().map_or(0, |a| a.pos)
    }

    /// Stops appending to the active file: writes out the head, trims the
    /// preallocated tail and submits a sync of both.
    pub(crate) fn close(&self) -> Option<Closed> {
        let mut ws = self.state.lock().unwrap();
        dispatch(&mut ws);
        let active = ws.active.take()?;
        ws.disk.set_len(&active.file, active.pos);
        Some(Closed {
            sync: Box::pin(ws.disk.sync(&active.file)),
            size: active.pos,
        })
    }

    /// Stops the idle timer.
    pub(crate) fn shutdown(&self) {
        let mut ws = self.state.lock().unwrap();
        ws.closed = true;
        self.timer.notify_one();
    }
}

impl Drop for Appender {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn dispatch(ws: &mut WriteState) {
    let Some(active) = &mut ws.active else {
        return;
    };
    let Some(chunk) = active.head.take() else {
        return;
    };
    let Chunk { buf, permit, end } = chunk;
    let pos = active.pos;
    active.pos += buf.len() as u64;
    if let Some(end) = end {
        active.dispatched = end;
    }
    let written = ws.written.clone();
    ws.disk.write(&active.file, pos, buf, move |ok| {
        drop(permit);
        if let (true, Some(end)) = (ok, end) {
            written.fetch_max(end, Ordering::AcqRel);
        }
    });
}

/// Writes out a head chunk that has sat unwritten for the idle timeout.
async fn idle_timer(state: Weak<Mutex<WriteState>>, wake: Arc<Notify>, idle: Duration) {
    loop {
        wake.notified().await;
        loop {
            let deadline = {
                let Some(state) = state.upgrade() else { return };
                let mut ws = state.lock().unwrap();
                if ws.closed {
                    return;
                }
                if ws.active.as_ref().is_none_or(|a| a.head.is_none()) {
                    ws.timer_armed = false;
                    break;
                }
                ws.last_append + idle
            };
            tokio::time::sleep_until(deadline).await;
            let Some(state) = state.upgrade() else { return };
            let mut ws = state.lock().unwrap();
            if ws.closed {
                return;
            }
            if Instant::now() >= ws.last_append + idle {
                dispatch(&mut ws);
                ws.timer_armed = false;
                break;
            }
        }
    }
}
