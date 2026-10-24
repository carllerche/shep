//! The unsafe SQ publication and descriptor-adoption boundary.
#![allow(unsafe_code)]
use crate::{
    Operation,
    arena::{Arena, IoId, OperationId},
};
use io_uring::{IoUring, opcode, types};
use std::{
    cell::RefCell,
    fs::File,
    io,
    net::Shutdown,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd},
    rc::Rc,
    task::{Context, Poll, Waker},
    time::Instant,
};

/// Opcode-specific ownership of a successful completion.
#[derive(Debug)]
pub enum Value {
    /// Number of bytes transferred by a read, write, send, or receive.
    Count(usize),
    /// Fresh descriptor returned by open, socket, or accept; closed on value drop.
    File(File),
    /// Ready poll-event bits from a single-shot PollAdd.
    Poll(u32),
    /// Successful completion of an operation with no returned payload.
    Done,
}
impl Value {
    /// Extract a byte count; panic if this completion belongs to another opcode.
    pub fn into_count(self) -> usize {
        match self {
            Self::Count(n) => n,
            _ => panic!("completion is not a byte count"),
        }
    }
}
/// Terminal result and original inputs, transferred out of a Completed record.
#[derive(Debug)]
pub struct Completion {
    /// Typed CQE result, including kernel errors; inputs are returned either way.
    pub result: io::Result<Value>,
    /// Original owned request, including its reusable buffer when applicable.
    pub operation: Operation,
}
/// Admission or publication failed; the caller still owns every unsubmitted input.
#[derive(Debug)]
pub struct Rejected {
    /// Validation, closed-driver, or immediate-capacity error; no waiter is queued.
    pub error: io::Error,
    /// Original owned request, including its reusable buffer when applicable.
    pub operation: Operation,
}
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for Rejected {}

/// All results have entered lifecycle state before callbacks are invoked.
/// Notification never looks up an old slab index.
#[must_use = "call wake() to notify operation waiters after reaping"]
#[derive(Debug, Default)]
pub struct Progress {
    /// Terminal CQEs reaped in this batch, including cancellation and watch operations.
    pub retired: usize,
    /// Completion notifications detached from slab IDs, ready to run without arena borrows.
    wakes: Vec<Waker>,
}
impl Progress {
    /// Deliver extracted notifications, containing each panic so others still run.
    /// Record transitions already happened during reap; this does not access the slab.
    pub fn wake(self) {
        for waker in self.wakes {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| waker.wake()));
        }
    }
}
/// Local ring and lifecycle storage retained by Io clones and active cancel pins.
struct Shared {
    /// Process-unique owner prefix used to reject handles from another Io.
    id: IoId,
    /// Shared operation state; no borrow may span application waker callbacks.
    arena: RefCell<Arena>,
    /// Live kernel queues; None after Driver destruction, even if Io clones remain.
    ring: RefCell<Option<IoUring>>,
}

/// Local push-only producer. It never submits, waits, or queues rejected work.
#[derive(Clone)]
pub struct Io {
    /// Shared local ownership of the ring and slab; ordinary operation handles do not clone it.
    shared: Rc<Shared>,
}
impl Io {
    /// Publish owned inputs as a new Submitted record and return its submitter claim.
    /// No submit syscall, reap, or waiting occurs. Validation/capacity errors return
    /// the operation; failed SQ push rolls Submitted back to removed. CLOSE transfers
    /// its unique File into a kernel-owned fd only after successful publication.
    pub fn push(&self, operation: Operation) -> Result<OperationHandle, Rejected> {
        if let Err(error) = validate(&operation, &self.shared) {
            return Err(Rejected { error, operation });
        }
        let mut ring = self.shared.ring.borrow_mut();
        let Some(ring) = ring.as_mut() else {
            return Err(Rejected {
                operation,
                error: io::ErrorKind::BrokenPipe.into(),
            });
        };
        let mut arena = self.shared.arena.borrow_mut();
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
            // CLOSE now owns the descriptor. Suppress std File's destructor so
            // it cannot close an unrelated fd reused before the CLOSE CQE.
            close.submitted = Some(close.file.take().unwrap().into_raw_fd());
        }
        Ok(OperationHandle { id: Some(id) })
    }
    /// Count accepted SQEs whose terminal CQEs have not been reaped; pins and
    /// retained completed results do not count. Includes the runtime's watch, if any.
    pub fn in_flight(&self) -> usize {
        self.shared.arena.borrow().in_flight()
    }
    /// Whether published SQEs still await submission; false once the ring is gone.
    pub fn has_pending(&self) -> bool {
        self.shared
            .ring
            .borrow_mut()
            .as_mut()
            .is_some_and(|r| !r.submission().is_empty())
    }
    /// Open admission -> ordinary-closed; cancellation remains possible through
    /// its reserve. Existing record states and kernel ownership are unchanged.
    pub fn close(&self) {
        self.shared.arena.borrow_mut().close();
    }
    pub fn is_failed(&self) -> bool {
        self.shared.arena.borrow().is_failed()
    }
}

/// Submitter's claim on an operation stored in an Io's slab. Not a Future.
/// Pass the allocating Io to access or explicitly release this claim. Dropping
/// the token alone does not release it; adapters must release on cancellation.
#[must_use = "consume the result or call release(&Io) to abandon it"]
#[derive(Debug)]
pub struct OperationHandle {
    /// Live submitter claim. None after completion consumption or explicit release;
    /// the token owns no Io and dropping it alone cannot change the slab.
    id: Option<OperationId>,
}
impl OperationHandle {
    /// Consume an already reaped completion without submitting, reaping, or waiting.
    /// Submitted/Waiting returns None unchanged and does not install a waker,
    /// including after unresolved driver failure; poll_complete checks that failure.
    /// Completed -> Ignored(None) returns Some(Completion), moving out the result
    /// and original inputs and clearing this handle's claim. Remove the slot only
    /// if no cancellation pins remain; otherwise retain it until the final unpin.
    /// Panics if this handle was consumed/released, or io is not its allocating Io.
    pub fn try_complete(&mut self, io: &Io) -> Option<Completion> {
        let id = self.id.expect("operation already consumed");
        id.check(io.shared.id);
        let result = io.shared.arena.borrow_mut().take(id)?;
        self.id = None;
        Some(result)
    }
    /// Poll the result: Completed is consumed exactly as in [`Self::try_complete`].
    /// Otherwise Submitted -> Waiting, or replace the existing Waiting waker, then
    /// recheck after dropping the replaced waker, which may reenter the driver.
    /// Return Pending while awaiting a CQE; no submit or reap is performed here.
    /// Panics for a consumed/released handle, wrong Io, or unresolved driver failure.
    pub fn poll_complete(&mut self, io: &Io, cx: &mut Context<'_>) -> Poll<Completion> {
        // Clone outside borrows: custom wakers may reenter the driver.
        let waker = cx.waker().clone();
        if let Some(result) = self.try_complete(io) {
            return Poll::Ready(result);
        }
        assert!(
            !io.is_failed(),
            "io_uring driver failed with unresolved resources"
        );
        let old = io.shared.arena.borrow_mut().wait(self.id.unwrap(), waker);
        drop(old);
        match self.try_complete(io) {
            Some(result) => Poll::Ready(result),
            None => Poll::Pending,
        }
    }
    /// Release submitter interest, keeping kernel-owned resources alive.
    /// An already consumed/released handle is a no-op. With a live claim, a wrong
    /// Io panics before slab access, leaving the claim usable with its actual Io.
    /// Submitted/Waiting -> Ignored(None); Completed -> Ignored(Some(result)).
    /// If no kernel refs remain, remove immediately; otherwise retain the record
    /// until they retire. Clear the handle's ID in either case. Never cancel I/O.
    pub fn release(&mut self, io: &Io) {
        if let Some(id) = self.id {
            id.check(io.shared.id);
            self.id = None;
            let released = io.shared.arena.borrow_mut().abandon(id);
            drop(released);
        }
    }
    /// Pin this identity until cancellation retires or the unsubmitted token is
    /// dropped. Cancellation tokens retain their Io to release that extra pin.
    /// Submitted/Waiting/Completed keep their state and gain one identity ref.
    /// Panics for a consumed/released handle, wrong Io, or reference overflow.
    pub fn cancel_target(&self, io: &Io) -> CancelTarget {
        let id = self.id.expect("operation already consumed");
        id.check(io.shared.id);
        io.shared.arena.borrow_mut().pin(id);
        CancelTarget {
            io: Some(io.clone()),
            id,
        }
    }
}

/// A cancellation target is an owned identity reference, not a reusable integer.
pub struct CancelTarget {
    /// Some while the target pin is armed; None after its cancellation CQE is reaped.
    /// Retains the allocating Io so an unsubmitted token can unpin on drop.
    io: Option<Io>,
    /// Pinned target identity; its slab index cannot be reused until this pin ends.
    id: OperationId,
}
impl std::fmt::Debug for CancelTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelTarget")
            .field("id", &self.id)
            .finish()
    }
}
impl CancelTarget {
    /// At the cancellation CQE, armed Some(Io) -> disarmed None, releasing the Io clone.
    /// Return the target ID for the driver to unpin after retiring the cancel record;
    /// this method itself does not unpin. Already disarmed returns None.
    fn disarm(&mut self) -> Option<OperationId> {
        self.io.take().map(|_| self.id)
    }
}
impl Drop for CancelTarget {
    /// Armed -> disarmed and release the target pin; remove an Ignored target
    /// only if that was its final kernel ref. A retired token is already disarmed.
    fn drop(&mut self) {
        if let Some(io) = self.io.take() {
            let released = io.shared.arena.borrow_mut().unpin(self.id);
            drop(released);
        }
    }
}

/// Single-owner ring driver. Push, submit, reap and wait are separate phases.
/// The configured bound excludes one reserved cancellation operation.
/// Explicitly drain before destruction. Dropping with unresolved work retains
/// its resources conservatively; it never blocks on an arbitrary socket read.
pub struct Driver {
    /// Driver-owned reference to the ring/slab; clones can outlive the driver
    /// to consume completed records, but cannot submit once the ring is closed.
    io: Io,
}
impl Driver {
    /// Create an empty ring with max_in_flight ordinary slots plus one cancel reserve.
    /// Fail on invalid capacity (outside 1..=32767), unsupported extended waits,
    /// ring creation errors, or exhausted process Io IDs.
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
        let id = IoId::allocate()?;
        Ok(Self {
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
            let wakes = self.io.shared.arena.borrow_mut().wakers();
            Progress { retired: 0, wakes }.wake();
        }
        result
    }
    /// Reap only: never calls submit or runs an application future.
    /// For each original CQE, Submitted/Waiting -> Completed, or keep Ignored
    /// until its final ref retires, then remove it. Cancellation CQEs also end
    /// their target pins. Return extracted wakers for Progress::wake outside
    /// borrows; abandoned inputs/results are destroyed after the CQ snapshot.
    /// Panics on invalid identities, duplicate CQEs, or unsupported nonterminal CQEs.
    pub fn reap(&mut self) -> Progress {
        let mut progress = Progress::default();
        let mut discarded = Vec::new();
        {
            let mut ring = self.io.shared.ring.borrow_mut();
            let ring = ring.as_mut().expect("live driver");
            let mut completions = ring.completion();
            if completions.is_empty() {
                return progress;
            }
            let mut arena = self.io.shared.arena.borrow_mut();
            for cqe in &mut completions {
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
                finish_close(operation, cqe.result());
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
                    discarded.push(record);
                }
                if let Some(target) = dependency
                    && let Some(record) = arena.unpin(target)
                {
                    discarded.push(record);
                }
                progress.retired += 1;
            }
        }
        drop(discarded);
        progress
    }
    /// Submit a batch and wait for a CQE/deadline; the runtime decides when to sleep.
    /// Does not reap or advance record states; call reap to observe completion.
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
        while self.in_flight() != 0 {
            match self.submit() {
                Err(e) if transient(&e) => {}
                result => {
                    result?;
                }
            }
            self.reap().wake();
            if self.in_flight() != 0 {
                self.submit_and_wait(None)?;
            }
        }
        Ok(())
    }
}
impl Drop for Driver {
    /// Close admission, reap available CQEs, then mark failed and extract pending
    /// wakers (Waiting -> Submitted). Retain unretired inputs conservatively and
    /// set the shared ring to None; no blocking drain occurs. Completed results
    /// remain available through retained Io clones until consumed/released.
    fn drop(&mut self) {
        self.io.close();
        self.reap().wake();
        let wakes = self.io.shared.arena.borrow_mut().wakers();
        let unresolved = self.io.shared.arena.borrow_mut().retain_unresolved();
        // Closing a ring isn't used as proof that userspace memory is retired.
        std::mem::forget(unresolved);
        let ring = self.io.shared.ring.borrow_mut().take();
        drop(ring);
        Progress { retired: 0, wakes }.wake();
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
fn validate(operation: &Operation, shared: &Rc<Shared>) -> io::Result<()> {
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
        Operation::Close(close) => close.file.is_none() || close.submitted.is_some(),
        Operation::Cancel(target) => target.io.is_none() || !target.id.belongs_to(shared.id),
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
        Operation::Cancel(target) => opcode::AsyncCancel::new(target.id.raw()).build(),
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
/// At a CLOSE CQE, submitted Some(fd) -> None. ECANCELED re-adopts and closes
/// the unexecuted descriptor; other results discard the saved fd number. Never
/// retry closing it: the kernel may have closed it and reused that number.
fn finish_close(operation: &mut Operation, result: i32) {
    if let Operation::Close(close) = operation
        && let Some(fd) = close.submitted.take()
        && result == -libc::ECANCELED
    {
        // SAFETY: A canceled CLOSE never executed. Its unique fd was transferred
        // at publication and has no other safe owner. Re-adopt it exactly once.
        drop(unsafe { File::from_raw_fd(fd) });
    }
}
