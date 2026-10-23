//! A format-independent append-only byte file, with explicit written and durable
//! positions. Uses any FileIo implementation, raw or scheduled. All state stays
//! on its Tokio local runtime. Record framing and restart recovery belong above.
use shep_smp_io::{Buffer, FileIo};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

/// Cloneable form of a sticky I/O error, shared by independent completion waiters.
#[derive(Clone, Debug)]
struct Failure(io::ErrorKind, String);
impl Failure {
    /// Capture a clonable failure so all waiters observe the same original error.
    fn from(e: io::Error) -> Self {
        Self(e.kind(), e.to_string())
    }
    /// Reconstruct the stored failure for an individual caller.
    fn error(&self) -> io::Error {
        io::Error::new(self.0, self.1.clone())
    }
}
/// A reusable local completion result for writes, overlap dependencies, and coalesced syncs.
#[derive(Default)]
struct Completion {
    /// Terminal result, retained so every dependency waiter can inspect it.
    result: RefCell<Option<Result<(), Failure>>>,
    /// Wake all dependency waiters after the result is published.
    changed: Notify,
}
impl Completion {
    /// Publish the shared write or sync result and wake every dependency waiter.
    fn finish(&self, result: Result<(), Failure>) {
        *self.result.borrow_mut() = Some(result);
        self.changed.notify_waiters();
    }
    /// Wait for a shared completion without consuming its result or missing a wakeup.
    async fn wait(&self) -> io::Result<()> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(result) = &*self.result.borrow() {
                return result.clone().map_err(|e| e.error());
            }
            changed.await;
        }
    }
}
/// Lets the pool dispatch idle file heads without owning or extending their lifetimes.
trait Reclaim {
    /// Dispatch buffered bytes so their pool lease can return after write completion.
    fn reclaim(&self);
}
/// Tracks allocations separately from active byte leases; cached buffers hold no lease.
struct PoolInner {
    /// Maximum bytes in pooled allocations and active chunk leases.
    capacity: usize,
    /// Byte leases held by buffered heads and dispatched writes.
    semaphore: Arc<Semaphore>,
    /// Bytes in all live pooled allocations, including cached free buffers.
    allocated: Cell<usize>,
    /// Reusable allocations whose active byte leases have been returned.
    free: RefCell<Vec<Buffer>>,
    /// Weak callbacks that dispatch idle heads when another file needs budget.
    reclaim: RefCell<Vec<Weak<dyn Reclaim>>>,
    /// Dispatched writebacks still holding work, including dependency waits.
    writes: Cell<usize>,
    /// Wake pool-drain waiters as dispatched writebacks retire.
    changed: Notify,
}
/// A shared local budget and reusable allocation pool. Pool pressure dispatches
/// idle partial heads so one inactive file cannot strand the entire budget.
#[derive(Clone)]
pub struct BufferPool(Rc<PoolInner>);
impl BufferPool {
    /// Create a local byte budget; capacity must be nonzero and fit a semaphore request.
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity > 0 && capacity <= u32::MAX as usize,
            "invalid buffer pool capacity"
        );
        Self(Rc::new(PoolInner {
            capacity,
            semaphore: Arc::new(Semaphore::new(capacity)),
            allocated: Cell::new(0),
            writes: Cell::new(0),
            changed: Notify::new(),
            free: RefCell::new(Vec::new()),
            reclaim: RefCell::new(Vec::new()),
        }))
    }
    /// Return the total byte budget shared by buffered heads and dispatched writes.
    pub fn capacity(&self) -> usize {
        self.0.capacity
    }
    /// Return budget bytes not currently leased, including bytes held in cached allocations.
    pub fn available(&self) -> usize {
        self.0.semaphore.available_permits()
    }
    /// Wait for dispatched writes only; buffered heads remain buffered.
    pub async fn drain(&self) {
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.0.writes.get() == 0 {
                return;
            }
            changed.await;
        }
    }
    /// Return allocated pooled bytes, including cached buffers, but excluding retained tails.
    pub fn allocated(&self) -> usize {
        self.0.allocated.get()
    }
    /// Lease and zero an aligned chunk, dispatching idle heads first when budget is tight.
    /// Reuse compatible allocations and evict cached buffers to keep allocation within budget.
    async fn get(&self, size: usize, alignment: usize) -> io::Result<Head> {
        if self.available() < size {
            let reclaim: Vec<_> = {
                let mut entries = self.0.reclaim.borrow_mut();
                entries.retain(|e| e.strong_count() > 0);
                entries.iter().filter_map(Weak::upgrade).collect()
            };
            for entry in reclaim {
                entry.reclaim();
            }
        }
        let permit = self
            .0
            .semaphore
            .clone()
            .acquire_many_owned(size as u32)
            .await
            .map_err(|_| io::Error::other("buffer pool closed"))?;
        let mut free = self.0.free.borrow_mut();
        let buffer = if let Some(i) = free
            .iter()
            .position(|b| b.capacity() == size && b.alignment() >= alignment)
        {
            let mut buffer = free.swap_remove(i);
            buffer.resize(size);
            buffer.fill(0);
            buffer
        } else {
            while self.0.allocated.get() + size > self.capacity() {
                let b = free
                    .pop()
                    .expect("free buffers account for available pool space");
                self.0.allocated.set(self.0.allocated.get() - b.capacity());
            }
            let buffer = Buffer::new(size, alignment)?;
            self.0.allocated.set(self.0.allocated.get() + size);
            buffer
        };
        Ok(Head {
            buffer,
            used: 0,
            dirty: false,
            permit,
        })
    }
    /// Cache a returned allocation; its separate lease controls when budget becomes available.
    fn put(&self, buffer: Buffer) {
        self.0.free.borrow_mut().push(buffer);
    }
}
/// Buffering and space-reservation policy for one file; record layout stays above this layer.
#[derive(Clone, Debug)]
pub struct Config {
    /// Pooled chunk size; clamped to pool capacity and rounded down to alignment on open.
    pub chunk_size: usize,
    /// Reserve space in byte increments; zero lets writes extend the file.
    pub preallocate: u64,
    /// Maximum idle interval before a dirty partial head is dispatched.
    pub idle: Duration,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            chunk_size: 64 << 10,
            preallocate: 2 << 20,
            idle: Duration::from_millis(100),
        }
    }
}
/// Logical byte ends: durable <= written <= appended; preallocation and padding are excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Positions {
    /// Logical byte end accepted by append calls, including buffered bytes.
    pub appended: u64,
    /// Contiguous logical byte end whose writes have completed.
    pub written: u64,
    /// Logical byte end covered by a completed synchronization.
    pub durable: u64,
}
/// One leased chunk, owned by either the current append head or its detached writeback.
struct Head {
    /// Aligned chunk retained by the head or detached writeback.
    buffer: Buffer,
    /// Logical prefix in this chunk, including any retained partial-page bytes.
    used: usize,
    /// The head contains newly appended bytes that still need dispatch.
    dirty: bool,
    /// Byte lease held until the chunk returns from writeback or is dropped.
    permit: OwnedSemaphorePermit,
}
/// A dispatched logical extent, retired in order to advance the contiguous written end.
struct Write {
    /// Exclusive logical byte end covered by this dispatched extent.
    end: u64,
    /// Shared result used for ordered retirement and overlapping-write dependencies.
    completion: Rc<Completion>,
}
/// Mutable local file state; ordered retirement hides out-of-order write completions.
struct State {
    /// Accepted, contiguous written, and durable logical byte ends.
    positions: Positions,
    /// File extent reserved so far, which may extend beyond the logical append end.
    allocated: u64,
    /// Current leased chunk, if any, accumulating bytes before dispatch.
    head: Option<Head>,
    /// Final partial-page prefix retained for the next write; at most alignment-1 bytes.
    /// Kept outside the pooled chunk budget so dispatch can return its full chunk.
    tail: Vec<u8>,
    /// Exclusive logical byte end last handed to writeback.
    dispatched: u64,
    /// Dispatched extents in logical order, regardless of completion order.
    writes: VecDeque<Write>,
    /// Last dispatched physical end and completion, guarding partial-page rewrites.
    overlap: Option<(u64, Rc<Completion>)>,
    /// Logical target and completion of the active synchronization, if any.
    sync: Option<(u64, Rc<Completion>)>,
    /// First asynchronous failure; poisons subsequent file operations.
    failure: Option<Failure>,
    /// Latest accepted-write timestamp used by idle dispatch.
    last_append: Instant,
    /// Reject new appends once finishing begins.
    closed: bool,
    /// Finish completed successfully; repeated finish calls can return immediately.
    finished: bool,
    /// Finish or truncate owns file mutation exclusion; sync callers must wait.
    mutating: bool,
    /// Invalidates position waiters when truncation changes the file's byte history.
    generation: u64,
}
/// Shared local file ownership; weak references let timers and writeback avoid retaining it.
struct Inner<F: FileIo> {
    /// Underlying owned-I/O handle, raw or scheduled.
    file: F,
    /// Local chunk budget shared with other appenders.
    pool: BufferPool,
    /// Normalized chunk, preallocation, and idle-dispatch policy.
    config: Config,
    /// Mutable local positions and dependency state; no borrow spans an await.
    state: RefCell<State>,
    /// Serialize byte acceptance, truncation, and finishing across awaits.
    append_lock: tokio::sync::Mutex<()>,
    /// Wake position and mutation waiters when progress changes.
    changed: Notify,
    /// Wake the idle timer on append activity or final-handle destruction.
    timer: Rc<Notify>,
    /// Allow reclaim callbacks to dispatch without permanently retaining this file.
    weak: Weak<Inner<F>>,
}
/// Cloneable handle to one byte file. Appends are serialized internally; writes
/// of disjoint chunks overlap. `sync_through` may run concurrently with appends.
pub struct AppendFile<F: FileIo> {
    /// Shared local file state; cloned handles serialize byte acceptance.
    inner: Rc<Inner<F>>,
}
impl<F: FileIo> Clone for AppendFile<F> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
impl<F: FileIo> AppendFile<F> {
    /// Attach to an existing logical end, retaining its final partial-page prefix.
    /// Recovered bytes start written; durability must be established explicitly.
    /// `end` is a logical end validated by the caller's recovery procedure.
    pub async fn open(file: F, pool: BufferPool, mut config: Config, end: u64) -> io::Result<Self> {
        let alignment = file.alignment();
        if alignment == 0
            || !alignment.is_power_of_two()
            || pool.capacity() < alignment
            || config.idle.is_zero()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid append-file configuration",
            ));
        }
        config.chunk_size =
            config.chunk_size.clamp(alignment, pool.capacity()) / alignment * alignment;
        let allocated = file.len().await?;
        if end > allocated {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "logical end exceeds file length",
            ));
        }
        let tail_len = (end % alignment as u64) as usize;
        let tail = if tail_len == 0 {
            Vec::new()
        } else {
            let (result, buffer) = file
                .read_at(Buffer::new(alignment, alignment)?, end - tail_len as u64)
                .await;
            if result? < tail_len {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            buffer[..tail_len].to_vec()
        };
        let inner = Rc::new_cyclic(|weak| Inner {
            file,
            pool: pool.clone(),
            config,
            state: RefCell::new(State {
                positions: Positions {
                    appended: end,
                    written: end,
                    durable: 0,
                },
                allocated,
                head: None,
                tail,
                dispatched: end,
                writes: VecDeque::new(),
                overlap: None,
                sync: None,
                failure: None,
                last_append: Instant::now(),
                closed: false,
                finished: false,
                mutating: false,
                generation: 0,
            }),
            append_lock: tokio::sync::Mutex::new(()),
            changed: Notify::new(),
            timer: Rc::new(Notify::new()),
            weak: weak.clone(),
        });
        let reclaimer: Rc<dyn Reclaim> = inner.clone();
        pool.0.reclaim.borrow_mut().push(Rc::downgrade(&reclaimer));
        tokio::task::spawn_local(idle_timer(Rc::downgrade(&inner), inner.timer.clone()));
        Ok(Self { inner })
    }
    /// Snapshot logical append, contiguous write-completion, and durable byte positions.
    pub fn positions(&self) -> Positions {
        self.inner.state.borrow().positions
    }
    /// Hand the current dirty head to detached writeback without waiting or synchronizing.
    pub fn dispatch(&self) {
        dispatch(&self.inner);
    }
    /// Return the first recorded failure, which poisons subsequent file operations.
    fn check(&self) -> io::Result<()> {
        check(&self.inner.state.borrow())
    }
    /// Retain the first asynchronous failure and wake position waiters.
    fn remember(&self, result: &io::Result<()>) {
        if let Err(e) = result {
            let mut state = self.inner.state.borrow_mut();
            if state.failure.is_none() {
                state.failure = Some(Failure(e.kind(), e.to_string()));
            }
            self.inner.changed.notify_waiters();
        }
    }
    /// Copy bytes into pooled heads and return the new logical end, without a durability wait.
    /// Cancellation may leave an accepted prefix; record-level atomicity belongs to the caller.
    pub async fn append(&self, bytes: &[u8]) -> io::Result<u64> {
        let result = self.append_inner(bytes).await;
        if let Err(e) = &result {
            let mut state = self.inner.state.borrow_mut();
            if state.failure.is_none() && !(state.closed && e.kind() == io::ErrorKind::BrokenPipe) {
                state.failure = Some(Failure(e.kind(), e.to_string()));
            }
        }
        result
    }
    /// Serialize byte acceptance, retaining a partial-page prefix when acquiring a new head.
    /// Grow preallocation only after conflicting writes and syncs finish.
    async fn append_inner(&self, mut bytes: &[u8]) -> io::Result<u64> {
        let _lock = self.inner.append_lock.lock().await;
        self.check()?;
        if self.inner.state.borrow().closed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        while !bytes.is_empty() {
            let needs_head = self.inner.state.borrow().head.is_none();
            if needs_head {
                let mut head = self
                    .inner
                    .pool
                    .get(self.inner.config.chunk_size, self.inner.file.alignment())
                    .await?;
                let mut state = self.inner.state.borrow_mut();
                head.used = state.tail.len();
                head.buffer[..head.used].copy_from_slice(&state.tail);
                state.head = Some(head);
            }
            let (n, end, allocated) = {
                let state = self.inner.state.borrow();
                let head = state.head.as_ref().unwrap();
                let n = bytes.len().min(self.inner.config.chunk_size - head.used);
                let end = state
                    .positions
                    .appended
                    .checked_add(n as u64)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
                (n, end, state.allocated)
            };
            let physical_end = end.div_ceil(self.inner.file.alignment() as u64)
                * self.inner.file.alignment() as u64;
            if physical_end > allocated && self.inner.config.preallocate != 0 {
                // Metadata changes exclude outstanding writes and syncs on this file.
                self.dispatch();
                let (target, sync) = {
                    let s = self.inner.state.borrow();
                    (s.dispatched, s.sync.clone())
                };
                self.wait_written(target).await?;
                if let Some((_, sync)) = sync {
                    sync.wait().await?;
                }
                let step = self
                    .inner
                    .config
                    .preallocate
                    .max(self.inner.file.alignment() as u64);
                let new_end = physical_end
                    .div_ceil(step)
                    .checked_mul(step)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
                self.inner
                    .file
                    .allocate(allocated, new_end - allocated)
                    .await?;
                self.inner.file.set_len(new_end).await?;
                self.inner.state.borrow_mut().allocated = new_end;
                // Pressure or dispatch may have handed off the old head.
                continue;
            }
            let full = {
                let mut state = self.inner.state.borrow_mut();
                let head = state.head.as_mut().unwrap();
                head.buffer[head.used..head.used + n].copy_from_slice(&bytes[..n]);
                head.used += n;
                head.dirty = true;
                let full = head.used == self.inner.config.chunk_size;
                state.positions.appended = end;
                state.last_append = Instant::now();
                full
            };
            bytes = &bytes[n..];
            if full {
                self.dispatch();
            } else {
                self.inner.timer.notify_one();
            }
        }
        Ok(self.positions().appended)
    }
    /// Wait for the contiguous completed-write prefix; this does not synchronize.
    pub async fn wait_written(&self, target: u64) -> io::Result<()> {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.inner.state.borrow();
                check(&state)?;
                if state.positions.written >= target {
                    return Ok(());
                }
                if target > state.positions.appended {
                    return Err(io::Error::from(io::ErrorKind::InvalidInput));
                }
            }
            changed.await;
        }
    }
    /// Dispatch and synchronize at least the requested logical prefix, sharing active syncs.
    /// Started syncs survive cancellation; truncation invalidates an outstanding position.
    pub async fn sync_through(&self, target: u64) -> io::Result<()> {
        let generation = self.inner.state.borrow().generation;
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.inner.state.borrow().mutating {
                changed.await;
                continue;
            }
            self.check()?;
            let completion = {
                let state = self.inner.state.borrow();
                if state.generation != generation {
                    return Err(io::Error::other("position invalidated by truncation"));
                }
                if target > state.positions.appended {
                    return Err(io::Error::from(io::ErrorKind::InvalidInput));
                }
                if state.positions.durable >= target {
                    return Ok(());
                }
                state.sync.as_ref().map(|(_, c)| c.clone())
            };
            if let Some(completion) = completion {
                completion.wait().await?;
                continue;
            }
            self.dispatch();
            let (upto, completion) = {
                let mut state = self.inner.state.borrow_mut();
                let upto = state.dispatched;
                let completion = Rc::new(Completion::default());
                state.sync = Some((upto, completion.clone()));
                // Subsequent rewrites of this partial page must wait for its sync.
                if let Some((end, _)) = state.overlap.as_ref() {
                    state.overlap = Some((*end, completion.clone()));
                }
                (upto, completion)
            };
            let this = self.clone();
            let done = completion.clone();
            tokio::task::spawn_local(async move {
                let result = async {
                    this.wait_written(upto).await?;
                    this.inner.file.sync_data().await
                }
                .await
                .map_err(Failure::from);
                {
                    let mut state = this.inner.state.borrow_mut();
                    if result.is_ok() {
                        state.positions.durable = state.positions.durable.max(upto);
                    } else if state.failure.is_none() {
                        state.failure = result.clone().err();
                    }
                    state.sync = None;
                }
                done.finish(result);
                this.inner.changed.notify_waiters();
            });
            completion.wait().await?;
        }
    }
    /// Finish accepted writes, trim direct-I/O padding/preallocation, and sync.
    /// The detached operation keeps running if the caller stops waiting.
    pub async fn finish(&self) -> io::Result<()> {
        let this = self.clone();
        tokio::task::spawn_local(async move {
            let result = this.finish_inner().await;
            this.remember(&result);
            result
        })
        .await
        .map_err(io::Error::other)?
    }
    /// Exclude appends, stop new acceptance, then drain before trimming and synchronizing.
    async fn finish_inner(&self) -> io::Result<()> {
        let _lock = self.inner.append_lock.lock().await;
        self.check()?;
        if self.inner.state.borrow().finished {
            return Ok(());
        }
        self.inner.state.borrow_mut().closed = true;
        self.inner.timer.notify_one();
        self.dispatch();
        let target = self.positions().appended;
        let pending = self.inner.state.borrow().sync.clone();
        self.wait_written(target).await?;
        if let Some((_, pending)) = pending {
            pending.wait().await?;
        }
        self.inner.file.set_len(target).await?;
        self.inner.file.sync_data().await?;
        let mut state = self.inner.state.borrow_mut();
        state.allocated = target;
        state.positions.durable = target;
        state.finished = true;
        Ok(())
    }
    /// Truncate under per-file exclusion. Outstanding positions are invalidated.
    pub async fn truncate(&self, end: u64) -> io::Result<()> {
        let this = self.clone();
        tokio::task::spawn_local(async move {
            let result = this.truncate_inner(end).await;
            if result.as_ref().is_err_and(|e| {
                e.kind() != io::ErrorKind::InvalidInput && e.kind() != io::ErrorKind::BrokenPipe
            }) {
                this.remember(&result);
            }
            result
        })
        .await
        .map_err(io::Error::other)?
    }
    /// Drain the old file before truncation, then rebuild its retained partial-page prefix.
    /// Advance the generation so old sync waiters reject positions invalidated by truncation.
    async fn truncate_inner(&self, end: u64) -> io::Result<()> {
        let _lock = self.inner.append_lock.lock().await;
        self.check()?;
        if self.inner.state.borrow().closed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        if end > self.positions().appended {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        self.dispatch();
        let target = self.positions().appended;
        self.sync_through(target).await?;
        self.inner.state.borrow_mut().mutating = true;
        let _mutation = Mutation(self.inner.clone());
        self.inner.file.set_len(end).await?;
        self.inner.file.sync_data().await?;
        let a = self.inner.file.alignment();
        let n = (end % a as u64) as usize;
        let tail = if n == 0 {
            Vec::new()
        } else {
            let (result, b) = self
                .inner
                .file
                .read_at(Buffer::new(a, a)?, end - n as u64)
                .await;
            if result? < n {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            b[..n].to_vec()
        };
        let mut state = self.inner.state.borrow_mut();
        state.generation += 1;
        state.positions = Positions {
            appended: end,
            written: end,
            durable: end,
        };
        state.allocated = end;
        state.dispatched = end;
        state.tail = tail;
        state.overlap = None;
        if let Some(head) = state.head.take() {
            self.inner.pool.put(head.buffer);
        }
        Ok(())
    }
}
/// Releases truncate/finish exclusion on every exit path and wakes waiting syncs.
struct Mutation<F: FileIo>(Rc<Inner<F>>);
impl<F: FileIo> Drop for Mutation<F> {
    /// Release mutation exclusion and wake sync waiters on both success and failure.
    fn drop(&mut self) {
        self.0.state.borrow_mut().mutating = false;
        self.0.changed.notify_waiters();
    }
}
/// Expose the sticky failure before treating any position as successfully completed.
fn check(state: &State) -> io::Result<()> {
    match &state.failure {
        Some(e) => Err(e.error()),
        None => Ok(()),
    }
}
impl<F: FileIo> Reclaim for Inner<F> {
    fn reclaim(&self) {
        if let Some(inner) = self.weak.upgrade() {
            dispatch(&inner);
        }
    }
}
impl<F: FileIo> Drop for Inner<F> {
    /// Return the unsubmitted head to the pool; dispatched writes own their resources separately.
    fn drop(&mut self) {
        if let Some(head) = self.state.get_mut().head.take() {
            self.pool.put(head.buffer);
        }
        self.timer.notify_one();
    }
}

/// Write an aligned head while retaining its partial-page prefix for the next append.
/// Only overlapping page rewrites wait on predecessors; completed chunks retire in order.
fn dispatch<F: FileIo>(inner: &Rc<Inner<F>>) {
    let (mut head, start, end, predecessor, completion) = {
        let mut state = inner.state.borrow_mut();
        if state.head.as_ref().is_none_or(|h| !h.dirty) {
            return;
        }
        let head = state.head.take().unwrap();
        let end = state.positions.appended;
        let start = end - head.used as u64;
        let alignment = inner.file.alignment();
        let write_len = head.used.div_ceil(alignment) * alignment;
        let tail_start = head.used / alignment * alignment;
        state.tail = head.buffer[tail_start..head.used].to_vec();
        let predecessor = state
            .overlap
            .as_ref()
            .filter(|(previous_end, _)| start < *previous_end)
            .map(|(_, c)| c.clone());
        let completion = Rc::new(Completion::default());
        state.overlap = Some((start + write_len as u64, completion.clone()));
        state.dispatched = end;
        state.writes.push_back(Write {
            end,
            completion: completion.clone(),
        });
        (head, start, end, predecessor, completion)
    };
    head.buffer
        .resize(head.used.div_ceil(inner.file.alignment()) * inner.file.alignment());
    let file = inner.file.clone();
    let pool = inner.pool.clone();
    let weak = Rc::downgrade(inner);
    pool.0.writes.set(pool.0.writes.get() + 1);
    tokio::task::spawn_local(async move {
        let mut result = Ok(());
        if let Some(predecessor) = predecessor {
            result = predecessor.wait().await;
        }
        let Head {
            mut buffer, permit, ..
        } = head;
        let mut offset = start;
        while result.is_ok() && !buffer.is_empty() {
            let expected = buffer.len();
            let (written, returned) = file.write_at(buffer, offset).await;
            buffer = returned;
            match written {
                Ok(0) => result = Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(n) if n > expected || !n.is_multiple_of(file.alignment()) => {
                    result = Err(io::Error::other("invalid direct-I/O short write"))
                }
                Ok(n) => {
                    offset += n as u64;
                    buffer.copy_within(n..expected, 0);
                    buffer.resize(expected - n);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => result = Err(e),
            }
        }
        pool.put(buffer);
        drop(permit);
        completion.finish(result.map_err(Failure::from));
        if let Some(inner) = weak.upgrade() {
            let mut state = inner.state.borrow_mut();
            while state
                .writes
                .front()
                .is_some_and(|w| w.completion.result.borrow().is_some())
            {
                let write = state.writes.pop_front().unwrap();
                match write.completion.result.borrow().as_ref().unwrap() {
                    Ok(()) if state.failure.is_none() => state.positions.written = write.end,
                    Err(e) if state.failure.is_none() => state.failure = Some(e.clone()),
                    _ => {}
                }
            }
            drop(state);
            inner.changed.notify_waiters();
        }
        pool.0.writes.set(pool.0.writes.get() - 1);
        pool.0.changed.notify_waiters();
        let _ = end;
    });
}
/// Dispatch a dirty head after its idle interval without keeping the file alive.
/// Dropping the last handle returns buffered heads; only dispatched writes continue.
async fn idle_timer<F: FileIo>(weak: Weak<Inner<F>>, timer: Rc<Notify>) {
    loop {
        let deadline = {
            let Some(inner) = weak.upgrade() else { return };
            let state = inner.state.borrow();
            if state.closed {
                return;
            }
            state
                .head
                .as_ref()
                .filter(|h| h.dirty)
                .map(|_| state.last_append + inner.config.idle)
        };
        if let Some(deadline) = deadline {
            tokio::select! {
                _ = timer.notified() => {},
                _ = tokio::time::sleep_until(deadline) => {
                    let Some(inner) = weak.upgrade() else { return };
                    if Instant::now() >= inner.state.borrow().last_append + inner.config.idle { dispatch(&inner); }
                }
            }
        } else {
            timer.notified().await;
        }
    }
}
