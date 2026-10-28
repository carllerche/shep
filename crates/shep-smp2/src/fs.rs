//! Core-local asynchronous filesystem operations over io2.
//!
//! Transfers own their buffers, expose short results, and reject saturation with
//! WouldBlock. Namespace and metadata helpers use a bounded worker, never the
//! core thread. Callers order writes, syncs and directory syncs explicitly.
//! File close waits for accepted work, including abandoned result waiters; it
//! does not flush appender buffers or imply durability. The runtime must keep
//! driving io2 while these futures wait.
#![forbid(unsafe_code)]

mod activity;
mod file;
mod file_io;
mod filesystem;
mod helper;
pub(crate) mod metadata;
mod open_options;
mod pending;

pub use crate::io::Buffer;
pub use file::File;
pub use file_io::FileIo;
pub use open_options::OpenOptions;
