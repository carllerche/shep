//! Weighted local admission above asynchronous files. A Device is shared across
//! cores that use the same physical device. Dispatch stays on each calling core.
//! The scheduler provides no file ordering; only dependency-ready work belongs here.
use shep_smp_io::{Buffer, FileIo, Io, OpenOptions};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore, oneshot};

/// Shared physical-device limits, enforced across all cores using the same Device.
#[derive(Clone, Debug)]
pub struct DeviceConfig {
    /// Maximum simultaneously admitted operations, including downstream admission waits.
    pub max_operations: usize,
    /// Maximum sum of byte charges held by admitted operations.
    pub max_bytes: usize,
    /// Optional bytes-per-second cap, with max_bytes initial burst capacity.
    pub bytes_per_second: Option<u64>,
    /// Optional operations-per-second cap, with max_operations initial burst capacity.
    pub operations_per_second: Option<u64>,
}
impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            max_operations: 128,
            max_bytes: 64 << 20,
            bytes_per_second: None,
            operations_per_second: None,
        }
    }
}
/// One class's FIFO in the device-wide weighted virtual-service ordering.
#[derive(Debug)]
struct Lane {
    /// Relative service weight assigned when this class registers.
    shares: u32,
    /// Virtual service consumed by this lane; smaller values run first.
    finish: f64,
    /// Device tickets in FIFO order for this lane.
    queue: VecDeque<u64>,
}
/// An ungranted device ticket's resource charge and owning lane.
#[derive(Debug)]
struct Demand {
    /// Index of the registered lane owning this ticket.
    lane: usize,
    /// Byte reservation required before this operation may execute.
    bytes: usize,
}
/// Mutex-protected capacity, rate tokens, and lane ordering shared by every core.
#[derive(Debug)]
struct Capacity {
    /// Currently admitted operations across all participating cores.
    operations: usize,
    /// Total byte reservations held by admitted operations.
    bytes: usize,
    /// Available byte-rate credit, capped at max_bytes.
    byte_tokens: f64,
    /// Available operation-rate credit, capped at max_operations.
    op_tokens: f64,
    /// Last token-refill timestamp.
    updated: Instant,
    /// Registered classes; empty slots can be reused after scheduler retirement.
    lanes: Vec<Option<Lane>>,
    /// Ungranted ticket demands, shared across all cores.
    pending: BTreeMap<u64, Demand>,
    /// Next globally unique ticket number.
    next: u64,
    /// Shared service baseline preventing idle lanes from accumulating credit.
    virtual_time: f64,
}
/// Cross-core device domain; wakeups prompt local schedulers to retry admission.
#[derive(Debug)]
struct DeviceInner {
    /// Immutable capacity and optional rate-limit policy.
    config: DeviceConfig,
    /// Shared admission state, locked only while updating device accounting.
    capacity: Mutex<Capacity>,
    /// Wake competing local schedulers after demand or capacity changes.
    changed: Notify,
}
/// An explicitly configured physical-device domain. Clones share capacity even
/// across different thread-local schedulers. No central dispatch thread is used.
#[derive(Clone, Debug)]
pub struct Device(Arc<DeviceInner>);
impl Device {
    /// Create a shared device budget with optional rate limits and bounded initial bursts.
    pub fn new(config: DeviceConfig) -> io::Result<Self> {
        if config.max_operations == 0
            || config.max_bytes == 0
            || config.bytes_per_second == Some(0)
            || config.operations_per_second == Some(0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid device budget",
            ));
        }
        let capacity = Capacity {
            operations: 0,
            bytes: 0,
            byte_tokens: config.max_bytes as f64,
            op_tokens: config.max_operations as f64,
            updated: Instant::now(),
            lanes: Vec::new(),
            pending: BTreeMap::new(),
            next: 0,
            virtual_time: 0.0,
        };
        Ok(Self(Arc::new(DeviceInner {
            config,
            capacity: Mutex::new(capacity),
            changed: Notify::new(),
        })))
    }
    /// Return admitted operation and byte counts across every scheduler sharing this device.
    pub fn in_flight(&self) -> (usize, usize) {
        let c = self.0.capacity.lock().unwrap();
        (c.operations, c.bytes)
    }
    /// Register one weighted class in the shared domain, reusing a retired lane slot.
    fn register_lane(&self, shares: u32) -> usize {
        let mut c = self.0.capacity.lock().unwrap();
        let lane = Lane {
            shares,
            finish: c.virtual_time,
            queue: VecDeque::new(),
        };
        if let Some(id) = c.lanes.iter().position(Option::is_none) {
            c.lanes[id] = Some(lane);
            id
        } else {
            let id = c.lanes.len();
            c.lanes.push(Some(lane));
            id
        }
    }
    /// Issue a global ticket in lane order without letting an idle lane accumulate credit.
    fn enqueue(&self, lane: usize, bytes: usize) -> u64 {
        let mut c = self.0.capacity.lock().unwrap();
        if c.lanes[lane].as_ref().unwrap().queue.is_empty() {
            let virtual_time = c.virtual_time;
            let lane = c.lanes[lane].as_mut().unwrap();
            lane.finish = lane.finish.max(virtual_time);
        }
        let ticket = c.next;
        c.next += 1;
        c.pending.insert(ticket, Demand { lane, bytes });
        c.lanes[lane].as_mut().unwrap().queue.push_back(ticket);
        drop(c);
        self.0.changed.notify_waiters();
        ticket
    }
    /// Remove ungranted demand and wake competing lanes when a waiter is cancelled.
    fn cancel(&self, ticket: u64) {
        let mut c = self.0.capacity.lock().unwrap();
        if let Some(demand) = c.pending.remove(&ticket) {
            c.lanes[demand.lane]
                .as_mut()
                .unwrap()
                .queue
                .retain(|t| *t != ticket);
        }
        drop(c);
        self.0.changed.notify_waiters();
    }
    /// Grant only the eligible lane head with the earliest weighted virtual finish.
    /// Reserve shared capacity and rate tokens together so cores cannot oversubscribe.
    fn take(&self, ticket: u64) -> Option<DevicePermit> {
        let mut c = self.0.capacity.lock().unwrap();
        let cfg = &self.0.config;
        let elapsed = c.updated.elapsed().as_secs_f64();
        c.updated = Instant::now();
        if let Some(rate) = cfg.bytes_per_second {
            c.byte_tokens = (c.byte_tokens + elapsed * rate as f64).min(cfg.max_bytes as f64);
        }
        if let Some(rate) = cfg.operations_per_second {
            c.op_tokens = (c.op_tokens + elapsed * rate as f64).min(cfg.max_operations as f64);
        }
        if c.operations == cfg.max_operations {
            return None;
        }
        let selected = c
            .lanes
            .iter()
            .flatten()
            .filter_map(|lane| {
                let ticket = *lane.queue.front()?;
                let demand = &c.pending[&ticket];
                (demand.bytes <= cfg.max_bytes - c.bytes
                    && (cfg.bytes_per_second.is_none() || c.byte_tokens >= demand.bytes as f64)
                    && (cfg.operations_per_second.is_none() || c.op_tokens >= 1.0))
                    .then_some((ticket, lane.finish))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)))
            .map(|(ticket, _)| ticket);
        if selected != Some(ticket) {
            return None;
        }
        let demand = c.pending.remove(&ticket).unwrap();
        let bytes = demand.bytes;
        let lane = c.lanes[demand.lane].as_mut().unwrap();
        let finish = lane.finish;
        assert_eq!(lane.queue.pop_front(), Some(ticket));
        lane.finish += bytes.max(4096) as f64 / lane.shares as f64;
        c.virtual_time = c.virtual_time.max(finish);
        c.operations += 1;
        c.bytes += bytes;
        if cfg.bytes_per_second.is_some() {
            c.byte_tokens -= bytes as f64;
        }
        if cfg.operations_per_second.is_some() {
            c.op_tokens -= 1.0;
        }
        drop(c);
        self.0.changed.notify_waiters();
        Some(DevicePermit {
            device: self.clone(),
            bytes,
        })
    }
}
impl Default for Device {
    fn default() -> Self {
        Self::new(DeviceConfig::default()).unwrap()
    }
}
/// Holds operation/byte capacity until actual execution finishes, then wakes contenders.
struct DevicePermit {
    /// Shared device whose reservation this guard releases.
    device: Device,
    /// Byte capacity to return alongside one operation slot on drop.
    bytes: usize,
}
impl Drop for DevicePermit {
    fn drop(&mut self) {
        let mut c = self.device.0.capacity.lock().unwrap();
        c.operations -= 1;
        c.bytes -= self.bytes;
        drop(c);
        self.device.0.changed.notify_waiters();
    }
}
/// Local class queue/execution gauges and cumulative service/timing counters.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClassStats {
    /// Callers registered in this class queue, excluding queue-slot waiters.
    pub queued: usize,
    /// Outstanding device reservations, including downstream admission waits.
    pub executing: usize,
    /// Retired reservations, including operations returning errors.
    pub completed: u64,
    /// Cumulative admitted byte charges retired by this class.
    pub bytes: u64,
    /// Total wait before grants, including local queue-slot admission.
    pub queue_time: Duration,
    /// Total reservation time, including downstream driver admission and completion delivery.
    pub execution_time: Duration,
}
/// Links a local caller to its shared-device ticket until admission or cancellation.
struct Waiter {
    /// Local identifier used to remove this waiter on cancellation.
    id: u64,
    /// Matching demand in the shared device ordering.
    device_ticket: u64,
    /// Requested byte charge, retained for execution accounting.
    bytes: usize,
    /// Deliver ownership of an execution reservation to this caller.
    ready: oneshot::Sender<Admission>,
    /// Admission start, including time waiting for a local queue slot.
    started: Instant,
}
/// Local FIFO and accounting for one class registered as a shared-device lane.
struct ClassState {
    /// Matching lane index in the shared device domain.
    lane: usize,
    /// Diagnostic class name exposed to callers.
    name: String,
    /// Local callers awaiting device admission, in FIFO order.
    queue: VecDeque<Waiter>,
    /// Queue gauges and cumulative accounting for this class.
    stats: ClassStats,
}
/// Core-local queues and shutdown state, tracking queued callers separately from admitted work.
struct State {
    /// Registered local classes; their vector indices are stable handle IDs.
    classes: Vec<ClassState>,
    /// Next local waiter ID, used by cancellation guards.
    next: u64,
    /// Reject new admissions once scheduler shutdown begins.
    closed: bool,
    /// Outstanding execution reservations across all local classes.
    active: usize,
}
/// Keeps the raw driver, device lanes, and bounded local queue alive for all handles.
struct Inner {
    /// Raw local completion driver used after scheduler admission.
    io: Io,
    /// Cross-core capacity and weighted-service domain.
    device: Device,
    /// Local queues and accounting; borrows never span asynchronous waits.
    state: RefCell<State>,
    /// Bound callers registered in local queues before device admission.
    queue_slots: Arc<Semaphore>,
    /// Wake local admissions and drain waiters when state changes.
    changed: Notify,
}
/// A core-local admission queue using one raw driver and a shared device budget.
#[derive(Clone)]
pub struct Scheduler {
    /// Shared local queues and driver ownership retained by scheduler handles.
    inner: Rc<Inner>,
}
/// A cloneable handle to one weighted lane; clones share ordering and accounting.
#[derive(Clone)]
pub struct Class {
    /// Keep the scheduler and device lane alive for this class.
    inner: Rc<Inner>,
    /// Stable index into the scheduler's local class table.
    id: usize,
}
/// A raw descriptor paired with the class charged for each operation.
#[derive(Clone)]
pub struct File {
    /// Descriptor retaining its driver and shared close gate.
    raw: shep_smp_io::File,
    /// Admission lane charged by operations through this handle.
    class: Class,
}

impl Scheduler {
    /// Attach a local driver to a shared device domain and bound queued admissions.
    pub fn new(io: Io, device: Device, max_queued: usize) -> io::Result<Self> {
        if max_queued == 0 || max_queued > Semaphore::MAX_PERMITS {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        Ok(Self {
            inner: Rc::new(Inner {
                io,
                device,
                queue_slots: Arc::new(Semaphore::new(max_queued)),
                state: RefCell::new(State {
                    classes: Vec::new(),
                    next: 0,
                    closed: false,
                    active: 0,
                }),
                changed: Notify::new(),
            }),
        })
    }
    /// Create a scheduling class whose positive shares compete across the whole device.
    /// Each call creates a separate lane, even when names match.
    pub fn class(&self, name: impl Into<String>, shares: u32) -> io::Result<Class> {
        if shares == 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let mut state = self.inner.state.borrow_mut();
        if state.closed {
            return Err(stopped());
        }
        let id = state.classes.len();
        state.classes.push(ClassState {
            lane: self.inner.device.register_lane(shares),
            name: name.into(),
            queue: VecDeque::new(),
            stats: ClassStats::default(),
        });
        Ok(Class {
            inner: self.inner.clone(),
            id,
        })
    }
    /// Expose the underlying driver; direct calls bypass class and device admission.
    pub fn io(&self) -> &Io {
        &self.inner.io
    }
    /// Wait for queued and executing work, then drain the driver, without synchronizing.
    /// New admission stays open; callers should quiesce producers for a stable drain.
    pub async fn drain(&self) -> io::Result<()> {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let idle = {
                let st = self.inner.state.borrow();
                st.active == 0 && st.classes.iter().all(|c| c.queue.is_empty())
            };
            if idle {
                return self.inner.io.drain().await;
            }
            changed.await;
        }
    }
    /// Stop admission, finish accepted work, and drain the underlying driver.
    pub async fn shutdown(&self) -> io::Result<()> {
        self.inner.state.borrow_mut().closed = true;
        self.inner.queue_slots.close();
        self.drain().await?;
        self.inner.io.shutdown().await
    }
}
impl Inner {
    /// Grant locally owned lane heads chosen by the shared device policy.
    /// Deliver grants after releasing local state because a cancelled receiver drops its grant.
    fn pump(self: &Rc<Self>) {
        let mut grants = Vec::new();
        let mut state = self.state.borrow_mut();
        loop {
            let mut granted = false;
            for id in 0..state.classes.len() {
                let class = &mut state.classes[id];
                if class.queue.is_empty() {
                    continue;
                }
                let bytes = class.queue.front().unwrap().bytes;
                if let Some(permit) = self.device.take(class.queue.front().unwrap().device_ticket) {
                    let waiter = class.queue.pop_front().unwrap();
                    class.stats.queued -= 1;
                    class.stats.executing += 1;
                    class.stats.queue_time += waiter.started.elapsed();
                    state.active += 1;
                    grants.push((
                        waiter.ready,
                        Admission {
                            inner: self.clone(),
                            class: id,
                            permit,
                            started: Instant::now(),
                            bytes,
                        },
                    ));
                    granted = true;
                    break;
                }
            }
            if !granted {
                break;
            }
        }
        drop(state);
        for (sender, admission) in grants {
            let _ = sender.send(admission);
        }
    }
}
impl Drop for Inner {
    /// Retire device lanes once no class, queued waiter, or active grant retains this scheduler.
    fn drop(&mut self) {
        let state = self.state.get_mut();
        let mut capacity = self.device.0.capacity.lock().unwrap();
        for class in &state.classes {
            capacity.lanes[class.lane] = None;
        }
    }
}
/// Cancellation guard that removes a caller's demand before it receives admission.
struct Queued {
    /// Scheduler whose queues hold the pending demand.
    inner: Rc<Inner>,
    /// Local class containing the waiter.
    class: usize,
    /// Local waiter ID to remove if this guard is dropped before admission.
    id: u64,
}
impl Drop for Queued {
    /// Cancel ungranted demand in both queues and give other local waiters a chance to run.
    fn drop(&mut self) {
        let mut state = self.inner.state.borrow_mut();
        let class = &mut state.classes[self.class];
        if let Some(i) = class.queue.iter().position(|w| w.id == self.id) {
            let waiter = class.queue.remove(i).unwrap();
            self.inner.device.cancel(waiter.device_ticket);
            class.stats.queued -= 1;
        }
        drop(state);
        self.inner.changed.notify_waiters();
        self.inner.pump();
    }
}
/// Owns an execution reservation and its accounting until the detached task completes.
struct Admission {
    /// Keep scheduler accounting alive through detached execution.
    inner: Rc<Inner>,
    /// Local class to credit when this reservation retires.
    class: usize,
    /// Device capacity retained until actual execution completes.
    permit: DevicePermit,
    /// Reservation start for cumulative execution-time accounting.
    started: Instant,
    /// Requested byte charge credited when the reservation retires.
    bytes: usize,
}
impl Admission {
    /// Start detached execution and hold the device reservation until the operation completes.
    /// Once execution starts, cancellation only drops the result waiter.
    async fn run<T: 'static>(self, operation: impl Future<Output = T> + 'static) -> T {
        // Hold admission until actual completion even if the caller stops waiting.
        tokio::task::spawn_local(async move {
            let result = operation.await;
            drop(self);
            result
        })
        .await
        .expect("scheduled I/O task panicked")
    }
}
impl Drop for Admission {
    /// Retire the reservation and its timing before releasing shared device capacity.
    fn drop(&mut self) {
        let _ = &self.permit;
        let mut state = self.inner.state.borrow_mut();
        state.active -= 1;
        let class = &mut state.classes[self.class];
        class.stats.executing -= 1;
        class.stats.completed += 1;
        class.stats.bytes += self.bytes as u64;
        class.stats.execution_time += self.started.elapsed();
        drop(state);
        self.inner.changed.notify_waiters();
        // DevicePermit's drop wakes admissions on every participating core.
    }
}
impl Class {
    /// Return the diagnostic name assigned when this class was registered.
    pub fn name(&self) -> String {
        self.inner.state.borrow().classes[self.id].name.clone()
    }
    /// Snapshot this class's queue, reservations, and accumulated timing counters.
    pub fn stats(&self) -> ClassStats {
        self.inner.state.borrow().classes[self.id].stats
    }
    /// Queue cancellable demand until both local queue space and device budget are available.
    /// A returned reservation must enter `run` before execution becomes detached.
    async fn admit(&self, bytes: usize) -> io::Result<Admission> {
        if bytes > self.inner.device.0.config.max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "request exceeds device byte budget",
            ));
        }
        if self.inner.state.borrow().closed {
            return Err(stopped());
        }
        let started = Instant::now();
        let slot = self
            .inner
            .queue_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| stopped())?;
        let (sender, mut receiver) = oneshot::channel();
        let id = {
            let mut state = self.inner.state.borrow_mut();
            if state.closed {
                return Err(stopped());
            }
            let id = state.next;
            state.next += 1;
            let class = &mut state.classes[self.id];
            let device_ticket = self.inner.device.enqueue(class.lane, bytes);
            class.queue.push_back(Waiter {
                id,
                device_ticket,
                bytes,
                ready: sender,
                started,
            });
            class.stats.queued += 1;
            id
        };
        let queued = Queued {
            inner: self.inner.clone(),
            class: self.id,
            id,
        };
        loop {
            let changed = self.inner.device.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            self.inner.pump();
            tokio::select! {
                permit = &mut receiver => {
                    let admission = permit.map_err(|_| stopped())?;
                    drop(queued); drop(slot);
                    return Ok(admission);
                },
                _ = changed => {},
                // Token replenishment has no completion event to wake us.
                _ = tokio::time::sleep(Duration::from_millis(1)), if self.inner.device.0.config.bytes_per_second.is_some() || self.inner.device.0.config.operations_per_second.is_some() => {},
            }
        }
    }
    /// Schedule metadata with one operation charge and no transfer-byte charge.
    async fn execute<T: 'static>(
        &self,
        operation: impl Future<Output = io::Result<T>> + 'static,
    ) -> io::Result<T> {
        self.admit(0).await?.run(operation).await
    }
    /// Schedule a metadata open and bind subsequent operations to this class.
    pub async fn open(&self, path: impl Into<PathBuf>, options: OpenOptions) -> io::Result<File> {
        let io = self.inner.io.clone();
        let path = path.into();
        let raw = self
            .execute(async move { io.open(path, options).await })
            .await?;
        Ok(File {
            raw,
            class: self.clone(),
        })
    }
    /// Create missing directories under this class's operation budget.
    pub async fn create_dir_all(&self, path: PathBuf) -> io::Result<()> {
        let io = self.inner.io.clone();
        self.execute(async move { io.create_dir_all(path).await })
            .await
    }
    /// Collect directory entries under this class's operation budget.
    pub async fn read_dir(&self, path: PathBuf) -> io::Result<Vec<PathBuf>> {
        let io = self.inner.io.clone();
        self.execute(async move { io.read_dir(path).await }).await
    }
    /// Schedule an unlink; directory durability remains an explicit dependency.
    pub async fn remove_file(&self, path: PathBuf) -> io::Result<()> {
        let io = self.inner.io.clone();
        self.execute(async move { io.remove_file(path).await })
            .await
    }
    /// Schedule a rename; directory durability remains an explicit dependency.
    pub async fn rename(&self, from: PathBuf, to: PathBuf) -> io::Result<()> {
        let io = self.inner.io.clone();
        self.execute(async move { io.rename(from, to).await }).await
    }
    /// Persist directory entry changes under this class's operation budget.
    pub async fn sync_dir(&self, path: PathBuf) -> io::Result<()> {
        let io = self.inner.io.clone();
        self.execute(async move { io.sync_dir(path).await }).await
    }
}
fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "I/O scheduler shut down")
}
impl File {
    /// Return the descriptor's recorded open path.
    pub fn path(&self) -> &Path {
        self.raw.path()
    }
    /// Reuse the same descriptor with another scheduling policy, such as recovery reads.
    /// The descriptor keeps its original raw driver; this does not reopen or migrate it.
    pub fn with_class(&self, class: Class) -> Self {
        Self {
            raw: self.raw.clone(),
            class,
        }
    }
}
impl FileIo for File {
    fn alignment(&self) -> usize {
        self.raw.alignment()
    }
    async fn read_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        let permit = match self.class.admit(buffer.len()).await {
            Ok(p) => p,
            Err(e) => return (Err(e), buffer),
        };
        let file = self.raw.clone();
        permit
            .run(async move { file.read_at(buffer, offset).await })
            .await
    }
    async fn write_at(&self, buffer: Buffer, offset: u64) -> (io::Result<usize>, Buffer) {
        let permit = match self.class.admit(buffer.len()).await {
            Ok(p) => p,
            Err(e) => return (Err(e), buffer),
        };
        let file = self.raw.clone();
        permit
            .run(async move { file.write_at(buffer, offset).await })
            .await
    }
    async fn sync_data(&self) -> io::Result<()> {
        let file = self.raw.clone();
        self.class
            .execute(async move { file.sync_data().await })
            .await
    }
    async fn sync_all(&self) -> io::Result<()> {
        let file = self.raw.clone();
        self.class
            .execute(async move { file.sync_all().await })
            .await
    }
    async fn allocate(&self, offset: u64, len: u64) -> io::Result<()> {
        let file = self.raw.clone();
        self.class
            .execute(async move { file.allocate(offset, len).await })
            .await
    }
    async fn set_len(&self, len: u64) -> io::Result<()> {
        let file = self.raw.clone();
        self.class
            .execute(async move { file.set_len(len).await })
            .await
    }
    async fn len(&self) -> io::Result<u64> {
        let file = self.raw.clone();
        self.class.execute(async move { file.len().await }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scheduler(device: Device) -> Scheduler {
        Scheduler::new(Io::new(Default::default()).unwrap(), device, 128).unwrap()
    }
    fn device() -> Device {
        Device::new(DeviceConfig {
            max_operations: 1,
            max_bytes: 4096,
            ..Default::default()
        })
        .unwrap()
    }
    async fn settle() {
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(flavor = "local")]
    async fn weighted_classes_make_progress_under_saturation() {
        let scheduler = scheduler(device());
        let blocker = scheduler.class("blocker", 1).unwrap();
        let high = scheduler.class("foreground", 3).unwrap();
        let low = scheduler.class("background", 1).unwrap();
        let held = blocker.admit(1).await.unwrap();
        let order = Rc::new(RefCell::new(Vec::new()));
        let mut tasks = Vec::new();
        for _ in 0..24 {
            for (id, class) in [(0, high.clone()), (1, low.clone())] {
                let order = order.clone();
                tasks.push(tokio::task::spawn_local(async move {
                    let permit = class.admit(1).await.unwrap();
                    order.borrow_mut().push(id);
                    drop(permit);
                }));
            }
        }
        settle().await;
        assert_eq!(high.stats().queued, 24);
        assert_eq!(low.stats().queued, 24);
        drop(held);
        for task in tasks {
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap();
        }
        let foreground = order.borrow()[..16].iter().filter(|id| **id == 0).count();
        assert!((11..=13).contains(&foreground), "{order:?}");
        assert_eq!(scheduler.inner.device.in_flight(), (0, 0));
        scheduler.shutdown().await.unwrap();
    }
    #[tokio::test(flavor = "local")]
    async fn canceling_a_waiter_releases_its_queue_position() {
        let scheduler = scheduler(device());
        let class = scheduler.class("reads", 1).unwrap();
        let held = class.admit(4096).await.unwrap();
        let pending = {
            let class = class.clone();
            tokio::task::spawn_local(async move { class.admit(4096).await.unwrap() })
        };
        settle().await;
        assert_eq!(class.stats().queued, 1);
        pending.abort();
        let _ = pending.await;
        assert_eq!(class.stats().queued, 0);
        drop(held);
        drop(class.admit(4096).await.unwrap());
        scheduler.shutdown().await.unwrap();
    }
    #[tokio::test(flavor = "local")]
    async fn dropping_an_execution_waiter_keeps_capacity_until_completion() {
        let device = device();
        let scheduler = scheduler(device.clone());
        let class = scheduler.class("writes", 1).unwrap();
        let held = class.admit(4096).await.unwrap();
        let release = Rc::new(Notify::new());
        let signal = release.clone();
        let task = tokio::task::spawn_local(async move {
            held.run(async move { signal.notified().await }).await
        });
        settle().await;
        task.abort();
        let _ = task.await;
        assert_eq!(device.in_flight(), (1, 4096));
        release.notify_one();
        scheduler.drain().await.unwrap();
        assert_eq!(device.in_flight(), (0, 0));
        scheduler.shutdown().await.unwrap();
    }
    #[test]
    fn device_capacity_is_shared_by_independent_core_runtimes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let device = device();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let active = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let (device, barrier, active) = (device.clone(), barrier.clone(), active.clone());
            threads.push(std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build_local(tokio::runtime::LocalOptions::default())
                    .unwrap();
                runtime.block_on(async move {
                    let scheduler = scheduler(device);
                    let class = scheduler.class("wal", 1).unwrap();
                    barrier.wait();
                    for _ in 0..16 {
                        let permit = class.admit(4096).await.unwrap();
                        assert_eq!(active.fetch_add(1, Ordering::SeqCst), 0);
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        assert_eq!(active.fetch_sub(1, Ordering::SeqCst), 1);
                        drop(permit);
                    }
                    scheduler.shutdown().await.unwrap();
                });
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(device.in_flight(), (0, 0));
    }
}
