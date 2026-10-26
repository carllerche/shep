//! The unsafe SQ publication, ring-driving, and descriptor-adoption boundary.
#![allow(unsafe_code)]
use crate::{
    Io, Operation, OperationHandle, Rejected, Value,
    arena::{Arena, OperationId, Record},
};
use io_uring::{IoUring, opcode, types};
use std::{
    cell::RefCell,
    fs::File,
    io,
    net::Shutdown,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd},
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

/// Deferred notifications for completed I/O operations.
///
/// Pass this to [`Driver::reap_into`] to process available completions. Reaping
/// updates operation state immediately and adds waiter notifications here.
/// Call [`Progress::wake`] when ready to notify those waiters.
///
/// Multiple calls to `reap_into` may accumulate into the same `Progress` before
/// waking. I/O results remain available through their operation handles; they
/// are not stored here. Reuse this value to retain its notification allocation.
///
/// # Example
///
/// ```no_run
/// # fn example(driver: &mut shep_smp_io2::Driver) {
/// let mut progress = shep_smp_io2::Progress::default();
/// loop {
///     let _completed = driver.reap_into(&mut progress);
///     progress.wake();
///     // Run ready tasks and submit or wait for more I/O.
/// }
/// # }
/// ```
#[must_use = "call wake() to notify operation waiters after reaping"]
#[derive(Debug, Default)]
pub struct Progress {
    /// Completion notifications detached from slab IDs, ready to run without arena borrows.
    wakes: Vec<Waker>,
}
impl Progress {
    /// Notify all accumulated waiters, retaining notification storage for reuse.
    ///
    /// Operation states have already been updated by `reap_into`. Each
    /// notification is delivered even if another waiter panics. This does not
    /// access operation storage.
    pub fn wake(&mut self) {
        for waker in self.wakes.drain(..) {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| waker.wake()));
        }
    }
}
/// Local ring and lifecycle storage retained by Io clones and active cancel pins.
pub(crate) struct Shared {
    /// Process-unique owner prefix used to reject handles from another Io.
    pub(crate) id: DriverId,
    /// Shared operation state; no borrow may span application waker callbacks.
    pub(crate) arena: RefCell<Arena>,
    /// Live kernel queues; None after Driver destruction, even if Io clones remain.
    ring: RefCell<Option<IoUring>>,
}

impl Shared {
    /// Validate and publish a Submitted request, rolling it back if SQ push fails.
    pub(crate) fn push(&self, operation: Operation) -> Result<OperationHandle, Rejected> {
        if let Err(error) = validate(&operation, self) {
            return Err(Rejected { error, operation });
        }
        let mut ring = self.ring.borrow_mut();
        let Some(ring) = ring.as_mut() else {
            return Err(Rejected {
                operation,
                error: io::ErrorKind::BrokenPipe.into(),
            });
        };
        let mut arena = self.arena.borrow_mut();
        let id = arena.insert(operation)?;
        let operation = arena.record(id).operation.as_mut().unwrap();
        let entry = entry(operation).user_data(id.raw());
        // SAFETY: The slab owns every descriptor, initialized buffer, path and
        // boxed address before publication. Inputs stay stable through terminal
        // CQE. Releasing the submitter never releases the kernel's reference.
        if unsafe { ring.submission().push(&entry) }.is_err() {
            return Err(arena.reject(id, io::ErrorKind::WouldBlock.into()));
        }
        if let Operation::Close(close) = operation {
            // CLOSE now owns the descriptor and cannot be canceled through this
            // driver. Never recover the number from a CQE error: even ECANCELED
            // can come from flush after the fd was removed and reused.
            let _ = close.file.take().unwrap().into_raw_fd();
        }
        Ok(OperationHandle::new(id))
    }
    /// Whether this live ring has published SQEs awaiting submission.
    pub(crate) fn has_pending(&self) -> bool {
        self.ring
            .borrow_mut()
            .as_mut()
            .is_some_and(|r| !r.submission().is_empty())
    }
}

/// Single-owner ring driver. Push, submit, reap and wait are separate phases.
/// The configured bound excludes one reserved cancellation operation.
/// Destruction closes admission, cancels outstanding requests, and blocks until
/// all terminal completions are reaped. Completed results remain available via
/// retained Io clones. A fatal teardown error aborts the process rather than
/// releasing resources that the kernel may still access.
pub struct Driver {
    /// Driver-owned reference to the ring/slab; clones can outlive the driver
    /// to consume completed records, but cannot submit once the ring is closed.
    io: Io,
    /// Removed records destroyed after releasing ring and arena borrows.
    discarded: Vec<Record>,
    /// One internal cancellation, outside the public admission reserve.
    shutdown_cancel_pending: bool,
}
impl Driver {
    /// Create an empty ring with max_in_flight ordinary slots plus one cancel reserve.
    /// Fail on invalid capacity (outside 1..=32767), unsupported extended waits,
    /// ring creation errors, or exhausted process DriverIds.
    pub fn new(max_in_flight: usize) -> io::Result<Self> {
        if !(1..=32767).contains(&max_in_flight) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid operation limit",
            ));
        }
        let ring = IoUring::new((max_in_flight + 1).next_power_of_two() as u32)?;
        if !ring.params().is_feature_ext_arg() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "extended io_uring waits required",
            ));
        }
        let id = DriverId::allocate()?;
        Ok(Self {
            discarded: Vec::new(),
            shutdown_cancel_pending: false,
            io: Io {
                shared: Rc::new(Shared {
                    id,
                    arena: RefCell::new(Arena::new(max_in_flight, id)),
                    ring: RefCell::new(Some(ring)),
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
    /// all requests for retry; fatal errors mark failure and extract/wake waiters.
    pub fn submit(&mut self) -> io::Result<usize> {
        let result = {
            let mut ring = self.io.shared.ring.borrow_mut();
            let ring = ring.as_mut().expect("live driver");
            if ring.submission().is_empty() {
                return Ok(0);
            }
            ring.submit()
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
            self.io.shared.arena.borrow_mut().fail();
            let mut progress = Progress::default();
            self.io
                .shared
                .arena
                .borrow_mut()
                .wakers_into(&mut progress.wakes);
            progress.wake();
        }
        result
    }
    /// Reap only: never calls submit or runs an application future.
    /// For each original CQE, Submitted/Waiting -> Completed, or keep Ignored
    /// until its final ref retires, then remove it. Cancellation CQEs also end
    /// their target pins. Append extracted wakers for Progress::wake outside
    /// borrows; abandoned inputs/results are destroyed after the CQ snapshot.
    /// Panics on invalid identities, duplicate CQEs, or unsupported nonterminal CQEs.
    ///
    /// Return the number of terminal CQEs processed by this call, including
    /// cancellation and watch completions. Existing notifications in `progress`
    /// are preserved; call [`Progress::wake`] to deliver them outside borrows.
    pub fn reap_into(&mut self, progress: &mut Progress) -> usize {
        let mut retired = 0;
        {
            let mut ring = self.io.shared.ring.borrow_mut();
            let ring = ring.as_mut().expect("live driver");
            let mut completions = ring.completion();
            if completions.is_empty() {
                return 0;
            }
            let mut arena = self.io.shared.arena.borrow_mut();
            for cqe in &mut completions {
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
                    retired += 1;
                    continue;
                }
                let id = OperationId::from_raw(cqe.user_data());
                id.check(self.io.shared.id);
                // No exposed opcode produces MORE/NOTIF CQEs. Extensions must
                // implement their full kernel-retirement protocol first.
                assert!(
                    !io_uring::cqueue::more(cqe.flags()),
                    "unsupported nonterminal CQE"
                );
                let record = arena.record(id);
                let operation = record.operation.as_mut().expect("kernel-owned operation");
                let dependency = match operation {
                    Operation::Cancel(target) => target.disarm(),
                    _ => None,
                };
                let result = value(operation, cqe.result());
                let (wake, released) = arena.complete(id, result);
                if let Some(wake) = wake {
                    progress.wakes.push(wake);
                }
                if let Some(record) = released {
                    self.discarded.push(record);
                }
                if let Some(target) = dependency
                    && let Some(record) = arena.unpin(target)
                {
                    self.discarded.push(record);
                }
                retired += 1;
            }
        }
        self.discarded.clear();
        retired
    }
    /// Submit a batch and wait for a CQE/deadline; the runtime decides when to sleep.
    /// Does not reap or advance record states; call reap_into to observe completion.
    /// An expired deadline skips submission. Transient errors return Interrupted;
    /// fatal errors close admission and wake Waiting records as in submit.
    pub fn submit_and_wait(&mut self, deadline: Option<Instant>) -> io::Result<WaitOutcome> {
        let result = {
            let mut ring = self.io.shared.ring.borrow_mut();
            let ring = ring.as_mut().expect("live driver");
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
    /// Stop admission and drive accepted work to terminal CQEs. Pending watches
    /// and receives must be explicitly canceled by their higher-level owner.
    /// Repeated reap turns pending records into Completed or removes Ignored
    /// records after their refs retire. Completed submitter claims remain for
    /// later consumption. Return enter errors; this can wait indefinitely for I/O.
    pub fn drain(&mut self) -> io::Result<()> {
        self.io.close();
        let mut progress = Progress::default();
        while self.in_flight() != 0 {
            match self.submit() {
                Err(e) if transient(&e) => {}
                result => {
                    result?;
                }
            }
            self.reap_into(&mut progress);
            progress.wake();
            if self.in_flight() != 0 {
                self.submit_and_wait(None)?;
            }
        }
        Ok(())
    }
    /// Drive teardown without the normal fatal-error callback path. All waiter
    /// notifications remain deferred until every original request has retired.
    fn shutdown_enter(&mut self, progress: &mut Progress, wait: bool) -> io::Result<()> {
        let result = {
            let mut ring = self.io.shared.ring.borrow_mut();
            let ring = ring.as_mut().expect("live driver");
            if wait {
                ring.submit_and_wait(1)
            } else {
                ring.submit()
            }
        };
        self.reap_into(progress);
        match result {
            Ok(_) => Ok(()),
            Err(error) if transient(&error) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn cancel_and_drain(&mut self, progress: &mut Progress) -> io::Result<()> {
        // Also reject public cancellation submissions from reentrant destructors.
        self.io.shared.arena.borrow_mut().fail();
        self.reap_into(progress);
        // Ensure every accepted SQE precedes its cancellation in submission order.
        while self.has_pending() {
            self.shutdown_enter(progress, false)?;
        }
        let mut cursor = 0;
        loop {
            let target = self
                .io
                .shared
                .arena
                .borrow()
                .next_cancel_target(&mut cursor);
            let Some(target) = target else { break };
            let entry = opcode::AsyncCancel::new(target.raw())
                .build()
                .user_data(SHUTDOWN_CANCEL);
            {
                let mut ring = self.io.shared.ring.borrow_mut();
                let ring = ring.as_mut().expect("live driver");
                // SAFETY: This SQE contains only the target's identity, no user
                // pointers. Admission is failed, so slots cannot be reused. All
                // prior SQEs were submitted, and each internal cancel is reaped
                // before publishing the next, leaving space even if the public
                // cancellation reserve was occupied at shutdown.
                unsafe { ring.submission().push(&entry) }.expect("shutdown SQ space");
            }
            self.shutdown_cancel_pending = true;
            while self.shutdown_cancel_pending {
                self.shutdown_enter(progress, true)?;
            }
        }
        // Cancel success is not proof of original-request retirement. In
        // particular, EALREADY means we must wait for natural completion.
        while self.in_flight() != 0 {
            self.shutdown_enter(progress, true)?;
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
        let mut progress = Progress::default();
        if let Err(error) = self.cancel_and_drain(&mut progress) {
            eprintln!("io_uring teardown failed before request retirement: {error}");
            std::process::abort();
        }
        guard.retired = true;
        let ring = self.io.shared.ring.borrow_mut().take();
        drop(ring);
        progress.wake();
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

/// Reject offsets, lengths, spent CLOSEs, and foreign/disarmed cancellation tokens
/// before admission; on success this does not change lifecycle state.
fn validate(operation: &Operation, shared: &Shared) -> io::Result<()> {
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
            shared.arena.borrow_mut().record(target.id()).operation,
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
            opcode::Close::new(types::Fd(close.file.as_ref().unwrap().as_raw_fd())).build()
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
            // success. Adopt it even for ignored operations so it cannot leak.
            Value::File(unsafe { File::from_raw_fd(result) })
        }
        Operation::ReadAt { .. }
        | Operation::WriteAt { .. }
        | Operation::Send { .. }
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
    fn progress_reuses_storage_and_delivers_each_notification_once() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::Relaxed},
        };
        struct Count(AtomicUsize);
        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Relaxed);
            }
        }
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let mut progress = Progress::default();
        progress.wakes.push(Waker::from(count.clone()));
        progress.wakes.push(Waker::from(count.clone()));
        let capacity = progress.wakes.capacity();
        let storage = progress.wakes.as_ptr();
        progress.wake();
        assert_eq!(count.0.load(Relaxed), 2);
        assert!(progress.wakes.is_empty());
        assert_eq!(progress.wakes.capacity(), capacity);
        assert_eq!(progress.wakes.as_ptr(), storage);
        progress.wake();
        assert_eq!(count.0.load(Relaxed), 2);
        progress.wakes.push(Waker::from(count.clone()));
        progress.wake();
        assert_eq!(count.0.load(Relaxed), 3);
        assert_eq!(progress.wakes.as_ptr(), storage);
    }
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
