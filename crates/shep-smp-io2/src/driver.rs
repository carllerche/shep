//! The unsafe SQ publication and descriptor-adoption boundary.
#![allow(unsafe_code)]
use crate::{
    Operation,
    lifecycle::{self, IoId, OperationId},
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
    Count(usize),
    File(File),
    Poll(u32),
    Done,
}
impl Value {
    pub fn into_count(self) -> usize {
        match self {
            Self::Count(n) => n,
            _ => panic!("completion is not a byte count"),
        }
    }
}
#[derive(Debug)]
pub struct Completion {
    pub result: io::Result<Value>,
    pub operation: Operation,
}
#[derive(Debug)]
pub struct Rejected {
    pub error: io::Error,
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
    pub retired: usize,
    wakes: Vec<Waker>,
}
impl Progress {
    pub fn wake(self) {
        for waker in self.wakes {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| waker.wake()));
        }
    }
}
struct Shared {
    id: IoId,
    arena: RefCell<lifecycle::Arena>,
    ring: RefCell<Option<IoUring>>,
}

/// Local push-only producer. It never submits, waits, or queues rejected work.
#[derive(Clone)]
pub struct Io {
    shared: Rc<Shared>,
}
impl Io {
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
    pub fn in_flight(&self) -> usize {
        self.shared.arena.borrow().in_flight()
    }
    pub fn has_pending(&self) -> bool {
        self.shared
            .ring
            .borrow_mut()
            .as_mut()
            .is_some_and(|r| !r.submission().is_empty())
    }
    /// Stop ordinary admission; cancellation remains possible through its reserve.
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
    id: Option<OperationId>,
}
impl OperationHandle {
    pub fn try_complete(&mut self, io: &Io) -> Option<Completion> {
        let id = self.id.expect("operation already consumed");
        id.check(io.shared.id);
        let result = io.shared.arena.borrow_mut().take(id)?;
        self.id = None;
        Some(result)
    }
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
    /// Release submitter interest, keeping kernel-owned resources alive. Safe
    /// after result consumption or an earlier release; a wrong Io panics before
    /// touching either slab and leaves this handle usable with its actual Io.
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
    io: Option<Io>,
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
    fn disarm(&mut self) -> Option<OperationId> {
        self.io.take().map(|_| self.id)
    }
}
impl Drop for CancelTarget {
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
    io: Io,
}
impl Driver {
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
                    arena: RefCell::new(lifecycle::Arena::new(max_in_flight, id)),
                    ring: RefCell::new(Some(ring)),
                }),
            },
        })
    }
    pub fn io(&self) -> &Io {
        &self.io
    }
    pub fn push(&self, operation: Operation) -> Result<OperationHandle, Rejected> {
        self.io.push(operation)
    }
    pub fn in_flight(&self) -> usize {
        self.io.in_flight()
    }
    pub fn has_pending(&self) -> bool {
        self.io.has_pending()
    }
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
    /// Explicit batching + wait. The runtime decides when it is safe to sleep.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    Ready,
    TimedOut,
    Interrupted,
}
fn transient(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EINTR | libc::EAGAIN | libc::EBUSY)
    )
}

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
