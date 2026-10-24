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
    Nop,
    ReadAt {
        file: Arc<File>,
        buffer: Buffer,
        offset: u64,
    },
    WriteAt {
        file: Arc<File>,
        buffer: Buffer,
        offset: u64,
    },
    Fsync {
        file: Arc<File>,
        data_only: bool,
    },
    Fallocate {
        file: Arc<File>,
        offset: u64,
        len: u64,
        mode: i32,
    },
    Ftruncate {
        file: Arc<File>,
        len: u64,
    },
    OpenAt {
        directory: Option<Arc<File>>,
        path: CString,
        flags: i32,
        mode: u32,
    },
    Close(Close),
    Socket {
        domain: i32,
        socket_type: i32,
        protocol: i32,
    },
    Accept {
        socket: Arc<File>,
    },
    Connect {
        socket: Arc<File>,
        address: SocketAddress,
    },
    Send {
        socket: Arc<File>,
        buffer: Buffer,
    },
    Recv {
        socket: Arc<File>,
        buffer: Buffer,
    },
    Shutdown {
        socket: Arc<File>,
        how: Shutdown,
    },
    PollAdd {
        file: Arc<File>,
        events: u32,
    },
    Cancel(CancelTarget),
}
impl Operation {
    pub fn close(file: File) -> Self {
        Self::Close(Close {
            file: Some(file),
            submitted: None,
        })
    }
    pub fn buffer(&self) -> Option<&Buffer> {
        match self {
            Self::ReadAt { buffer, .. }
            | Self::WriteAt { buffer, .. }
            | Self::Send { buffer, .. }
            | Self::Recv { buffer, .. } => Some(buffer),
            _ => None,
        }
    }
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
    pub(crate) file: Option<File>,
    pub(crate) submitted: Option<RawFd>,
}

/// Stable, owned socket-address storage for CONNECT.
#[derive(Debug)]
pub struct SocketAddress(Address);
#[derive(Debug)]
enum Address {
    V4(Box<libc::sockaddr_in>),
    V6(Box<libc::sockaddr_in6>),
}
impl From<SocketAddr> for SocketAddress {
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
