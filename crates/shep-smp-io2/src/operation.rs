//! Explicit owned requests; no caller supplies raw SQEs or kernel pointers.
#![forbid(unsafe_code)]

use crate::{Buffer, Fd, Io, arena::OperationId, driver::DriverId};
use std::{
    ffi::CString,
    io,
    net::{Shutdown, SocketAddr},
    rc::Rc,
    task::{Context, Poll},
};

/// A supported io_uring opcode and all resources needed to issue it safely.
/// Offsets are explicit. Send/recv are single-shot, ordinary copying operations.
/// Multishot, zero-copy, raw uring commands and arbitrary SQE flags are not exposed.
#[derive(Debug)]
#[non_exhaustive]
pub enum Operation {
    /// Complete without performing I/O.
    Nop,
    /// Read once at an explicit file offset; short reads and EOF are returned.
    ReadAt {
        /// Descriptor retained until the read CQE.
        file: Rc<Fd>,
        /// Initialized destination; its visible length is the requested byte count.
        buffer: Buffer,
        /// Absolute byte offset; current-file-position sentinel and signed overflow are rejected.
        offset: u64,
    },
    /// Write once at an explicit file offset; a short write is returned to the caller.
    ///
    /// Always requests `RWF_NOAPPEND` (Linux 6.9+), honoring the offset even if
    /// the descriptor has `O_APPEND` set or its append flag changes concurrently.
    /// Does not change the descriptor's shared flags. Unsupported kernels report
    /// an error through the completion; the driver never retries without the flag.
    WriteAt {
        /// Descriptor retained until the write CQE.
        file: Rc<Fd>,
        /// Owned source bytes; retained even if the submitter releases its claim.
        buffer: Buffer,
        /// Absolute byte offset; never advances the descriptor's shared file cursor.
        offset: u64,
    },
    /// Request a durability barrier; callers must order prerequisite writes themselves.
    Fsync {
        /// Descriptor whose file data/metadata is synchronized.
        file: Rc<Fd>,
        /// Use DATASYNC when true; otherwise request full file synchronization.
        data_only: bool,
    },
    /// Allocate or modify a file range using Linux fallocate semantics.
    Fallocate {
        /// Descriptor retained through completion.
        file: Rc<Fd>,
        /// Start of the byte range, fitting a signed file offset.
        offset: u64,
        /// Range length; both length and offset + length must fit a signed offset.
        len: u64,
        /// Linux fallocate mode flags passed to the kernel.
        mode: i32,
    },
    /// Set file length without using or advancing a shared file cursor.
    Ftruncate {
        /// Descriptor whose length is changed.
        file: Rc<Fd>,
        /// New byte length; values beyond a signed file offset are rejected.
        len: u64,
    },
    /// Open one path and return an owned descriptor; always requests CLOEXEC.
    OpenAt {
        /// Base for relative paths; None uses the process working directory.
        directory: Option<Rc<Fd>>,
        /// Owned NUL-terminated path, stable until the CQE.
        path: CString,
        /// Linux open flags; CLOEXEC is added by the driver.
        flags: i32,
        /// Creation permission bits, used when the open creates a file.
        mode: u32,
    },
    /// Close a uniquely owned descriptor; its ownership phases are tracked by Close.
    /// Once accepted, it cannot be canceled, including during driver destruction.
    /// Flush errors are returned without retrying the close or reclaiming its fd.
    Close(Close),
    /// Create one socket and return an owned descriptor with CLOEXEC.
    Socket {
        /// Linux address family, such as AF_INET or AF_INET6.
        domain: i32,
        /// Linux socket type and flags; SOCK_CLOEXEC is added by the driver.
        socket_type: i32,
        /// Linux protocol number; zero selects the family/type default.
        protocol: i32,
    },
    /// Accept one connection and return an owned descriptor; no peer address is returned.
    Accept {
        /// Listening descriptor retained through completion.
        socket: Rc<Fd>,
    },
    /// Connect once using stable owned IPv4/IPv6 address storage.
    Connect {
        /// Descriptor being connected, retained through completion.
        socket: Rc<Fd>,
        /// Destination sockaddr allocation retained until the CQE.
        address: SocketAddress,
    },
    /// Send owned bytes once; short sends are returned and MSG_NOSIGNAL is added.
    Send {
        /// Connected descriptor retained through completion.
        socket: Rc<Fd>,
        /// Owned source bytes for an ordinary copying send, without zero-copy notifications.
        buffer: Buffer,
    },
    /// Receive once into an owned buffer; returns the actual byte count or EOF.
    Recv {
        /// Descriptor retained until the receive CQE, including while waiting for data.
        socket: Rc<Fd>,
        /// Initialized destination; this operation does not use a provided-buffer pool.
        buffer: Buffer,
    },
    /// Shut down selected socket directions without closing its descriptor.
    Shutdown {
        /// Descriptor retained through shutdown completion.
        socket: Rc<Fd>,
        /// Receive, send, or both directions to shut down.
        how: Shutdown,
    },
    /// Wait once for descriptor readiness; the CQE terminates this watch.
    PollAdd {
        /// Watched descriptor, retained until its watch CQE is reaped.
        file: Rc<Fd>,
        /// Linux poll event bits to watch; multishot flags are not exposed.
        events: u32,
    },
    /// Request cancellation of a pinned same-Io identity; success does not undo I/O.
    /// Targeting an unconsumed CLOSE is rejected with Unsupported.
    Cancel(CancelTarget),
}
impl Operation {
    /// Build an unpublished CLOSE retaining its unique Fd. Successful push
    /// transfers ownership to the kernel permanently, even if its CQE is an error.
    /// Fd establishes descriptor eligibility at construction; push does not
    /// perform descriptor validation syscalls.
    pub fn close(file: Fd) -> Self {
        Self::Close(Close { file: Some(file) })
    }
    /// Borrow this owned request's input/output buffer, if its opcode has one.
    pub fn buffer(&self) -> Option<&Buffer> {
        match self {
            Self::ReadAt { buffer, .. }
            | Self::WriteAt { buffer, .. }
            | Self::Send { buffer, .. }
            | Self::Recv { buffer, .. } => Some(buffer),
            _ => None,
        }
    }
    /// Recover the buffer by consuming the request; other inputs are dropped.
    /// Returns None for opcodes without an owned buffer.
    pub fn into_buffer(self) -> Option<Buffer> {
        match self {
            Self::ReadAt { buffer, .. }
            | Self::WriteAt { buffer, .. }
            | Self::Send { buffer, .. }
            | Self::Recv { buffer, .. } => Some(buffer),
            _ => None,
        }
    }
    pub(crate) fn is_cancel(&self) -> bool {
        matches!(self, Self::Cancel(_))
    }
}

/// A unique descriptor transferred to CLOSE only after successful SQ publication.
/// Fields are private so callers cannot manufacture or reuse a submitted fd.
#[derive(Debug)]
pub struct Close {
    /// Some before SQ publication; None once descriptor ownership is transferred.
    pub(crate) file: Option<Fd>,
}

/// Stable, owned socket-address storage for CONNECT.
#[derive(Debug)]
pub struct SocketAddress(
    /// Boxed ABI address whose storage remains stable when the wrapper moves.
    Address,
);
/// Concrete sockaddr layout, boxed to preserve its address through CONNECT.
#[derive(Debug)]
enum Address {
    /// IPv4 sockaddr with network-order port and address bytes.
    V4(Box<libc::sockaddr_in>),
    /// IPv6 sockaddr, including flow and interface scope information.
    V6(Box<libc::sockaddr_in6>),
}
impl From<SocketAddr> for SocketAddress {
    /// Encode the standard address into owned kernel-ABI storage.
    fn from(address: SocketAddr) -> Self {
        Self(match address {
            SocketAddr::V4(a) => Address::V4(Box::new(libc::sockaddr_in {
                sin_family: libc::AF_INET as _,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(a.ip().octets()),
                },
                sin_zero: [0; 8],
            })),
            SocketAddr::V6(a) => Address::V6(Box::new(libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as _,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: a.flowinfo().to_be(),
                sin6_scope_id: a.scope_id(),
                sin6_addr: libc::in6_addr {
                    s6_addr: a.ip().octets(),
                },
            })),
        })
    }
}
impl SocketAddress {
    /// Borrow its stable sockaddr pointer and ABI length; the operation must
    /// retain this allocation until CONNECT's terminal CQE.
    pub(crate) fn raw(&self) -> (*const libc::sockaddr, libc::socklen_t) {
        match &self.0 {
            Address::V4(a) => (
                (&**a as *const libc::sockaddr_in).cast(),
                size_of::<libc::sockaddr_in>() as _,
            ),
            Address::V6(a) => (
                (&**a as *const libc::sockaddr_in6).cast(),
                size_of::<libc::sockaddr_in6>() as _,
            ),
        }
    }
}

/// Opcode-specific ownership of a successful completion.
#[derive(Debug)]
pub enum Value {
    /// Number of bytes transferred by a read, write, send, or receive.
    Count(usize),
    /// Fresh descriptor returned by open, socket, or accept; closed on value drop.
    File(Fd),
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
/// Submitter's claim on an operation stored in an Io's slab. Not a Future.
/// Pass the allocating Io to access or explicitly release this claim. Dropping
/// the token alone does not release it; adapters must release on cancellation.
#[must_use = "consume the result or call release(&Io) to abandon it"]
#[derive(Debug)]
pub struct OperationHandle {
    /// Live submitter claim. None after completion consumption or explicit release;
    /// the token owns no Io and dropping it alone cannot change the slab.
    pub(crate) id: Option<OperationId>,
}
impl OperationHandle {
    /// Attach the submitter's claim to a freshly published Submitted record.
    pub(crate) fn new(id: OperationId) -> Self {
        Self { id: Some(id) }
    }

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
    /// Whether this token still owns a pin in the specified driver.
    pub(crate) fn is_active_for(&self, driver: DriverId) -> bool {
        self.io.is_some() && self.id.belongs_to(driver)
    }
    pub(crate) fn id(&self) -> OperationId {
        self.id
    }
    /// At the cancellation CQE, armed Some(Io) -> disarmed None, releasing the Io clone.
    /// Return the target ID for the driver to unpin after retiring the cancel record;
    /// this method itself does not unpin. Already disarmed returns None.
    pub(crate) fn disarm(&mut self) -> Option<OperationId> {
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
