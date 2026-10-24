//! Explicit owned requests; no caller supplies raw SQEs or kernel pointers.
#![forbid(unsafe_code)]

use crate::{Buffer, CancelTarget};
use std::{
    ffi::CString,
    fs::File,
    net::{Shutdown, SocketAddr},
    os::fd::RawFd,
    sync::Arc,
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
        file: Arc<File>,
        /// Initialized destination; its visible length is the requested byte count.
        buffer: Buffer,
        /// Absolute byte offset; current-file-position sentinel and signed overflow are rejected.
        offset: u64,
    },
    /// Write once at an explicit file offset; a short write is returned to the caller.
    WriteAt {
        /// Descriptor retained until the write CQE.
        file: Arc<File>,
        /// Owned source bytes; retained even if the submitter releases its claim.
        buffer: Buffer,
        /// Absolute byte offset; never advances the descriptor's shared file cursor.
        offset: u64,
    },
    /// Request a durability barrier; callers must order prerequisite writes themselves.
    Fsync {
        /// Descriptor whose file data/metadata is synchronized.
        file: Arc<File>,
        /// Use DATASYNC when true; otherwise request full file synchronization.
        data_only: bool,
    },
    /// Allocate or modify a file range using Linux fallocate semantics.
    Fallocate {
        /// Descriptor retained through completion.
        file: Arc<File>,
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
        file: Arc<File>,
        /// New byte length; values beyond a signed file offset are rejected.
        len: u64,
    },
    /// Open one path and return an owned descriptor; always requests CLOEXEC.
    OpenAt {
        /// Base for relative paths; None uses the process working directory.
        directory: Option<Arc<File>>,
        /// Owned NUL-terminated path, stable until the CQE.
        path: CString,
        /// Linux open flags; CLOEXEC is added by the driver.
        flags: i32,
        /// Creation permission bits, used when the open creates a file.
        mode: u32,
    },
    /// Close a uniquely owned descriptor; its ownership phases are tracked by Close.
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
        socket: Arc<File>,
    },
    /// Connect once using stable owned IPv4/IPv6 address storage.
    Connect {
        /// Descriptor being connected, retained through completion.
        socket: Arc<File>,
        /// Destination sockaddr allocation retained until the CQE.
        address: SocketAddress,
    },
    /// Send owned bytes once; short sends are returned and MSG_NOSIGNAL is added.
    Send {
        /// Connected descriptor retained through completion.
        socket: Arc<File>,
        /// Owned source bytes for an ordinary copying send, without zero-copy notifications.
        buffer: Buffer,
    },
    /// Receive once into an owned buffer; returns the actual byte count or EOF.
    Recv {
        /// Descriptor retained until the receive CQE, including while waiting for data.
        socket: Arc<File>,
        /// Initialized destination; this operation does not use a provided-buffer pool.
        buffer: Buffer,
    },
    /// Shut down selected socket directions without closing its descriptor.
    Shutdown {
        /// Descriptor retained through shutdown completion.
        socket: Arc<File>,
        /// Receive, send, or both directions to shut down.
        how: Shutdown,
    },
    /// Wait once for descriptor readiness; the CQE terminates this watch.
    PollAdd {
        /// Watched descriptor, retained until its watch CQE is reaped.
        file: Arc<File>,
        /// Linux poll event bits to watch; multishot flags are not exposed.
        events: u32,
    },
    /// Request cancellation of a pinned same-Io identity; success does not undo I/O.
    Cancel(CancelTarget),
}
impl Operation {
    /// Build an unpublished CLOSE retaining its unique File. Successful push
    /// transfers File -> raw fd; its terminal CQE clears that transferred state.
    pub fn close(file: File) -> Self {
        Self::Close(Close {
            file: Some(file),
            submitted: None,
        })
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
    pub(crate) file: Option<File>,
    /// Some after publication until its CQE; no File destructor owns this fd.
    /// A canceled CLOSE re-adopts/closes it; other CQEs leave closure to the kernel.
    pub(crate) submitted: Option<RawFd>,
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
