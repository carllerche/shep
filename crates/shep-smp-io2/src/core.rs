//! Private io_uring ownership and parking behind a safe module API.
//!
//! No caller supplies SQEs, raw pointers, borrowed buffers, or raw descriptors.
//! Accepted operations own all kernel-visible resources until their CQEs retire.
//! There are no callbacks into user code while processing CQEs.
//!
//! The reactor is local, but its weak [`WakeHandle`] can cross threads.
#![allow(unsafe_code)]

mod arena;
mod buffer;
mod park;
mod park_state;
pub use buffer::Buffer;
pub use park::WakeHandle;

use io_uring::{IoUring, opcode, squeue, types};
use std::{
    cell::RefCell, fs::File, io, marker::PhantomData, os::fd::AsRawFd, rc::Rc, sync::Arc,
    task::Waker, time::Instant,
};

const WAKE: u64 = u64::MAX;
const REMOVE: u64 = u64::MAX - 1;

/// Owned file operations. Independent operations have no ordering guarantee.
#[derive(Debug)]
pub enum Operation {
    Read {
        file: Arc<File>,
        buffer: Buffer,
        offset: u64,
    },
    Write {
        file: Arc<File>,
        buffer: Buffer,
        offset: u64,
    },
    Sync {
        file: Arc<File>,
        data_only: bool,
    },
}
impl Operation {
    pub fn buffer(&self) -> Option<&Buffer> {
        match self {
            Self::Read { buffer, .. } | Self::Write { buffer, .. } => Some(buffer),
            Self::Sync { .. } => None,
        }
    }
    pub fn into_buffer(self) -> Option<Buffer> {
        match self {
            Self::Read { buffer, .. } | Self::Write { buffer, .. } => Some(buffer),
            Self::Sync { .. } => None,
        }
    }
}

/// A slab index shared by one operation future and its single-shot kernel op.
/// The last owner to finish removes the entry; neither retains the index after.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OperationId(u64);
impl OperationId {
    fn new(index: usize) -> Self {
        Self(index as u64)
    }
    fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug)]
pub struct Completion {
    pub id: OperationId,
    pub result: io::Result<usize>,
    pub operation: Operation,
}

/// A rejection returns the operation and all its resources to the caller.
#[derive(Debug)]
pub struct Rejected {
    pub error: io::Error,
    pub operation: Operation,
}

/// Completion delivery owns wakers, never slab indices. All CQEs are processed
/// before user callbacks, so callbacks can safely consume and reuse other slots.
#[derive(Debug, Default)]
pub struct Progress {
    pub retired: usize,
    pub wakes: Vec<Waker>,
}
#[derive(Debug)]
pub struct ProgressError {
    pub error: io::Error,
    pub progress: Progress,
}

struct Storage {
    arena: RefCell<arena::Arena>,
    ring: RefCell<Option<IoUring>>,
}
/// One local shared driver state. Futures get this safe handle, never raw SQEs
/// or kernel-visible memory. Operation resources always remain in its slab.
pub struct Operations {
    shared: Rc<Storage>,
}
impl Clone for Operations {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}
impl Operations {
    fn with_ring<R>(&self, f: impl FnOnce(&mut IoUring) -> R) -> R {
        f(self
            .shared
            .ring
            .borrow_mut()
            .as_mut()
            .expect("live reactor"))
    }
    /// Admit and publish to the SQ. The reactor batches the submission syscall.
    pub fn queue(&self, operation: Operation) -> Result<OperationId, Rejected> {
        let mut ring = self.shared.ring.borrow_mut();
        let Some(ring) = ring.as_mut() else {
            return Err(Rejected {
                operation,
                error: io::ErrorKind::BrokenPipe.into(),
            });
        };
        let mut arena = self.shared.arena.borrow_mut();
        let token = arena.insert(operation)?;
        let request = arena.get_mut(token);
        let entry = match &mut request.operation {
            Operation::Read {
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
            Operation::Write {
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
            Operation::Sync { file, data_only } => opcode::Fsync::new(types::Fd(file.as_raw_fd()))
                .flags(if *data_only {
                    types::FsyncFlags::DATASYNC
                } else {
                    types::FsyncFlags::empty()
                })
                .build(),
        }
        .user_data(token.0);
        // SAFETY: The slab already owns the descriptor and stable initialized
        // buffer. A dropped future only changes its lifecycle to Ignored; the
        // terminal CQE is required before those resources can be released.
        if unsafe { ring.submission().push(&entry) }.is_err() {
            // Failed push publishes nothing, so returning the owned values is safe.
            return Err(arena.reject(token, io::ErrorKind::WouldBlock.into()));
        }
        Ok(token)
    }
    pub fn take(&self, token: OperationId) -> Option<Completion> {
        self.shared.arena.borrow_mut().take(token)
    }
    pub fn wait(&self, token: OperationId, waker: Waker) -> Option<Waker> {
        self.shared.arena.borrow_mut().wait(token, waker)
    }
    pub fn abandon(&self, token: OperationId) {
        let released = self.shared.arena.borrow_mut().abandon(token);
        drop(released); // Waker destructors can reenter after borrow ends.
    }
    pub fn pending_wakers(&self) -> Vec<Waker> {
        self.shared.arena.borrow_mut().pending_wakers()
    }
    pub fn in_flight(&self) -> usize {
        self.shared.arena.borrow().in_flight()
    }
    pub fn has_pending(&self) -> bool {
        self.shared
            .ring
            .borrow_mut()
            .as_mut()
            .is_some_and(|ring| !ring.submission().is_empty())
    }
    pub fn is_failed(&self) -> bool {
        self.shared.arena.borrow().is_failed()
    }
    pub fn close(&self) {
        self.shared.arena.borrow_mut().close();
    }
    fn fail(&self) {
        self.shared.arena.borrow_mut().fail();
    }
    fn retain_unresolved(&self) {
        let unresolved = self.shared.arena.borrow_mut().retain_unresolved();
        std::mem::forget(unresolved);
    }
    #[cfg(all(test, not(miri)))]
    pub fn live_entries(&self) -> usize {
        self.shared.arena.borrow().live_entries()
    }
}

/// A local kernel reactor, with one reserved wake watch and bounded data I/O.
/// Requires Linux io_uring extended wait arguments (Linux 5.11+).
///
/// Call shutdown explicitly. Drop also drains accepted work, and may block. If
/// draining fails, unresolved resources are retained rather than freed while
/// the kernel could still access them. No SQPOLL, multishot or provided buffers
/// are enabled by this implementation.
pub struct Reactor {
    operations: Operations,
    event: Option<Arc<park::Event>>,
    wake_armed: bool,
    removing: bool,
    closed: bool,
    _local: PhantomData<Rc<()>>,
}

impl Reactor {
    /// Construct a reactor carrying shared lifecycle state alongside each owned
    /// operation in the same slab. Only its index is passed to the kernel.
    pub fn new(capacity: usize) -> io::Result<Self> {
        if !(1..=32766).contains(&capacity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid reactor capacity",
            ));
        }
        let ring = IoUring::new((capacity + 2).next_power_of_two() as u32)?;
        if !ring.params().is_feature_ext_arg() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "io_uring extended waits required",
            ));
        }
        Ok(Self {
            operations: Operations {
                shared: Rc::new(Storage {
                    arena: RefCell::new(arena::Arena::new(capacity)),
                    ring: RefCell::new(Some(ring)),
                }),
            },
            event: Some(park::Event::new()?),
            wake_armed: false,
            removing: false,
            closed: false,
            _local: PhantomData,
        })
    }
    pub fn wake_handle(&self) -> WakeHandle {
        WakeHandle(self.event.as_ref().map(Arc::downgrade).unwrap_or_default())
    }
    pub fn in_flight(&self) -> usize {
        self.operations.in_flight()
    }
    pub fn operations(&self) -> Operations {
        self.operations.clone()
    }

    #[cfg(all(test, not(miri)))]
    pub fn push(&mut self, operation: Operation) -> Result<OperationId, Rejected> {
        self.operations.queue(operation)
    }

    /// Submit already-published SQEs without waiting. Transient pressure is retried.
    pub fn flush(&mut self) -> io::Result<usize> {
        self.operations.with_ring(|ring| {
            if ring.submission().is_empty() {
                return Ok(0);
            }
            match ring.submit() {
                Err(e) if transient(&e) => Ok(0),
                result => result,
            }
        })
    }

    /// Retire all currently available CQEs before running any user callback.
    pub fn tick(&mut self) -> Result<Progress, ProgressError> {
        if let Err(error) = self.flush() {
            self.operations.fail();
            return Err(ProgressError {
                error,
                progress: Progress::default(),
            });
        }
        let mut progress = Progress::default();
        let mut discarded_requests = Vec::new();
        let mut error = None;
        {
            let mut ring = self.operations.shared.ring.borrow_mut();
            let ring = ring.as_mut().expect("live reactor");
            let mut arena = self.operations.shared.arena.borrow_mut();
            for cqe in ring.completion() {
                let result = cqe.result();
                match cqe.user_data() {
                    WAKE => {
                        self.wake_armed = false;
                        if result < 0 && result != -libc::ECANCELED {
                            error.get_or_insert_with(|| io::Error::from_raw_os_error(-result));
                        }
                    }
                    REMOVE => {
                        self.removing = false;
                        if result < 0 && result != -libc::ENOENT && result != -libc::EALREADY {
                            error.get_or_insert_with(|| io::Error::from_raw_os_error(-result));
                        }
                    }
                    key => {
                        let result = if result < 0 {
                            Err(io::Error::from_raw_os_error(-result))
                        } else {
                            Ok(result as usize)
                        };
                        let (waker, discarded) = arena.complete(OperationId(key), result);
                        progress.retired += 1;
                        if let Some(waker) = waker {
                            progress.wakes.push(waker);
                        }
                        if let Some(request) = discarded {
                            discarded_requests.push(request);
                        }
                    }
                }
            }
        }
        // No ring/arena borrow and no retained notification indices survive.
        drop(discarded_requests);
        match error {
            Some(error) => {
                self.operations.fail();
                Err(ProgressError { error, progress })
            }
            None => Ok(progress),
        }
    }

    #[cfg(all(test, not(miri)))]
    fn take_completed(&self) -> Vec<Completion> {
        let tokens = self.operations.shared.arena.borrow().completed();
        tokens
            .into_iter()
            .filter_map(|token| self.operations.take(token))
            .collect()
    }
    #[cfg(all(test, not(miri)))]
    pub fn poll(&mut self) -> Result<Vec<Completion>, ProgressError> {
        let _progress = self.tick()?;
        Ok(self.take_completed())
    }

    fn control(&mut self, entry: squeue::Entry) -> io::Result<()> {
        // SAFETY: Only the private PollAdd/PollRemove constructors reach here.
        // They contain no memory pointers. Event remains owned until both the
        // watch and its removal CQE retire, including during Drop.
        self.operations
            .with_ring(|ring| unsafe { ring.submission().push(&entry) })
            .map_err(|_| io::Error::from(io::ErrorKind::WouldBlock))
    }

    /// Arm the retained wakeup, drain old tokens, and submit. The caller MUST
    /// recheck its task inbox and timer deadline after this returns, before
    /// calling Park::wait. Dropping Park simply abandons this wait attempt.
    pub fn prepare_park(&mut self) -> io::Result<Park<'_>> {
        if self.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "reactor closed"));
        }
        if !self.wake_armed {
            let fd = self.event.as_ref().expect("open reactor").fd();
            self.control(
                opcode::PollAdd::new(types::Fd(fd), libc::POLLIN as u32)
                    .build()
                    .user_data(WAKE),
            )?;
            self.wake_armed = true;
        }
        self.event.as_ref().expect("open reactor").drain()?;
        let notified = self.event.as_ref().expect("open reactor").prepare()?;
        let park = Park {
            reactor: self,
            notified,
        };
        // The guard restores RUNNING on every error and abandoned attempt.
        park.reactor.flush()?;
        Ok(park)
    }

    fn wait(&mut self, deadline: Option<Instant>) -> io::Result<WaitOutcome> {
        let result = match deadline {
            Some(deadline) => {
                let duration = deadline.saturating_duration_since(Instant::now());
                if duration.is_zero() {
                    return Ok(WaitOutcome::TimedOut);
                }
                let ts = types::Timespec::from(duration);
                self.operations.with_ring(|ring| {
                    ring.submitter()
                        .submit_with_args(1, &types::SubmitArgs::new().timespec(&ts))
                })
            }
            None => self.operations.with_ring(|ring| ring.submit_and_wait(1)),
        };
        match result {
            Err(e) if transient(&e) => Ok(WaitOutcome::Interrupted),
            Err(e) if e.raw_os_error() == Some(libc::ETIME) => Ok(WaitOutcome::TimedOut),
            result => result.map(|_| WaitOutcome::Ready),
        }
    }

    pub fn shutdown_retained(&mut self) -> Result<Progress, ProgressError> {
        self.closed = true;
        self.operations.close();
        let mut progress = Progress::default();
        if self.wake_armed && !self.removing {
            self.control(opcode::PollRemove::new(WAKE).build().user_data(REMOVE))
                .map_err(|error| ProgressError {
                    error,
                    progress: Progress::default(),
                })?;
            self.removing = true;
        }
        loop {
            match self.tick() {
                Ok(mut turn) => {
                    progress.retired += turn.retired;
                    progress.wakes.append(&mut turn.wakes);
                }
                Err(mut failure) => {
                    progress.retired += failure.progress.retired;
                    progress.wakes.append(&mut failure.progress.wakes);
                    return Err(ProgressError {
                        error: failure.error,
                        progress,
                    });
                }
            }
            if self.in_flight() == 0 && !self.wake_armed && !self.removing {
                break;
            }
            if let Err(error) = self.wait(None) {
                return Err(ProgressError { error, progress });
            }
        }
        if let Some(event) = self.event.take() {
            event.close();
        }
        Ok(progress)
    }
    #[cfg(all(test, not(miri)))]
    pub fn shutdown(&mut self) -> Result<Vec<Completion>, ProgressError> {
        let _progress = self.shutdown_retained()?;
        Ok(self.take_completed())
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        // Keep completed results available to handles that outlive the reactor.
        if self.shutdown_retained().is_err() {
            if let Some(event) = &self.event {
                event.close();
            }
            self.operations.fail();
            self.operations.retain_unresolved();
        }
        // Completed values remain in the shared arena; the driver alone owns
        // ring lifetime. Handles that outlive it can still take their results.
        let ring = self.operations.shared.ring.borrow_mut().take();
        drop(ring);
    }
}

/// Result of a single wait attempt. All outcomes require the caller to recheck
/// completions, tasks and timers; a control CQE alone can cause Ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    Ready,
    TimedOut,
    Interrupted,
}

#[must_use = "recheck ready work and deadlines, then wait or abandon this park"]
pub struct Park<'a> {
    reactor: &'a mut Reactor,
    notified: bool,
}
impl Park<'_> {
    pub fn wait(self, deadline: Option<Instant>) -> io::Result<WaitOutcome> {
        if self.notified
            || self
                .reactor
                .event
                .as_ref()
                .expect("prepared reactor")
                .notified()
        {
            return Ok(WaitOutcome::Ready);
        }
        self.reactor.wait(deadline)
    }
}
impl Drop for Park<'_> {
    fn drop(&mut self) {
        if let Some(event) = &self.reactor.event {
            event.finish();
        }
    }
}
fn transient(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EINTR | libc::EAGAIN | libc::EBUSY)
    )
}

#[cfg(all(test, not(miri)))]
#[path = "core/tests/park.rs"]
mod park_tests;
#[cfg(all(test, not(miri)))]
#[path = "core/tests/reactor.rs"]
mod reactor_tests;
