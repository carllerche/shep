//! Explicit owned requests; no caller supplies raw SQEs or kernel pointers.
#![forbid(unsafe_code)]

use crate::io::{Buffer, Fd, Io, arena::OperationId, driver::DriverId};
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
    /// Send a checked range without copying or changing the owned buffer.
    SendRange {
        socket: Rc<Fd>,
        buffer: Buffer,
        /// Byte range within the buffer; invalid ranges are rejected before publication.
        range: std::ops::Range<usize>,
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
        // Keep the validated descriptor in a plain File so an unpublished or
        // rejected CLOSE falls back synchronously without reentering Fd::drop.
        Self::Close(Close {
            file: Some(file.into()),
        })
    }
    /// Borrow this owned request's input/output buffer, if its opcode has one.
    pub fn buffer(&self) -> Option<&Buffer> {
        match self {
            Self::ReadAt { buffer, .. }
            | Self::WriteAt { buffer, .. }
            | Self::Send { buffer, .. }
            | Self::SendRange { buffer, .. }
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
            | Self::SendRange { buffer, .. }
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
    pub(crate) file: Option<std::fs::File>,
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
/// Terminal result and concrete original inputs.
#[derive(Debug)]
pub struct Completion<T = Operation> {
    pub result: io::Result<Value>,
    pub operation: T,
}

/// An owned request. Drop abandons the result without requesting cancellation.
/// Concrete inputs stay here until consumed or transferred to the arena on
/// unfinished abandonment. Forgetting this owner leaks its inputs and Io clone.
#[must_use = "await the operation or drop it to abandon its result"]
pub struct Op<T: OperationData = Operation> {
    pub(crate) id: Option<OperationId>,
    pub(crate) io: Io,
    pub(crate) data: Option<T>,
}
impl<T: OperationData> std::fmt::Debug for Op<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Op")
            .field("id", &self.id)
            .field("data", &self.data)
            .finish()
    }
}
impl<T: OperationData> Op<T> {
    pub(crate) fn new(io: Io, id: OperationId, data: T) -> Self {
        Self {
            io,
            id: Some(id),
            data: Some(data),
        }
    }
    /// Consume a reaped result without driving the ring. The Io must be the origin.
    pub fn try_complete(&mut self, io: &Io) -> Option<Completion<T>> {
        let id = self.id.expect("operation already consumed");
        id.check(io.shared.id);
        let result = io.shared.state.borrow_mut().arena.take(id)?;
        self.id = None;
        Some(Completion {
            result,
            operation: self.data.take().unwrap(),
        })
    }
    /// Register a waiter; all custom waker callbacks occur outside state borrows.
    pub fn poll_complete(&mut self, io: &Io, cx: &mut Context<'_>) -> Poll<Completion<T>> {
        let waker = cx.waker().clone();
        if let Some(result) = self.try_complete(io) {
            return Poll::Ready(result);
        }
        assert!(
            !io.is_failed(),
            "io_uring driver failed with unresolved resources"
        );
        let old = io
            .shared
            .state
            .borrow_mut()
            .arena
            .wait(self.id.unwrap(), waker);
        drop(old);
        match self.try_complete(io) {
            Some(result) => Poll::Ready(result),
            None => Poll::Pending,
        }
    }
    /// Explicit abandonment; equivalent to Drop. Does not cancel the request.
    pub fn release(&mut self, io: &Io) {
        if let Some(id) = self.id {
            id.check(io.shared.id);
        }
        self.abandon();
    }
    fn abandon(&mut self) {
        if let Some(id) = self.id {
            let effects = self
                .io
                .shared
                .state
                .borrow_mut()
                .arena
                .abandon(id, &mut self.data);
            self.id = None;
            drop(effects);
            // Completed inputs, and inputs unnecessary after publication, need no box.
            drop(self.data.take());
        }
    }
    /// Pin the identity through cancellation retirement or unsubmitted token drop.
    pub fn cancel_target(&self, io: &Io) -> CancelTarget {
        let id = self.id.expect("operation already consumed");
        id.check(io.shared.id);
        io.shared.state.borrow_mut().arena.pin(id);
        CancelTarget {
            io: Some(io.clone()),
            id,
        }
    }
}
impl<T: OperationData> std::future::Future for Op<T> {
    type Output = Completion<T>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let io = this.io.clone();
        this.poll_complete(&io, cx)
    }
}
impl<T: OperationData> Drop for Op<T> {
    fn drop(&mut self) {
        self.abandon();
    }
}

// Conversion is sealed: only built-in stable owned inputs can cross publication.
// No user callback, allocation, or fallible work occurs in these conversions.
mod sealed {
    use super::Operation;
    pub trait Data {
        fn into_operation(self) -> Operation;
        fn from_operation(operation: Operation) -> Self;
    }
}
/// Supported concrete owned operation data. Implementations are sealed.
pub trait OperationData: sealed::Data + std::fmt::Debug + Unpin + 'static {}
impl OperationData for Operation {}
impl sealed::Data for Operation {
    fn into_operation(self) -> Operation {
        self
    }
    fn from_operation(operation: Operation) -> Self {
        operation
    }
}
macro_rules! file_operations {
    ($($name:ident { $($field:ident: $ty:ty),* $(,)? })*) => {$(
        #[derive(Debug)]
        pub struct $name { $(pub $field: $ty),* }
        impl OperationData for $name {}
        impl sealed::Data for $name {
            fn into_operation(self) -> Operation {
                Operation::$name { $($field: self.$field),* }
            }
            fn from_operation(operation: Operation) -> Self {
                let Operation::$name { $($field),* } = operation else { unreachable!("operation type changed") };
                Self { $($field),* }
            }
        }
    )*};
}
file_operations! {
    ReadAt { file: Rc<Fd>, buffer: Buffer, offset: u64 }
    WriteAt { file: Rc<Fd>, buffer: Buffer, offset: u64 }
    Fsync { file: Rc<Fd>, data_only: bool }
    Fallocate { file: Rc<Fd>, offset: u64, len: u64, mode: i32 }
    Ftruncate { file: Rc<Fd>, len: u64 }
}

/// A cancellation target is an owned identity reference, not a reusable integer.
pub struct CancelTarget {
    /// Some while the unsubmitted pin is armed; None after transfer at publication.
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
    /// Transfer the pin to a published cancellation record. Drop the returned Io
    /// outside the driver borrow; the cancellation CQE releases the pin.
    pub(crate) fn disarm(&mut self) -> Option<Io> {
        self.io.take()
    }
}
impl Drop for CancelTarget {
    /// Release an unsubmitted pin; a published token is already disarmed.
    fn drop(&mut self) {
        if let Some(io) = self.io.take() {
            io.shared.state.borrow_mut().arena.unpin(self.id);
        }
    }
}
