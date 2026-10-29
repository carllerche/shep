//! The unsafe SQ publication, ring-driving, and descriptor-adoption boundary.
#![allow(unsafe_code)]
use crate::io::{
    Fd, Io, Operation, OperationHandle, Rejected, Value,
    arena::{Arena, OperationId, Record},
};
use io_uring::{IoUring, opcode, types};
use std::{
    cell::RefCell,
    io,
    net::Shutdown,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd},
    rc::Rc,
    task::Waker,
    time::Instant,
};

// Zero cannot be an OperationId. Used only for one teardown cancellation at a time.
const SHUTDOWN_CANCEL: u64 = 0;

const MAX_DRIVER_ID: u32 = (1 << 24) - 1;
static NEXT_DRIVER_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// Process-unique owner identity; never reused after its driver is destroyed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DriverId(
    /// Nonzero owner number fitting in 24 bits.
    u32,
);
impl DriverId {
    /// Allocate an owner ID, or fail permanently once the 24-bit namespace is full.
    pub(crate) fn allocate() -> io::Result<Self> {
        Self::allocate_from(&NEXT_DRIVER_ID)
    }
    /// Reserve the next ID with relaxed CAS; exhaustion leaves the counter unchanged.
    fn allocate_from(counter: &std::sync::atomic::AtomicU32) -> io::Result<Self> {
        use std::sync::atomic::Ordering::Relaxed;
        let mut next = counter.load(Relaxed);
        loop {
            if next > MAX_DRIVER_ID {
                return Err(io::Error::other(
                    "process io_uring identity limit exhausted",
                ));
            }
            match counter.compare_exchange_weak(next, next + 1, Relaxed, Relaxed) {
                Ok(_) => return Ok(Self(next)),
                Err(actual) => next = actual,
            }
        }
    }
    pub(crate) fn raw(self) -> u32 {
        self.0
    }
}

/// Effects of one terminal CQE, delivered only after releasing driver state.
#[derive(Default)]
struct CompletionEffects {
    wake: Option<Waker>,
    guard: Option<Box<dyn std::any::Any>>,
    discarded: [Option<Record>; 2],
}
impl CompletionEffects {
    fn deliver(self) {
        let Self {
            wake,
            guard,
            discarded,
        } = self;
        drop(discarded);
        if let Some(guard) = guard {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(guard)));
        }
        if let Some(waker) = wake {
            wake_safely(waker);
        }
    }
}
fn wake_safely(waker: Waker) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| waker.wake()));
}

/// Teardown alone defers notifications until all kernel access has ended.
#[derive(Default)]
struct ShutdownNotifications {
    wakes: Vec<Waker>,
    guards: Vec<Box<dyn std::any::Any>>,
}
impl ShutdownNotifications {
    fn defer(&mut self, effects: CompletionEffects) {
        let CompletionEffects {
            wake,
            guard,
            discarded,
        } = effects;
        drop(discarded);
        self.wakes.extend(wake);
        self.guards.extend(guard);
    }
    fn deliver(self) {
        for guard in self.guards {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(guard)));
        }
        for wake in self.wakes {
            wake_safely(wake);
        }
    }
}

/// Local state retained by Io clones and active cancellation pins.
pub(crate) struct Shared {
    pub(crate) worker: crate::io::support::Worker,
    pub(crate) id: DriverId,
    pub(crate) state: RefCell<IoState>,
    #[cfg(test)]
    submit_faults: RefCell<std::collections::VecDeque<SubmitFault>>,
}
/// No state borrow may span application callbacks or resource destruction.
pub(crate) struct IoState {
    pub(crate) arena: Arena,
    /// None after Driver destruction; completed records can still be consumed.
    ring: Option<IoUring>,
    shutdown_cancel_pending: bool,
}

// Test-only enter shim: consume a real prefix, then optionally report an error.
// This exercises ownership against actual kernel head movement and CQEs.
#[cfg(test)]
struct SubmitFault {
    count: u32,
    error: Option<i32>,
}
#[cfg(test)]
impl SubmitFault {
    fn submit(self, ring: &mut IoUring) -> io::Result<usize> {
        let count = self.count.min(ring.submission().len() as u32);
        let submitted = if count == 0 {
            0
        } else {
            // SAFETY: Only already-published SQEs are submitted; no wait, flags,
            // or argument pointers. Their inputs remain owned by the arena.
            unsafe {
                ring.submitter()
                    .enter::<libc::sigset_t>(count, 0, 0, None)?
            }
        };
        match self.error {
            Some(code) => Err(io::Error::from_raw_os_error(code)),
            None => Ok(submitted),
        }
    }
}

impl Shared {
    /// Make room before publishing, as in Monoio's submit_with_data path.
    /// Unlike Monoio's retry loop, progress attempts here are bounded.
    pub(crate) fn push(&self, mut operation: Operation) -> Result<OperationHandle, Rejected> {
        for attempt in 0..=2 {
            match self.try_publish(operation) {
                Ok(handle) => return Ok(handle),
                Err(rejected)
                    if rejected.error.kind() == io::ErrorKind::WouldBlock && attempt < 2 =>
                {
                    operation = rejected.operation;
                    if let Err(error) = self.make_space() {
                        return Err(Rejected { operation, error });
                    }
                    // Callbacks may have filled the SQ, closed admission, or
                    // retired a cancellation target. Reacquire and recheck.
                }
                Err(rejected) => return Err(rejected),
            }
        }
        unreachable!()
    }

    /// No callbacks, submission, or reaping after successful publication. This
    /// lets adapters attach completion guards immediately after push returns.
    fn try_publish(&self, operation: Operation) -> Result<OperationHandle, Rejected> {
        let mut state = match self.state.try_borrow_mut() {
            Ok(state) => state,
            Err(_) => {
                return Err(Rejected {
                    operation,
                    error: io::ErrorKind::WouldBlock.into(),
                });
            }
        };
        let IoState { ring, arena, .. } = &mut *state;
        let Some(ring) = ring.as_mut() else {
            return Err(Rejected {
                operation,
                error: io::ErrorKind::BrokenPipe.into(),
            });
        };
        if let Err(error) =
            validate(&operation, self, arena).and_then(|()| arena.check_admission(&operation))
        {
            return Err(Rejected { operation, error });
        }
        if ring.submission().is_full() {
            return Err(Rejected {
                operation,
                error: io::ErrorKind::WouldBlock.into(),
            });
        }
        let id = arena.insert(operation)?;
        let operation = arena.record(id).operation.as_mut().unwrap();
        let entry = entry(operation).user_data(id.raw());
        // SAFETY: Stable owned inputs reside in the arena until terminal CQE.
        // The SQ has space and no callback can consume it between check and push.
        // Keep rollback defensive: a failed push has published nothing.
        if unsafe { ring.submission().push(&entry) }.is_err() {
            return Err(arena.reject(id, io::ErrorKind::WouldBlock.into()));
        }
        if let Operation::Close(close) = operation {
            // Transfer exactly once, only after publication. A CLOSE error may
            // follow removal/reuse of this descriptor number.
            let _ = close.file.take().unwrap().into_raw_fd();
        }
        Ok(OperationHandle::new(id))
    }

    /// Make progress before publication, with no callbacks under state borrows.
    fn make_space(&self) -> io::Result<()> {
        let result = {
            let mut state = self
                .state
                .try_borrow_mut()
                .map_err(|_| io::ErrorKind::WouldBlock)?;
            let ring = state.ring.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
            let result = self.submit_ring(ring);
            if result.as_ref().is_err_and(|error| !transient(error)) {
                state.arena.fail();
            }
            result
        };
        self.tick();
        match result {
            Err(error) if !transient(&error) => {
                self.wake_failed();
                Err(error)
            }
            _ => Ok(()),
        }
    }

    fn wake_failed(&self) {
        let mut wakes = Vec::new();
        self.state.borrow_mut().arena.wakers_into(&mut wakes);
        for wake in wakes {
            wake_safely(wake);
        }
    }

    /// Process at most the CQ length observed on entry. Callbacks may reenter,
    /// consume remaining CQEs, publish more work, or destroy the owning driver.
    fn tick(&self) -> usize {
        self.tick_with(CompletionEffects::deliver)
    }

    fn tick_with(&self, mut deliver: impl FnMut(CompletionEffects)) -> usize {
        let budget = self
            .state
            .borrow_mut()
            .ring
            .as_mut()
            .map_or(0, |ring| ring.completion().len());
        let mut retired = 0;
        for _ in 0..budget {
            let effects = {
                let mut state = self.state.borrow_mut();
                let Some(effects) = state.retire_one(self.id) else {
                    break;
                };
                effects
            };
            retired += 1;
            deliver(effects);
        }
        retired
    }

    fn submit_ring(&self, ring: &mut IoUring) -> io::Result<usize> {
        #[cfg(test)]
        if let Some(fault) = self.submit_faults.borrow_mut().pop_front() {
            return fault.submit(ring);
        }
        ring.submit()
    }
    /// Whether this live ring has published SQEs awaiting submission.
    pub(crate) fn has_pending(&self) -> bool {
        self.state
            .borrow_mut()
            .ring
            .as_mut()
            .is_some_and(|ring| !ring.submission().is_empty())
    }
}

impl IoState {
    #[inline]
    fn retire_one(&mut self, owner: DriverId) -> Option<CompletionEffects> {
        // Drop the CQ view to publish its consumed head before any callback.
        let cqe = self.ring.as_mut()?.completion().next()?;
        if cqe.user_data() == SHUTDOWN_CANCEL {
            assert!(
                self.shutdown_cancel_pending,
                "unexpected shutdown cancellation"
            );
            assert_eq!(cqe.flags(), 0, "nonterminal shutdown cancellation");
            self.shutdown_cancel_pending = false;
            let result = cqe.result();
            assert!(
                result == 0 || result == -libc::ENOENT || result == -libc::EALREADY,
                "shutdown cancellation failed: {result}"
            );
            return Some(CompletionEffects::default());
        }
        let id = OperationId::from_raw(cqe.user_data());
        id.check(owner);
        // No exposed opcode produces MORE/NOTIF CQEs. Extensions must
        // implement their full kernel-retirement protocol first.
        assert!(
            !io_uring::cqueue::more(cqe.flags()),
            "unsupported nonterminal CQE"
        );
        let record = self.arena.record(id);
        let operation = record.operation.as_mut().expect("kernel-owned operation");
        let dependency = match operation {
            Operation::Cancel(target) => target.disarm(),
            _ => None,
        };
        let result = value(operation, cqe.result());
        let guard = record.completion_guard.take();
        let (wake, released) = self.arena.complete(id, result);
        let target = dependency.and_then(|target| self.arena.unpin(target));
        Some(CompletionEffects {
            wake,
            guard,
            discarded: [released, target],
        })
    }
}

/// Single-owner ring driver with bounded SQ storage and growing operation tracking.
/// Publication submits/reaps under SQ pressure; explicit batch driving is also available.
/// Destruction closes admission, cancels outstanding requests, and blocks until
/// all terminal completions are reaped. Completed results remain available via
/// retained Io clones. A fatal teardown error aborts the process rather than
/// releasing resources that the kernel may still access.
pub struct Driver {
    /// Driver-owned reference to the ring/slab; clones can outlive the driver
    /// to consume completed records, but cannot submit once the ring is closed.
    io: Io,
}
impl Driver {
    /// Create a ring with the requested SQ capacity rounded up to a power of two.
    /// This bounds queued SQEs, not outstanding requests or retained completions.
    /// Fail on invalid capacity (outside 1..=32768), missing lossless CQ overflow or extended waits,
    /// ring creation errors, or exhausted process DriverIds.
    /// Does not probe support for every opcode or flag; unsupported operations
    /// report kernel errors through their completions.
    pub fn new(submission_queue_capacity: usize) -> io::Result<Self> {
        if !(1..=32768).contains(&submission_queue_capacity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid submission queue capacity",
            ));
        }
        let ring = IoUring::new(submission_queue_capacity.next_power_of_two() as u32)?;
        if !ring.params().is_feature_ext_arg() || !ring.params().is_feature_nodrop() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "extended waits and lossless CQ overflow required",
            ));
        }
        let id = DriverId::allocate()?;
        Ok(Self {
            io: Io {
                shared: Rc::new(Shared {
                    worker: crate::io::support::Worker::new(),
                    id,
                    state: RefCell::new(IoState {
                        arena: Arena::new(submission_queue_capacity, id),
                        ring: Some(ring),
                        shutdown_cancel_pending: false,
                    }),
                    #[cfg(test)]
                    submit_faults: RefCell::default(),
                }),
            },
        })
    }
    pub fn io(&self) -> &Io {
        &self.io
    }
    /// Publish through [`Io::push`], with the same admission and lifecycle rules.
    pub fn push(&self, operation: Operation) -> Result<OperationHandle, Rejected> {
        self.io.push(operation)
    }
    /// Count requests awaiting their original CQEs; see [`Io::in_flight`].
    pub fn in_flight(&self) -> usize {
        self.io.in_flight()
    }
    /// Whether SQEs await submission; see [`Io::has_pending`].
    pub fn has_pending(&self) -> bool {
        self.io.has_pending()
    }
    /// Submit the currently published batch; Submitted/Waiting remain unchanged
    /// until their CQEs are reaped. Empty SQ returns zero. Transient errors retain
    /// all published requests for retry; a partial success leaves the tail queued.
    /// Fatal errors mark failure and extract/wake waiters without freeing inputs.
    pub fn submit(&mut self) -> io::Result<usize> {
        let result = {
            let mut state = self.io.shared.state.borrow_mut();
            let ring = state.ring.as_mut().expect("live driver");
            if ring.submission().is_empty() {
                return Ok(0);
            }
            self.io.shared.submit_ring(ring)
        };
        self.check(result)
    }
    /// On a fatal enter error, close all admission and turn Waiting -> Submitted
    /// while extracting/waking those waiters. Leave their inputs kernel-owned;
    /// transient errors and ETIME leave both admission and record states unchanged.
    fn check<T>(&self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result
            && !transient(error)
            && error.raw_os_error() != Some(libc::ETIME)
        {
            self.io.shared.state.borrow_mut().arena.fail();
            self.io.shared.wake_failed();
        }
        result
    }
    /// Retire available CQEs one at a time and deliver each completion outside
    /// driver borrows. Does not explicitly submit or wait; resource destructors
    /// can publish detached closes and drive SQ pressure. Work is bounded by the
    /// CQ length observed on entry, even when callbacks publish more operations.
    /// Returns terminal CQEs retired here, excluding any retired by reentrant calls.
    pub fn tick(&mut self) -> usize {
        self.io.shared.tick()
    }
    /// Submit a batch and wait for a CQE/deadline; the runtime decides when to sleep.
    /// Does not reap or advance record states; call tick to observe completion.
    /// An expired deadline skips submission. Transient errors return Interrupted;
    /// fatal errors close admission and wake Waiting records as in submit.
    pub fn submit_and_wait(&mut self, deadline: Option<Instant>) -> io::Result<WaitOutcome> {
        let result = {
            let mut state = self.io.shared.state.borrow_mut();
            let ring = state.ring.as_mut().expect("live driver");
            match deadline {
                Some(deadline) => {
                    let duration = deadline.saturating_duration_since(Instant::now());
                    if duration.is_zero() {
                        return Ok(WaitOutcome::TimedOut);
                    }
                    let ts = types::Timespec::from(duration);
                    ring.submitter()
                        .submit_with_args(1, &types::SubmitArgs::new().timespec(&ts))
                }
                None => ring.submit_and_wait(1),
            }
        };
        match self.check(result) {
            Ok(_) => Ok(WaitOutcome::Ready),
            Err(e) if e.raw_os_error() == Some(libc::ETIME) => Ok(WaitOutcome::TimedOut),
            Err(e) if transient(&e) => Ok(WaitOutcome::Interrupted),
            Err(e) => Err(e),
        }
    }
    /// Stop admission, join accepted metadata work, and drive kernel work to terminal CQEs. Pending watches
    /// and receives must be explicitly canceled by their higher-level owner.
    /// Repeated reap turns pending records into Completed or removes Ignored
    /// records after their refs retire. Completed submitter claims remain for
    /// later consumption. Return enter errors; this can wait indefinitely for I/O.
    pub fn drain(&mut self) -> io::Result<()> {
        self.io.close();
        self.io.shared.worker.shutdown();
        while self.in_flight() != 0 {
            match self.submit() {
                Err(e) if transient(&e) => {}
                result => {
                    result?;
                }
            }
            self.tick();
            if self.in_flight() != 0 {
                self.submit_and_wait(None)?;
            }
        }
        Ok(())
    }
    /// Drive teardown without the normal fatal-error callback path. All waiter
    /// notifications remain deferred until every original request has retired.
    fn shutdown_enter(
        &mut self,
        notifications: &mut ShutdownNotifications,
        wait: bool,
    ) -> io::Result<()> {
        let result = {
            let mut state = self.io.shared.state.borrow_mut();
            let ring = state.ring.as_mut().expect("live driver");
            if wait {
                ring.submit_and_wait(1)
            } else {
                ring.submit()
            }
        };
        self.io
            .shared
            .tick_with(|effects| notifications.defer(effects));
        match result {
            Ok(_) => Ok(()),
            Err(error) if transient(&error) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn cancel_and_drain(&mut self, notifications: &mut ShutdownNotifications) -> io::Result<()> {
        // Also reject public cancellation submissions from reentrant destructors.
        self.io.shared.state.borrow_mut().arena.fail();
        self.io
            .shared
            .tick_with(|effects| notifications.defer(effects));
        // Ensure every accepted SQE precedes its cancellation in submission order.
        while self.has_pending() {
            self.shutdown_enter(notifications, false)?;
        }
        let mut cursor = 0;
        loop {
            let target = self
                .io
                .shared
                .state
                .borrow()
                .arena
                .next_cancel_target(&mut cursor);
            let Some(target) = target else { break };
            let entry = opcode::AsyncCancel::new(target.raw())
                .build()
                .user_data(SHUTDOWN_CANCEL);
            {
                let mut state = self.io.shared.state.borrow_mut();
                let ring = state.ring.as_mut().expect("live driver");
                // SAFETY: This SQE contains only the target's identity, no user
                // pointers. Admission is failed, so slots cannot be reused. All
                // prior SQEs were submitted, and each internal cancel is reaped
                // before publishing the next, leaving space regardless of the
                // number of public operations still outstanding.
                unsafe { ring.submission().push(&entry) }.expect("shutdown SQ space");
            }
            self.io.shared.state.borrow_mut().shutdown_cancel_pending = true;
            while self.io.shared.state.borrow().shutdown_cancel_pending {
                self.shutdown_enter(notifications, true)?;
            }
        }
        // Cancel success is not proof of original-request retirement. In
        // particular, EALREADY means we must wait for natural completion.
        while self.in_flight() != 0 {
            self.shutdown_enter(notifications, true)?;
        }
        Ok(())
    }
}
impl Drop for Driver {
    /// Cancel and reap all accepted work before releasing the ring. This can
    /// block indefinitely for an operation that cannot finish or be canceled.
    /// Fatal errors or panics before retirement abort instead of freeing live
    /// kernel inputs. Notifications run after retirement and ring destruction.
    fn drop(&mut self) {
        let mut guard = RetirementGuard { retired: false };
        self.io.close();
        self.io.shared.worker.shutdown();
        let mut notifications = ShutdownNotifications::default();
        if let Err(error) = self.cancel_and_drain(&mut notifications) {
            eprintln!("io_uring teardown failed before request retirement: {error}");
            std::process::abort();
        }
        guard.retired = true;
        let ring = self.io.shared.state.borrow_mut().ring.take();
        drop(ring);
        notifications.deliver();
    }
}

/// Prevent unwinding from destroying kernel-owned inputs during failed teardown.
struct RetirementGuard {
    retired: bool,
}
impl Drop for RetirementGuard {
    fn drop(&mut self) {
        if !self.retired {
            std::process::abort();
        }
    }
}
/// Outcome of one kernel wait attempt; completion records have not yet been reaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Kernel wait returned; reap CQEs to observe their results.
    Ready,
    /// Deadline elapsed; an already-expired deadline skips submission as well.
    TimedOut,
    /// Transient EINTR/EAGAIN/EBUSY; the caller may service other work and retry.
    Interrupted,
}
/// Whether an enter error permits retry without invalidating published requests.
fn transient(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EINTR | libc::EAGAIN | libc::EBUSY)
    )
}

/// Reject offsets, lengths, spent CLOSEs, and foreign/disarmed
/// cancellation tokens before admission; success does not change lifecycle state.
fn validate(operation: &Operation, shared: &Shared, arena: &mut Arena) -> io::Result<()> {
    let invalid = match operation {
        Operation::ReadAt { offset, .. } | Operation::WriteAt { offset, .. } => {
            *offset > i64::MAX as u64
        }
        Operation::Fallocate { offset, len, .. } => {
            *offset > i64::MAX as u64
                || *len > i64::MAX as u64
                || offset
                    .checked_add(*len)
                    .is_none_or(|end| end > i64::MAX as u64)
        }
        Operation::Ftruncate { len, .. } => *len > i64::MAX as u64,
        Operation::SendRange { buffer, range, .. } => buffer.get(range.clone()).is_none(),
        Operation::Close(close) => close.file.is_none(),
        Operation::Cancel(target) => !target.is_active_for(shared.id),
        _ => false,
    };
    if invalid
        || operation
            .buffer()
            .is_some_and(|b| b.len() > i32::MAX as usize)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid owned operation arguments",
        ));
    }
    if let Operation::Cancel(target) = operation
        && matches!(
            arena.record(target.id()).operation,
            Some(Operation::Close(_))
        )
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "accepted CLOSE operations cannot be canceled",
        ));
    }
    Ok(())
}
/// Build an SQE from stable owned inputs. Does not publish or advance lifecycle;
/// the caller must retain those inputs through the eventual terminal CQE.
fn entry(operation: &mut Operation) -> io_uring::squeue::Entry {
    match operation {
        Operation::Nop => opcode::Nop::new().build(),
        Operation::ReadAt {
            file,
            buffer,
            offset,
        } => opcode::Read::new(
            types::Fd(file.as_raw_fd()),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
        .offset(*offset)
        .build(),
        Operation::WriteAt {
            file,
            buffer,
            offset,
        } => opcode::Write::new(
            types::Fd(file.as_raw_fd()),
            buffer.as_ptr(),
            buffer.len() as u32,
        )
        .offset(*offset)
        // Override append per request, including changes after SQ publication.
        // Unsupported kernels must fail the write; never retry without this flag.
        .rw_flags(libc::RWF_NOAPPEND)
        .build(),
        Operation::Fsync { file, data_only } => opcode::Fsync::new(types::Fd(file.as_raw_fd()))
            .flags(if *data_only {
                types::FsyncFlags::DATASYNC
            } else {
                types::FsyncFlags::empty()
            })
            .build(),
        Operation::Fallocate {
            file,
            offset,
            len,
            mode,
        } => opcode::Fallocate::new(types::Fd(file.as_raw_fd()), *len)
            .offset(*offset)
            .mode(*mode)
            .build(),
        Operation::Ftruncate { file, len } => {
            opcode::Ftruncate::new(types::Fd(file.as_raw_fd()), *len).build()
        }
        Operation::OpenAt {
            directory,
            path,
            flags,
            mode,
        } => opcode::OpenAt::new(
            types::Fd(directory.as_ref().map_or(libc::AT_FDCWD, |f| f.as_raw_fd())),
            path.as_ptr(),
        )
        .flags(*flags | libc::O_CLOEXEC)
        .mode(*mode)
        .build(),
        Operation::Close(close) => {
            opcode::Close::new(types::Fd(close.file.as_ref().unwrap().as_raw_fd()))
                .build()
                // A socket's final release can wait for SO_LINGER. Keep that
                // work off the thread submitting the ring as well as Drop.
                .flags(io_uring::squeue::Flags::ASYNC)
        }
        Operation::Socket {
            domain,
            socket_type,
            protocol,
        } => opcode::Socket::new(*domain, *socket_type | libc::SOCK_CLOEXEC, *protocol).build(),
        Operation::Accept { socket } => opcode::Accept::new(
            types::Fd(socket.as_raw_fd()),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
        .flags(libc::SOCK_CLOEXEC)
        .build(),
        Operation::Connect { socket, address } => {
            let (addr, len) = address.raw();
            opcode::Connect::new(types::Fd(socket.as_raw_fd()), addr, len).build()
        }
        Operation::Send { socket, buffer } => opcode::Send::new(
            types::Fd(socket.as_raw_fd()),
            buffer.as_ptr(),
            buffer.len() as u32,
        )
        .flags(libc::MSG_NOSIGNAL)
        .build(),
        Operation::SendRange {
            socket,
            buffer,
            range,
        } => {
            let bytes = &buffer[range.clone()];
            opcode::Send::new(
                types::Fd(socket.as_raw_fd()),
                bytes.as_ptr(),
                bytes.len() as u32,
            )
            .flags(libc::MSG_NOSIGNAL)
            .build()
        }
        Operation::Recv { socket, buffer } => opcode::Recv::new(
            types::Fd(socket.as_raw_fd()),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
        .build(),
        Operation::Shutdown { socket, how } => opcode::Shutdown::new(
            types::Fd(socket.as_raw_fd()),
            match how {
                Shutdown::Read => libc::SHUT_RD,
                Shutdown::Write => libc::SHUT_WR,
                Shutdown::Both => libc::SHUT_RDWR,
            },
        )
        .build(),
        Operation::PollAdd { file, events } => {
            opcode::PollAdd::new(types::Fd(file.as_raw_fd()), *events).build()
        }
        Operation::Cancel(target) => opcode::AsyncCancel::new(target.id().raw()).build(),
    }
}
/// Translate a terminal CQE into a typed result, adopting successful fresh fds
/// even when the submitter abandoned interest. Does not update the arena itself.
fn value(operation: &Operation, result: i32) -> io::Result<Value> {
    if result < 0 {
        return Err(io::Error::from_raw_os_error(-result));
    }
    Ok(match operation {
        Operation::OpenAt { .. } | Operation::Socket { .. } | Operation::Accept { .. } => {
            // SAFETY: These constructors return a fresh, exclusively owned fd on
            // success, never an io_uring instance. Adopt it without revalidation
            // even for ignored operations so it cannot leak.
            Value::File(unsafe { Fd::from_owned_fd_unchecked(OwnedFd::from_raw_fd(result)) })
        }
        Operation::ReadAt { .. }
        | Operation::WriteAt { .. }
        | Operation::Send { .. }
        | Operation::SendRange { .. }
        | Operation::Recv { .. } => Value::Count(result as usize),
        Operation::PollAdd { .. } => Value::Poll(result as u32),
        _ => Value::Done,
    })
}
#[cfg(test)]
mod tests {
    #![forbid(unsafe_code)]
    use super::*;
    #[test]
    fn exhausted_owner_counter_never_wraps_or_reuses_an_id() {
        use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
        let counter = AtomicU32::new(MAX_DRIVER_ID);
        let owner = DriverId::allocate_from(&counter).unwrap();
        assert_eq!(owner, DriverId(MAX_DRIVER_ID));
        assert_eq!(
            OperationId::new(owner, (1usize << 40) - 1).unwrap().raw(),
            u64::MAX
        );
        assert_eq!(counter.load(Relaxed), MAX_DRIVER_ID + 1);
        for _ in 0..3 {
            assert!(DriverId::allocate_from(&counter).is_err());
        }
        assert_eq!(counter.load(Relaxed), MAX_DRIVER_ID + 1);
    }
}

#[cfg(all(test, not(miri)))]
mod pressure_tests;
