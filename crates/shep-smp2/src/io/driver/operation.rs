//! Sealed opcode preparation. Concrete requests and the enum convenience API
//! share builders without converting or moving owned inputs through an enum.
#![forbid(unsafe_code)]

use super::Shared;
use crate::io::{
    Buffer, Fd, Io,
    arena::{Arena, Kind, OperationId},
    operation::*,
};
use io_uring::{opcode, squeue::Entry, types};
use std::{
    ffi::CString,
    io,
    net::Shutdown as ShutdownHow,
    os::fd::{AsRawFd, IntoRawFd},
    rc::Rc,
};

pub(crate) struct Prepared {
    pub(super) entry: Entry,
    pub(super) kind: Kind,
    pub(super) retain_data: bool,
}

/// Only built-in stable owners implement this private supertrait of OperationData.
/// Preparation may fail; publication effects must be infallible and invoke no callbacks.
pub(crate) trait Submission {
    fn prepare(&mut self, shared: &Shared, arena: &mut Arena) -> io::Result<Prepared>;

    #[inline]
    fn published(&mut self, _arena: &mut Arena, _id: OperationId) -> Option<Io> {
        None
    }
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid owned operation arguments",
    )
}
fn check_offset(value: u64) -> io::Result<()> {
    if value > i64::MAX as u64 {
        Err(invalid())
    } else {
        Ok(())
    }
}
fn buffer_len(buffer: &Buffer) -> io::Result<u32> {
    if buffer.len() > i32::MAX as usize {
        Err(invalid())
    } else {
        Ok(buffer.len() as u32)
    }
}

// Each opcode has one builder. The enum borrows its variant fields; typed
// requests call the same builder directly, without constructing an Operation.
macro_rules! operations {
    ($($name:ident($($field:ident: $ty:ty),*) => ($kind:ident, $retain:literal) $body:block)*) => {
        $(
            impl $name {
                #[inline]
                fn entry($($field: $ty),*) -> io::Result<Entry> $body
            }
            impl Submission for $name {
                #[inline]
                fn prepare(&mut self, _: &Shared, _: &mut Arena) -> io::Result<Prepared> {
                    Ok(Prepared {
                        entry: Self::entry($(&mut self.$field),*)?,
                        kind: Kind::$kind,
                        retain_data: $retain,
                    })
                }
            }
        )*
        impl Submission for Operation {
            fn prepare(&mut self, shared: &Shared, arena: &mut Arena) -> io::Result<Prepared> {
                match self {
                    $(Self::$name { $($field),* } => Ok(Prepared {
                        entry: $name::entry($($field),*)?,
                        kind: Kind::$kind,
                        retain_data: $retain,
                    }),)*
                    Self::Nop => Ok(Prepared { entry: opcode::Nop::new().build(), kind: Kind::Done, retain_data: false }),
                    Self::Close(close) => close.prepare(shared, arena),
                    Self::Cancel(target) => target.prepare(shared, arena),
                }
            }
            #[inline]
    fn published(&mut self, arena: &mut Arena, id: OperationId) -> Option<Io> {
                match self {
                    Self::Close(close) => close.published(arena, id),
                    Self::Cancel(target) => target.published(arena, id),
                    _ => None,
                }
            }
        }
    };
}
operations! {
    ReadAt(file: &Rc<Fd>, buffer: &mut Buffer, offset: &u64) => (Count, true) {
        check_offset(*offset)?;
        let len = buffer_len(buffer)?;
        Ok(
            opcode::Read::new(types::Fd(file.as_raw_fd()), buffer.as_mut_ptr(), len)
                .offset(*offset)
                .build(),
        )
    }
    WriteAt(file: &Rc<Fd>, buffer: &Buffer, offset: &u64) => (Count, true) {
        check_offset(*offset)?;
        let len = buffer_len(buffer)?;
        // Override append per request, including changes after SQ publication.
        // Unsupported kernels must fail; never retry without RWF_NOAPPEND.
        Ok(
            opcode::Write::new(types::Fd(file.as_raw_fd()), buffer.as_ptr(), len)
                .offset(*offset)
                .rw_flags(libc::RWF_NOAPPEND)
                .build(),
        )
    }
    Fsync(file: &Rc<Fd>, data_only: &bool) => (Done, true) {
        Ok(opcode::Fsync::new(types::Fd(file.as_raw_fd()))
            .flags(if *data_only {
                types::FsyncFlags::DATASYNC
            } else {
                types::FsyncFlags::empty()
            })
            .build())
    }
    Fallocate(file: &Rc<Fd>, offset: &u64, len: &u64, mode: &i32) => (Done, true) {
        check_offset(*offset)?;
        check_offset(*len)?;
        check_offset(offset.checked_add(*len).ok_or_else(invalid)?)?;
        Ok(opcode::Fallocate::new(types::Fd(file.as_raw_fd()), *len)
            .offset(*offset)
            .mode(*mode)
            .build())
    }
    Ftruncate(file: &Rc<Fd>, len: &u64) => (Done, true) {
        check_offset(*len)?;
        Ok(opcode::Ftruncate::new(types::Fd(file.as_raw_fd()), *len).build())
    }
    OpenAt(directory: &Option<Rc<Fd>>, path: &CString, flags: &i32, mode: &u32) => (File, true) {
        Ok(opcode::OpenAt::new(
            types::Fd(directory.as_ref().map_or(libc::AT_FDCWD, |f| f.as_raw_fd())),
            path.as_ptr(),
        )
        .flags(*flags | libc::O_CLOEXEC)
        .mode(*mode)
        .build())
    }
    Socket(domain: &i32, socket_type: &i32, protocol: &i32) => (File, false) {
        Ok(opcode::Socket::new(*domain, *socket_type | libc::SOCK_CLOEXEC, *protocol).build())
    }
    Accept(socket: &Rc<Fd>) => (File, true) {
        Ok(opcode::Accept::new(
            types::Fd(socket.as_raw_fd()),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
        .flags(libc::SOCK_CLOEXEC)
        .build())
    }
    Connect(socket: &Rc<Fd>, address: &crate::io::SocketAddress) => (Done, true) {
        let (addr, len) = address.raw();
        Ok(opcode::Connect::new(types::Fd(socket.as_raw_fd()), addr, len).build())
    }
    Send(socket: &Rc<Fd>, buffer: &Buffer) => (Count, true) {
        let len = buffer_len(buffer)?;
        Ok(
            opcode::Send::new(types::Fd(socket.as_raw_fd()), buffer.as_ptr(), len)
                .flags(libc::MSG_NOSIGNAL)
                .build(),
        )
    }
    SendRange(socket: &Rc<Fd>, buffer: &Buffer, range: &std::ops::Range<usize>) => (Count, true) {
        buffer_len(buffer)?;
        let bytes = buffer.get(range.clone()).ok_or_else(invalid)?;
        Ok(opcode::Send::new(
            types::Fd(socket.as_raw_fd()),
            bytes.as_ptr(),
            bytes.len() as u32,
        )
        .flags(libc::MSG_NOSIGNAL)
        .build())
    }
    Recv(socket: &Rc<Fd>, buffer: &mut Buffer) => (Count, true) {
        let len = buffer_len(buffer)?;
        Ok(opcode::Recv::new(types::Fd(socket.as_raw_fd()), buffer.as_mut_ptr(), len).build())
    }
    Shutdown(socket: &Rc<Fd>, how: &ShutdownHow) => (Done, true) {
        let how = match how {
            ShutdownHow::Read => libc::SHUT_RD,
            ShutdownHow::Write => libc::SHUT_WR,
            ShutdownHow::Both => libc::SHUT_RDWR,
        };
        Ok(opcode::Shutdown::new(types::Fd(socket.as_raw_fd()), how).build())
    }
    PollAdd(file: &Rc<Fd>, events: &u32) => (Poll, true) {
        Ok(opcode::PollAdd::new(types::Fd(file.as_raw_fd()), *events).build())
    }
}

impl Submission for Close {
    fn prepare(&mut self, _: &Shared, _: &mut Arena) -> io::Result<Prepared> {
        let file = self.file.as_ref().ok_or_else(invalid)?;
        Ok(Prepared {
            // A socket's final release can wait for SO_LINGER. Keep that work
            // off the thread submitting the ring as well as Drop.
            entry: opcode::Close::new(types::Fd(file.as_raw_fd()))
                .build()
                .flags(io_uring::squeue::Flags::ASYNC),
            kind: Kind::Close,
            retain_data: false,
        })
    }
    #[inline]
    fn published(&mut self, _: &mut Arena, _: OperationId) -> Option<Io> {
        let _ = self.file.take().unwrap().into_raw_fd();
        None
    }
}
impl Submission for CancelTarget {
    fn prepare(&mut self, shared: &Shared, arena: &mut Arena) -> io::Result<Prepared> {
        if !self.is_active_for(shared.id) {
            return Err(invalid());
        }
        if arena.record(self.id()).kind == Kind::Close {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "accepted CLOSE operations cannot be canceled",
            ));
        }
        Ok(Prepared {
            entry: opcode::AsyncCancel::new(self.id().raw()).build(),
            kind: Kind::Cancel,
            retain_data: false,
        })
    }
    #[inline]
    fn published(&mut self, arena: &mut Arena, id: OperationId) -> Option<Io> {
        arena.record(id).dependency = Some(self.id());
        self.disarm()
    }
}
