use crate::Buffer;
use io_uring::{IoUring, opcode, types};
use slab::Slab;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::io::unix::AsyncFd;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot};

/// Per-driver capacity limits, applied on one local runtime.
#[derive(Clone, Debug)]
pub struct Config {
    /// Accepted data/sync requests, including queued requests, until completion.
    pub max_in_flight: usize,
    /// Concurrent blocking metadata jobs.
    pub helper_concurrency: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_in_flight: 256,
            helper_concurrency: 8,
        }
    }
}
/// Request and batching counters for this driver; blocking metadata jobs are separate.
#[derive(Clone, Copy, Debug, Default)]
pub struct IoStats {
    /// Cumulative data/sync requests accepted by this driver.
    pub submitted: u64,
    /// Cumulative requests retired, including failed operations.
    pub completed: u64,
    /// Nonempty batches dispatched by the driver loop.
    pub submission_batches: u64,
    /// Nonempty kernel completion batches.
    pub completion_batches: u64,
    /// Accepted requests still queued or awaiting completion.
    pub in_flight: usize,
}
/// Operations whose descriptor and optional buffer remain owned by one request.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Read,
    Write,
    SyncData,
    SyncAll,
}
/// Shared close gate for a file and its clones, tracking actual I/O completion.
#[derive(Default)]
pub(crate) struct Activity {
    /// Accepted operations whose resource guards have not yet retired.
    pending: AtomicUsize,
    /// Reject new activity after close begins.
    closed: AtomicBool,
    /// Wake close waiters when an operation guard retires.
    changed: Notify,
}
/// Keeps a file's accepted operation visible to close until its resources retire.
pub(crate) struct ActivityGuard(Arc<Activity>);
impl Activity {
    /// Track one file operation until its actual completion, rejecting a closed file.
    pub(crate) fn begin(self: &Arc<Self>) -> io::Result<ActivityGuard> {
        if self.closed.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "file closed"));
        }
        self.pending.fetch_add(1, Ordering::AcqRel);
        Ok(ActivityGuard(self.clone()))
    }
    /// Stop new file activity and wait for existing guards, including detached helpers.
    pub(crate) async fn close(&self) {
        self.closed.store(true, Ordering::Release);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.pending.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
}
impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.0.pending.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}
/// Owns everything an accepted operation needs, even after its waiter is dropped.
struct Request {
    /// Operation to execute on the retained descriptor.
    kind: Kind,
    /// Keep the descriptor open until actual completion.
    file: Arc<std::fs::File>,
    /// Owned data allocation for reads/writes; absent for sync operations.
    buffer: Option<Buffer>,
    /// Explicit byte offset for data operations; unused for syncs.
    offset: u64,
    /// Return completion and buffer; dropping the receiver does not cancel I/O.
    reply: oneshot::Sender<(io::Result<usize>, Option<Buffer>)>,
    /// Hold driver admission capacity until this request retires.
    _permit: OwnedSemaphorePermit,
    /// Keep this operation visible to file-close waiters.
    _activity: ActivityGuard,
}
/// Core-local admission and driver-loop state; helper completion signals cross threads.
struct Shared {
    /// Accepted requests waiting for the local driver to dispatch them.
    pending: RefCell<VecDeque<Request>>,
    /// Bound all accepted data/sync requests through completion.
    slots: Arc<Semaphore>,
    /// Bound blocking metadata jobs.
    helpers: Arc<Semaphore>,
    /// Started metadata helpers still running, including detached ones.
    helper_active: Arc<AtomicUsize>,
    /// Wake drain/shutdown waiters after requests or helpers complete.
    changed: Arc<Notify>,
    /// Wake the local driver for newly queued work or admission closure.
    wake: Notify,
    /// Stop new admission while previously accepted work drains.
    closed: Cell<bool>,
    /// Driver loop has exited after its accepted requests retired.
    done: Cell<bool>,
    /// Local request and batching counters.
    stats: Cell<IoStats>,
}

/// A cloneable local driver handle; independent of shep-smp. Last-handle drop
/// stops admission and lets accepted work drain. Prefer explicit shutdown.
pub struct Io {
    /// Core-local state also owned by the driver task.
    shared: Rc<Shared>,
    // Counts public handles separately from the driver task's ownership of Shared.
    life: Rc<()>,
}
impl Clone for Io {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            life: self.life.clone(),
        }
    }
}
impl Drop for Io {
    fn drop(&mut self) {
        if Rc::strong_count(&self.life) == 1 {
            self.shared.closed.set(true);
            self.shared.wake.notify_one();
        }
    }
}
impl Io {
    /// Start a bounded driver on the current Tokio local runtime.
    /// Return the initialization error if io_uring or its completion wakeup is unavailable.
    pub fn new(config: Config) -> io::Result<Self> {
        if config.max_in_flight == 0
            || config.max_in_flight > 32768
            || config.helper_concurrency == 0
            || config.helper_concurrency > Semaphore::MAX_PERMITS
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid I/O capacity",
            ));
        }
        let kernel = Kernel::new(config.max_in_flight)?;
        let shared = Rc::new(Shared {
            pending: RefCell::new(VecDeque::new()),
            slots: Arc::new(Semaphore::new(config.max_in_flight)),
            helpers: Arc::new(Semaphore::new(config.helper_concurrency)),
            helper_active: Arc::default(),
            changed: Arc::new(Notify::new()),
            wake: Notify::new(),
            closed: Cell::new(false),
            done: Cell::new(false),
            stats: Cell::new(IoStats::default()),
        });
        tokio::task::spawn_local(drive(shared.clone(), kernel));
        Ok(Self {
            shared,
            life: Rc::new(()),
        })
    }
    /// Snapshot accepted data/sync requests and kernel batching counters.
    pub fn stats(&self) -> IoStats {
        self.shared.stats.get()
    }
    /// Wait until accepted requests and active metadata helpers are idle, without syncing.
    /// Admission stays open; futures still waiting for capacity are not included.
    pub async fn drain(&self) -> io::Result<()> {
        loop {
            let changed = self.shared.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.shared.stats.get().in_flight == 0
                && self.shared.helper_active.load(Ordering::Acquire) == 0
            {
                return Ok(());
            }
            changed.await;
        }
    }
    /// Stop admission, finish accepted work, and wait for the driver task to exit.
    /// Consumers must establish their own durability before shutting down.
    pub async fn shutdown(&self) -> io::Result<()> {
        self.shared.closed.set(true);
        self.shared.slots.close();
        self.shared.wake.notify_one();
        self.drain().await?;
        self.shared.helpers.close();
        loop {
            let changed = self.shared.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.shared.done.get() {
                return Ok(());
            }
            changed.await;
        }
    }
    /// Run metadata work with a bounded helper permit held through actual completion.
    /// Once started, dropping the waiter neither cancels work nor releases its capacity.
    pub(crate) async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        if self.shared.closed.get() {
            return Err(stopped());
        }
        let permit = self
            .shared
            .helpers
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| stopped())?;
        if self.shared.closed.get() {
            return Err(stopped());
        }
        self.shared.helper_active.fetch_add(1, Ordering::AcqRel);
        let guard = HelperGuard {
            active: self.shared.helper_active.clone(),
            changed: self.shared.changed.clone(),
        };
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _guard = guard;
            f()
        })
        .await
        .map_err(io::Error::other)?
    }
    /// Reserve capacity and transfer a request into driver ownership.
    /// Cancellation can remove a capacity waiter; a queued request runs to completion.
    async fn perform(
        &self,
        kind: Kind,
        file: Arc<std::fs::File>,
        activity: Arc<Activity>,
        buffer: Option<Buffer>,
        offset: u64,
    ) -> (io::Result<usize>, Option<Buffer>) {
        if self.shared.closed.get() {
            return (Err(stopped()), buffer);
        }
        let permit = match self.shared.slots.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return (Err(stopped()), buffer),
        };
        if self.shared.closed.get() {
            return (Err(stopped()), buffer);
        }
        let activity = match activity.begin() {
            Ok(a) => a,
            Err(e) => return (Err(e), buffer),
        };
        let (reply, result) = oneshot::channel();
        self.shared.pending.borrow_mut().push_back(Request {
            kind,
            file,
            buffer,
            offset,
            reply,
            _permit: permit,
            _activity: activity,
        });
        let mut stats = self.shared.stats.get();
        stats.submitted += 1;
        stats.in_flight += 1;
        self.shared.stats.set(stats);
        self.shared.wake.notify_one();
        // Unexpected driver termination propagates as a panic. The accepted
        // allocation remains driver-owned until cleanup can safely release it.
        result
            .await
            .expect("I/O driver terminated without completing accepted work")
    }
    /// Submit owned-buffer I/O and preserve its buffer on validation or admission failure.
    pub(crate) async fn data(
        &self,
        kind: Kind,
        file: Arc<std::fs::File>,
        activity: Arc<Activity>,
        buffer: Buffer,
        offset: u64,
    ) -> (io::Result<usize>, Buffer) {
        let (result, buffer) = self
            .perform(kind, file, activity, Some(buffer), offset)
            .await;
        (result, buffer.expect("data operations retain their buffer"))
    }
    /// Submit an independent synchronization request; callers supply write dependencies.
    pub(crate) async fn sync(
        &self,
        file: Arc<std::fs::File>,
        activity: Arc<Activity>,
        data: bool,
    ) -> io::Result<()> {
        self.perform(
            if data { Kind::SyncData } else { Kind::SyncAll },
            file,
            activity,
            None,
            0,
        )
        .await
        .0
        .map(|_| ())
    }
}
fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "I/O driver shut down")
}
/// Tracks a started metadata helper through completion, independently of its waiter.
struct HelperGuard {
    /// Shared count decremented only when the helper actually exits.
    active: Arc<AtomicUsize>,
    /// Notify drain/shutdown waiters when this helper retires.
    changed: Arc<Notify>,
}
impl Drop for HelperGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
        self.changed.notify_waiters();
    }
}
/// Retire accepted work and deliver its buffer, or release it if its waiter is gone.
fn complete(shared: &Shared, request: Request, result: io::Result<usize>) {
    let mut stats = shared.stats.get();
    stats.completed += 1;
    stats.in_flight -= 1;
    shared.stats.set(stats);
    let _ = request.reply.send((result, request.buffer));
    shared.changed.notify_waiters();
    shared.wake.notify_one();
}
/// Batch ready requests and completions while keeping dispatch on this local runtime.
/// Requests retain their resources until real kernel completion.
async fn drive(shared: Rc<Shared>, mut kernel: Kernel) {
    loop {
        let pending = std::mem::take(&mut *shared.pending.borrow_mut());
        if !pending.is_empty() {
            let mut stats = shared.stats.get();
            stats.submission_batches += 1;
            shared.stats.set(stats);
        }
        for request in pending {
            kernel.push(request);
        }
        kernel.submit();
        let completions = kernel.reap();
        if !completions.is_empty() {
            let mut stats = shared.stats.get();
            stats.completion_batches += 1;
            shared.stats.set(stats);
            for (request, result) in completions {
                complete(&shared, request, result);
            }
        }
        if shared.closed.get() && shared.stats.get().in_flight == 0 {
            shared.done.set(true);
            shared.changed.notify_waiters();
            return;
        }
        tokio::select! {
            _ = shared.wake.notified() => {},
            ready = kernel.event.readable() => {
                if let Ok(mut ready) = ready {
                    let mut value = 0u64;
                    // Drain the nonblocking eventfd. CQEs, not this counter, are authoritative.
                    unsafe { libc::read(kernel.event.as_raw_fd(), (&mut value as *mut u64).cast(), 8); }
                    ready.clear_ready();
                }
            },
            // Retry transient submission failures without blocking the runtime.
            _ = tokio::time::sleep(std::time::Duration::from_millis(1)), if shared.stats.get().in_flight > 0 => {},
        }
        tokio::task::yield_now().await;
    }
}
/// Owns the ring and submitted requests; slab keys connect CQEs to retained resources.
/// Destruction drains the ring before releasing any kernel-visible allocation.
struct Kernel {
    /// Owned ring; taken during destruction after draining visible requests.
    ring: Option<IoUring>,
    /// Own request resources; slab keys identify their completion entries.
    requests: Slab<Request>,
    /// Registered eventfd bridging kernel completions into the local reactor.
    event: AsyncFd<OwnedFd>,
}
impl Kernel {
    /// Create the ring and its reactor-integrated eventfd completion wakeup.
    fn new(capacity: usize) -> io::Result<Self> {
        let ring = IoUring::new(capacity.next_power_of_two() as u32)?;
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        ring.submitter().register_eventfd(fd.as_raw_fd())?;
        Ok(Self {
            ring: Some(ring),
            requests: Slab::with_capacity(capacity),
            event: unsafe { AsyncFd::register(fd)? },
        })
    }
    /// Retain request resources in the slab before exposing their addresses to the kernel.
    fn push(&mut self, mut request: Request) {
        let key = self.requests.vacant_key();
        let fd = types::Fd(request.file.as_raw_fd());
        let entry = match request.kind {
            Kind::Read => {
                let b = request.buffer.as_mut().unwrap();
                opcode::Read::new(fd, b.as_mut_ptr(), b.len() as u32)
                    .offset(request.offset)
                    .build()
            }
            Kind::Write => {
                let b = request.buffer.as_ref().unwrap();
                opcode::Write::new(fd, b.as_ptr(), b.len() as u32)
                    .offset(request.offset)
                    .build()
            }
            Kind::SyncData => opcode::Fsync::new(fd)
                .flags(types::FsyncFlags::DATASYNC)
                .build(),
            Kind::SyncAll => opcode::Fsync::new(fd).build(),
        }
        .user_data(key as u64);
        self.requests.insert(request);
        // Every request holds one of at most SQ-capacity admission permits.
        unsafe { self.ring.as_mut().unwrap().submission().push(&entry) }
            .expect("admission bounds the submission queue");
    }
    /// Submit the current batch; transient queue pressure is retried by the driver loop.
    fn submit(&mut self) {
        if let Err(e) = self.ring.as_ref().unwrap().submit()
            && !matches!(
                e.raw_os_error(),
                Some(libc::EAGAIN | libc::EBUSY | libc::EINTR)
            )
        {
            panic!("io_uring submission failed: {e}");
        }
    }
    /// Translate completion entries and recover the resources associated with each request.
    fn reap(&mut self) -> Vec<(Request, io::Result<usize>)> {
        self.ring
            .as_mut()
            .unwrap()
            .completion()
            .map(|cqe| {
                let result = cqe.result();
                let result = if result < 0 {
                    Err(io::Error::from_raw_os_error(-result))
                } else {
                    Ok(result as usize)
                };
                (self.requests.remove(cqe.user_data() as usize), result)
            })
            .collect()
    }
}
impl Drop for Kernel {
    fn drop(&mut self) {
        // Last-resort memory safety when a runtime is dropped without shutdown.
        // Normal shutdown is asynchronous. Never free a kernel-owned buffer early.
        while !self.requests.is_empty() {
            match self.ring.as_ref().unwrap().submit_and_wait(1) {
                Ok(_) => {
                    drop(self.reap());
                }
                Err(e)
                    if matches!(
                        e.raw_os_error(),
                        Some(libc::EINTR | libc::EAGAIN | libc::EBUSY)
                    ) =>
                {
                    continue;
                }
                Err(e) => {
                    tracing::error!(error = %e, "cannot drain io_uring; retaining in-flight allocations");
                    let requests = std::mem::take(&mut self.requests);
                    std::mem::forget(requests);
                    break;
                }
            }
        }
        drop(self.ring.take());
    }
}
